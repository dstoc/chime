use crate::{
    devices::{PushSubscription, PushSubscriptionKeys, validate_subscription},
    push::{ProviderResult, PushTransport},
    store::{Store, StoredSubscription},
};
use anyhow::Result;
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chime_protocol::{DeliveryOutcome, DeliverySummary, NotificationPayload, NotificationRequest};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{Mutex, Semaphore},
    task::JoinSet,
    time::timeout,
};
use web_push::{
    ContentEncoding, SubscriptionInfo, Urgency, VapidSignatureBuilder, WebPushMessage,
    WebPushMessageBuilder,
};

const MAX_TITLE_BYTES: usize = 200;
const MAX_BODY_BYTES: usize = 1500;
const MAX_PUSH_PAYLOAD_BYTES: usize = 3000;
const SEND_TTL_SECONDS: u32 = 60 * 60;
const SEND_TIMEOUT: Duration = Duration::from_secs(90);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_IN_FLIGHT_SENDS: usize = 4;
const MAX_PROVIDER_REQUESTS: usize = 8;
const MAX_SENDS_PER_MINUTE: u16 = 60;
const MAX_ACKNOWLEDGEMENTS: usize = 4096;

#[derive(Clone)]
pub struct NotifyState(Arc<NotifyInner>);

struct NotifyInner {
    store: Store,
    transport: Arc<dyn PushTransport>,
    vapid_private_key: Vec<u8>,
    send_slots: Arc<Semaphore>,
    provider_slots: Arc<Semaphore>,
    send_budget: Mutex<SendBudget>,
    acknowledgements: Mutex<Acknowledgements>,
}

struct SendBudget {
    window_started: Instant,
    requests: u16,
}

#[derive(Default)]
struct Acknowledgements {
    entries: HashMap<String, AckEntry>,
    order: VecDeque<(String, Instant)>,
}

struct AckEntry {
    digest: [u8; 32],
    expires_at: Instant,
}

#[derive(Debug)]
pub struct NotifyError {
    pub status: StatusCode,
    pub message: &'static str,
}

impl NotifyState {
    pub fn new(store: Store, transport: Arc<dyn PushTransport>) -> Self {
        Self(Arc::new(NotifyInner {
            vapid_private_key: store.vapid().private_key_bytes().to_vec(),
            store,
            transport,
            send_slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT_SENDS)),
            provider_slots: Arc::new(Semaphore::new(MAX_PROVIDER_REQUESTS)),
            send_budget: Mutex::new(SendBudget {
                window_started: Instant::now(),
                requests: 0,
            }),
            acknowledgements: Mutex::new(Acknowledgements::default()),
        }))
    }

    pub async fn send(
        &self,
        secret: String,
        request: NotificationRequest,
    ) -> Result<DeliverySummary, NotifyError> {
        validate_request(&request)?;
        let subscriptions = self
            .0
            .store
            .authorize_send(secret)
            .await
            .map_err(|_| internal_error())?
            .ok_or_else(unauthorized)?;
        self.check_send_budget().await?;

        let id = random_token(16)?;
        let ack_capability = random_token(32)?;
        let expires_at = unix_now().map_err(|_| internal_error())? + i64::from(SEND_TTL_SECONDS);
        let payload = NotificationPayload {
            version: 1,
            id: id.clone(),
            title: request.title,
            body: request.body,
            expires_at,
            ack_capability: ack_capability.clone(),
        };
        let payload_bytes = serde_json::to_vec(&payload).map_err(|_| internal_error())?;
        if payload_bytes.len() > MAX_PUSH_PAYLOAD_BYTES {
            return Err(invalid_payload());
        }

        if subscriptions.is_empty() {
            return Ok(DeliverySummary {
                id,
                attempted: 0,
                accepted: 0,
                failed: 0,
                expired_subscriptions_removed: 0,
                outcome: DeliveryOutcome::NoDevices,
            });
        }

        let total_recipients = u32::try_from(subscriptions.len()).map_err(|_| internal_error())?;

        let _send_slot = self
            .0
            .send_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| capacity_error())?;

        let prepared = subscriptions
            .into_iter()
            .map(|subscription| {
                let message =
                    prepare_message(&subscription, &payload_bytes, &self.0.vapid_private_key)?;
                Ok((subscription, message))
            })
            .collect::<Result<Vec<_>, NotifyError>>()?;

        self.0
            .acknowledgements
            .lock()
            .await
            .insert(id.clone(), &ack_capability, SEND_TTL_SECONDS);

        let attempted = Arc::new(AtomicU32::new(0));
        let accepted = Arc::new(AtomicU32::new(0));
        let expired_removed = Arc::new(AtomicU32::new(0));
        let mut deliveries = JoinSet::new();
        for (subscription, message) in prepared {
            let store = self.0.store.clone();
            let transport = Arc::clone(&self.0.transport);
            let provider_slots = Arc::clone(&self.0.provider_slots);
            let attempted = Arc::clone(&attempted);
            let accepted = Arc::clone(&accepted);
            let expired_removed = Arc::clone(&expired_removed);
            let notification_id = id.clone();
            deliveries.spawn(async move {
                let Ok(_provider_slot) = provider_slots.acquire_owned().await else {
                    return;
                };
                attempted.fetch_add(1, Ordering::Relaxed);
                let outcome = timeout(PROVIDER_TIMEOUT, transport.send(message))
                    .await
                    .unwrap_or(ProviderResult::Failed);
                match outcome {
                    ProviderResult::Accepted => {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    }
                    ProviderResult::Expired => {
                        if store
                            .remove_expired_subscription(subscription.id, subscription.revision)
                            .await
                            .unwrap_or(false)
                        {
                            expired_removed.fetch_add(1, Ordering::Relaxed);
                        }
                        eprintln!(
                            "notification {notification_id}: push provider reported a permanently expired device endpoint"
                        );
                    }
                    ProviderResult::Failed => eprintln!(
                        "notification {notification_id}: push provider request failed"
                    ),
                }
            });
        }

        let join = timeout(SEND_TIMEOUT, async {
            while deliveries.join_next().await.is_some() {}
        })
        .await;
        if join.is_err() {
            deliveries.abort_all();
        }

        let attempted = attempted.load(Ordering::Relaxed);
        let accepted = accepted.load(Ordering::Relaxed);
        let failed = attempted.saturating_sub(accepted);
        let expired_subscriptions_removed = expired_removed.load(Ordering::Relaxed);
        let outcome = if attempted == total_recipients && accepted == total_recipients {
            DeliveryOutcome::Accepted
        } else if accepted > 0 {
            DeliveryOutcome::Partial
        } else {
            DeliveryOutcome::Failed
        };
        eprintln!(
            "notification {id}: attempted {attempted}, accepted {accepted}, failed {failed}, expired subscriptions removed {expired_subscriptions_removed}, outcome {outcome:?}"
        );
        Ok(DeliverySummary {
            id,
            attempted,
            accepted,
            failed,
            expired_subscriptions_removed,
            outcome,
        })
    }

    async fn check_send_budget(&self) -> Result<(), NotifyError> {
        let mut budget = self.0.send_budget.lock().await;
        let now = Instant::now();
        if now.duration_since(budget.window_started) >= Duration::from_secs(60) {
            budget.window_started = now;
            budget.requests = 0;
        }
        if budget.requests >= MAX_SENDS_PER_MINUTE {
            return Err(NotifyError {
                status: StatusCode::TOO_MANY_REQUESTS,
                message: "Chime has reached its send rate limit. Wait a minute and try again.",
            });
        }
        budget.requests += 1;
        Ok(())
    }
}

impl Acknowledgements {
    fn insert(&mut self, id: String, capability: &str, ttl: u32) {
        let now = Instant::now();
        while self
            .order
            .front()
            .is_some_and(|(_, expires_at)| *expires_at <= now)
        {
            if let Some((expired_id, _)) = self.order.pop_front() {
                self.entries.remove(&expired_id);
            }
        }
        while self.entries.len() >= MAX_ACKNOWLEDGEMENTS {
            if let Some((oldest_id, _)) = self.order.pop_front() {
                self.entries.remove(&oldest_id);
            } else {
                break;
            }
        }
        let expires_at = now + Duration::from_secs(u64::from(ttl));
        self.entries.insert(
            id.clone(),
            AckEntry {
                digest: Sha256::digest(capability.as_bytes()).into(),
                expires_at,
            },
        );
        self.order.push_back((id, expires_at));
    }

    #[allow(dead_code)] // The acknowledgement endpoint is implemented in the next milestone.
    fn is_valid(&mut self, id: &str, capability: &str) -> bool {
        let now = Instant::now();
        self.order.retain(|(entry_id, expires_at)| {
            let keep = *expires_at > now;
            if !keep {
                self.entries.remove(entry_id);
            }
            keep
        });
        let digest: [u8; 32] = Sha256::digest(capability.as_bytes()).into();
        self.entries
            .get(id)
            .is_some_and(|entry| entry.expires_at > now && bool::from(entry.digest.ct_eq(&digest)))
    }
}

fn validate_request(request: &NotificationRequest) -> Result<(), NotifyError> {
    let title = request.title.trim();
    if title.is_empty()
        || request.title.len() > MAX_TITLE_BYTES
        || request.title.chars().any(char::is_control)
    {
        return Err(invalid_payload());
    }
    if request.body.as_ref().is_some_and(|body| {
        body.len() > MAX_BODY_BYTES
            || body
                .chars()
                .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    }) {
        return Err(invalid_payload());
    }
    Ok(())
}

fn prepare_message(
    stored: &StoredSubscription,
    payload: &[u8],
    private_key: &[u8],
) -> Result<WebPushMessage, NotifyError> {
    let subscription = validate_subscription(PushSubscription {
        endpoint: stored.endpoint.clone(),
        _expiration_time: None,
        keys: PushSubscriptionKeys {
            p256dh: stored.p256dh.clone(),
            auth: stored.auth.clone(),
        },
    })
    .map_err(|_| invalid_payload())?;
    let info = SubscriptionInfo::new(
        subscription.endpoint,
        subscription.p256dh,
        subscription.auth,
    );
    let private_key = URL_SAFE_NO_PAD.encode(private_key);
    let mut signature = VapidSignatureBuilder::from_base64_no_sub(&private_key)
        .map_err(|_| internal_error())?
        .add_sub_info(&info);
    signature.add_claim("sub", "mailto:chime@localhost");
    let signature = signature.build().map_err(|_| internal_error())?;
    let mut builder = WebPushMessageBuilder::new(&info);
    builder.set_ttl(SEND_TTL_SECONDS);
    builder.set_urgency(Urgency::Normal);
    builder.set_payload(ContentEncoding::Aes128Gcm, payload);
    builder.set_vapid_signature(signature);
    builder.build().map_err(|_| invalid_payload())
}

fn random_token(size: usize) -> Result<String, NotifyError> {
    let mut bytes = vec![0_u8; size];
    getrandom::fill(&mut bytes).map_err(|_| internal_error())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn unix_now() -> Result<i64> {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    Ok(i64::try_from(seconds)?)
}

fn unauthorized() -> NotifyError {
    NotifyError {
        status: StatusCode::UNAUTHORIZED,
        message: "A valid Chime send key is required.",
    }
}

fn invalid_payload() -> NotifyError {
    NotifyError {
        status: StatusCode::BAD_REQUEST,
        message: "The notification is invalid or too large.",
    }
}

fn capacity_error() -> NotifyError {
    NotifyError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "Chime is at its send capacity. Try again later.",
    }
}

fn internal_error() -> NotifyError {
    NotifyError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "Chime could not complete this request.",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_BODY_BYTES, MAX_SENDS_PER_MINUTE, MAX_TITLE_BYTES, NotifyState, validate_request,
    };
    use crate::{
        push::{ProviderResult, PushTransport},
        store::Store,
    };
    use async_trait::async_trait;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use chime_protocol::NotificationRequest;
    use p256::{SecretKey, elliptic_curve::Generate};
    use sha2::{Digest, Sha256};
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };
    use tokio::sync::Mutex;
    use web_push::WebPushMessage;

    struct FakeTransport {
        outcome: ProviderResult,
        requests: AtomicU32,
        observed: Mutex<Vec<(String, u32, bool)>>,
    }

    #[async_trait]
    impl PushTransport for FakeTransport {
        async fn send(&self, message: WebPushMessage) -> ProviderResult {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.observed.lock().await.push((
                message.endpoint.to_string(),
                message.ttl,
                message.payload.is_some(),
            ));
            self.outcome
        }
    }

    async fn fixture(
        endpoint: &str,
        outcome: ProviderResult,
    ) -> (
        NotifyState,
        Store,
        Arc<FakeTransport>,
        String,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state")).unwrap();
        let secret = format!("chime_sk_{}", URL_SAFE_NO_PAD.encode([9_u8; 32]));
        let digest = Sha256::digest(secret.as_bytes()).to_vec();
        let endpoint = endpoint.to_owned();
        let key = SecretKey::try_generate().unwrap();
        let public_key = URL_SAFE_NO_PAD.encode(key.public_key().to_sec1_bytes());
        let auth = URL_SAFE_NO_PAD.encode([4_u8; 16]);
        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "INSERT INTO send_keys (id, name, secret_digest, created_at)
                     VALUES ('AAAAAAAAAAAAAAAAAAAAAA', 'test sender', ?1, unixepoch())",
                    rusqlite::params![digest],
                )?;
                if !endpoint.is_empty() {
                    connection.execute(
                        "INSERT INTO receiving_devices
                         (id, name, endpoint, p256dh, auth, token_digest,
                          created_at, updated_at, revision)
                         VALUES ('BBBBBBBBBBBBBBBBBBBBBB', 'test device', ?1, ?2, ?3,
                           zeroblob(32), unixepoch(), unixepoch(), 1)",
                        rusqlite::params![endpoint, public_key, auth],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let fake = Arc::new(FakeTransport {
            outcome,
            requests: AtomicU32::new(0),
            observed: Mutex::new(Vec::new()),
        });
        let state = NotifyState::new(store.clone(), fake.clone());
        (state, store, fake, secret, directory)
    }

    #[test]
    fn validates_notification_text_by_utf8_bytes_before_delivery() {
        assert!(
            validate_request(&NotificationRequest {
                title: "Hello".into(),
                body: Some("Build complete".into()),
            })
            .is_ok()
        );
        assert!(
            validate_request(&NotificationRequest {
                title: " ".into(),
                body: None,
            })
            .is_err()
        );
        assert!(
            validate_request(&NotificationRequest {
                title: "é".repeat(MAX_TITLE_BYTES / 2 + 1),
                body: None,
            })
            .is_err()
        );
        assert!(
            validate_request(&NotificationRequest {
                title: "Hello".into(),
                body: Some("é".repeat(MAX_BODY_BYTES / 2 + 1)),
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn sends_versioned_payload_with_one_hour_ttl_and_provider_summary() {
        let (state, _store, fake, secret, _directory) = fixture(
            "https://fcm.googleapis.com/fcm/send/test",
            ProviderResult::Accepted,
        )
        .await;
        let summary = state
            .send(
                secret,
                NotificationRequest {
                    title: "Build complete".into(),
                    body: Some("All checks passed".into()),
                },
            )
            .await
            .unwrap();

        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.accepted, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.outcome, chime_protocol::DeliveryOutcome::Accepted);
        assert_eq!(summary.id.len(), 22);
        assert_eq!(fake.requests.load(Ordering::Relaxed), 1);
        assert_eq!(
            fake.observed.lock().await.as_slice(),
            &[(
                "https://fcm.googleapis.com/fcm/send/test".into(),
                3600,
                true
            )]
        );
    }

    #[tokio::test]
    async fn no_devices_returns_a_distinct_success_summary() {
        let (state, _store, fake, secret, _directory) = fixture("", ProviderResult::Accepted).await;
        let summary = state
            .send(
                secret,
                NotificationRequest {
                    title: "Hello".into(),
                    body: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(summary.attempted, 0);
        assert_eq!(summary.outcome, chime_protocol::DeliveryOutcome::NoDevices);
        assert_eq!(fake.requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn send_rate_limit_applies_only_after_key_authorization() {
        let (state, _store, _fake, secret, _directory) =
            fixture("", ProviderResult::Accepted).await;
        state.0.send_budget.lock().await.requests = MAX_SENDS_PER_MINUTE;
        let request = NotificationRequest {
            title: "Hello".into(),
            body: None,
        };
        let unauthorized = state
            .send("invalid".into(), request.clone())
            .await
            .unwrap_err();
        assert_eq!(unauthorized.status, axum::http::StatusCode::UNAUTHORIZED);
        let limited = state.send(secret, request).await.unwrap_err();
        assert_eq!(limited.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn fake_transport_cannot_bypass_production_endpoint_validation() {
        let (state, _store, fake, secret, _directory) =
            fixture("https://127.0.0.1/private", ProviderResult::Accepted).await;
        let error = state
            .send(
                secret,
                NotificationRequest {
                    title: "Hello".into(),
                    body: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(fake.requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn permanent_provider_expiry_removes_only_the_matching_subscription_version() {
        let (state, store, _fake, secret, _directory) = fixture(
            "https://fcm.googleapis.com/fcm/send/test",
            ProviderResult::Expired,
        )
        .await;
        let summary = state
            .send(
                secret,
                NotificationRequest {
                    title: "Hello".into(),
                    body: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(summary.accepted, 0);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.expired_subscriptions_removed, 1);
        let count = store
            .database()
            .run(|connection| {
                connection.query_row("SELECT COUNT(*) FROM receiving_devices", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
