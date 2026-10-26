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
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinSet,
    time::timeout,
};
use url::Url;
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
const MAX_RECONCILE_IDS: usize = 100;
const MAX_STREAMS: usize = 128;
const MAX_STREAMS_PER_DEVICE: usize = 2;
const STREAM_BUFFER: usize = 16;

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
    streams: StdMutex<StreamManager>,
    next_stream_id: AtomicU64,
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
    acknowledged: bool,
}

#[derive(Default)]
struct StreamManager {
    streams: HashMap<u64, StreamEntry>,
}

struct StreamEntry {
    device_id: String,
    sender: mpsc::Sender<String>,
    close: watch::Sender<bool>,
}

pub struct EventStream {
    pub receiver: mpsc::Receiver<String>,
    pub closed: watch::Receiver<bool>,
    pub(crate) _lease: StreamLease,
}

pub(crate) struct StreamLease {
    id: u64,
    owner: Weak<NotifyInner>,
}

#[derive(Debug)]
pub struct NotifyError {
    pub status: StatusCode,
    pub message: &'static str,
}

pub struct AuthorizedSend {
    subscriptions: Vec<StoredSubscription>,
    _send_slot: OwnedSemaphorePermit,
}

pub enum SendPlan {
    NoDevices(DeliverySummary),
    Ready(PreparedSend),
}

pub struct PreparedSend {
    id: String,
    ack_capability: String,
    total_recipients: u32,
    messages: Vec<(StoredSubscription, WebPushMessage)>,
    _send_slot: OwnedSemaphorePermit,
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
            streams: StdMutex::new(StreamManager::default()),
            next_stream_id: AtomicU64::new(1),
        }))
    }

    pub async fn authorize(&self, secret: String) -> Result<AuthorizedSend, NotifyError> {
        let subscriptions = self
            .0
            .store
            .authorize_send(secret)
            .await
            .map_err(|_| internal_error())?
            .ok_or_else(unauthorized)?;
        self.check_send_budget().await?;
        let send_slot = self
            .0
            .send_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| capacity_error())?;
        Ok(AuthorizedSend {
            subscriptions,
            _send_slot: send_slot,
        })
    }

    pub fn prepare(
        &self,
        authorized: AuthorizedSend,
        request: NotificationRequest,
    ) -> Result<SendPlan, NotifyError> {
        validate_request(&request)?;
        let id = random_token(16)?;
        let ack_capability = random_token(32)?;
        let expires_at = unix_now().map_err(|_| internal_error())? + i64::from(SEND_TTL_SECONDS);
        let payload = NotificationPayload {
            version: 1,
            id: id.clone(),
            title: request.title,
            body: request.body,
            image_url: request.image_url,
            url: request.url,
            expires_at,
            ack_capability: ack_capability.clone(),
        };
        let payload_bytes = serde_json::to_vec(&payload).map_err(|_| internal_error())?;
        if payload_bytes.len() > MAX_PUSH_PAYLOAD_BYTES {
            return Err(invalid_payload());
        }

        if authorized.subscriptions.is_empty() {
            return Ok(SendPlan::NoDevices(DeliverySummary {
                id,
                attempted: 0,
                accepted: 0,
                failed: 0,
                expired_subscriptions_removed: 0,
                outcome: DeliveryOutcome::NoDevices,
            }));
        }

        let total_recipients =
            u32::try_from(authorized.subscriptions.len()).map_err(|_| internal_error())?;
        let messages = authorized
            .subscriptions
            .into_iter()
            .map(|subscription| {
                let message =
                    prepare_message(&subscription, &payload_bytes, &self.0.vapid_private_key)?;
                Ok((subscription, message))
            })
            .collect::<Result<Vec<_>, NotifyError>>()?;

        Ok(SendPlan::Ready(PreparedSend {
            id,
            ack_capability,
            total_recipients,
            messages,
            _send_slot: authorized._send_slot,
        }))
    }

    pub async fn dispatch(&self, plan: SendPlan) -> Result<DeliverySummary, NotifyError> {
        let prepared = match plan {
            SendPlan::NoDevices(summary) => return Ok(summary),
            SendPlan::Ready(prepared) => prepared,
        };
        let PreparedSend {
            id,
            ack_capability,
            total_recipients,
            messages,
            _send_slot,
        } = prepared;

        self.0
            .acknowledgements
            .lock()
            .await
            .insert(id.clone(), &ack_capability, SEND_TTL_SECONDS);

        let attempted = Arc::new(AtomicU32::new(0));
        let accepted = Arc::new(AtomicU32::new(0));
        let expired_removed = Arc::new(AtomicU32::new(0));
        let mut deliveries = JoinSet::new();
        for (subscription, message) in messages {
            let store = self.0.store.clone();
            let transport = Arc::clone(&self.0.transport);
            let provider_slots = Arc::clone(&self.0.provider_slots);
            let state = self.clone();
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
                        let _device_authority = state.device_authority().await;
                        let device_id = subscription.id.clone();
                        if store
                            .remove_expired_subscription(device_id.clone(), subscription.revision)
                            .await
                            .unwrap_or(false)
                        {
                            state.invalidate_device(&device_id);
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

    pub async fn acknowledge(
        &self,
        token: String,
        id: String,
        capability: String,
    ) -> Result<(), NotifyError> {
        let _device_authority = self.device_authority().await;
        self.authenticate_device(token).await?;
        if !valid_token_component(&id, 16) || !valid_token_component(&capability, 32) {
            return Err(unknown_acknowledgement());
        }
        let first_acknowledgement = self
            .0
            .acknowledgements
            .lock()
            .await
            .acknowledge(&id, &capability)
            .ok_or_else(unknown_acknowledgement)?;
        if first_acknowledgement {
            self.broadcast_acknowledgement(&id);
        }
        Ok(())
    }

    pub async fn reconcile(
        &self,
        token: String,
        ids: Vec<String>,
    ) -> Result<Vec<String>, NotifyError> {
        let _device_authority = self.device_authority().await;
        self.authenticate_device(token).await?;
        if ids.len() > MAX_RECONCILE_IDS || ids.iter().any(|id| !valid_token_component(id, 16)) {
            return Err(invalid_reconciliation());
        }
        Ok(self.0.acknowledgements.lock().await.acknowledged_ids(&ids))
    }

    pub async fn event_stream(&self, token: String) -> Result<EventStream, NotifyError> {
        let _device_authority = self.device_authority().await;
        let device_id = self.authenticate_device(token).await?;
        self.register_stream(device_id)
    }

    pub(crate) async fn device_authority(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.0.store.lock_device_authority().await
    }

    fn register_stream(&self, device_id: String) -> Result<EventStream, NotifyError> {
        let mut streams = self
            .0
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active_for_device = streams
            .streams
            .values()
            .filter(|stream| stream.device_id == device_id)
            .count();
        if streams.streams.len() >= MAX_STREAMS || active_for_device >= MAX_STREAMS_PER_DEVICE {
            return Err(stream_capacity_error());
        }
        let id = self.0.next_stream_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
        let (close_sender, closed) = watch::channel(false);
        streams.streams.insert(
            id,
            StreamEntry {
                device_id,
                sender,
                close: close_sender,
            },
        );
        Ok(EventStream {
            receiver,
            closed,
            _lease: StreamLease {
                id,
                owner: Arc::downgrade(&self.0),
            },
        })
    }

    #[cfg(test)]
    pub(crate) async fn remember_acknowledgement(&self, id: String, capability: &str) {
        self.0
            .acknowledgements
            .lock()
            .await
            .insert(id, capability, SEND_TTL_SECONDS);
    }

    pub fn invalidate_removed_devices(&self, active_device_ids: &[String]) {
        let active: std::collections::HashSet<&str> =
            active_device_ids.iter().map(String::as_str).collect();
        self.close_streams(|device_id| !active.contains(device_id));
    }

    pub fn invalidate_device(&self, device_id: &str) {
        self.close_streams(|active_id| active_id == device_id);
    }

    async fn authenticate_device(&self, token: String) -> Result<String, NotifyError> {
        self.0
            .store
            .authorize_device(token)
            .await
            .map_err(|_| internal_error())?
            .ok_or_else(device_unauthorized)
    }

    fn broadcast_acknowledgement(&self, id: &str) {
        let event = format!("event: acknowledged\ndata: {{\"id\":\"{id}\"}}\n\n");
        let mut streams = self
            .0
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        streams
            .streams
            .retain(|_, stream| match stream.sender.try_send(event.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    stream.close.send_replace(true);
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
    }

    fn close_streams(&self, mut should_close: impl FnMut(&str) -> bool) {
        let mut streams = self
            .0
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        streams.streams.retain(|_, stream| {
            if should_close(&stream.device_id) {
                stream.close.send_replace(true);
                false
            } else {
                true
            }
        });
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
                acknowledged: false,
            },
        );
        self.order.push_back((id, expires_at));
    }

    fn acknowledge(&mut self, id: &str, capability: &str) -> Option<bool> {
        self.prune_expired();
        let digest: [u8; 32] = Sha256::digest(capability.as_bytes()).into();
        let entry = self.entries.get_mut(id)?;
        if entry.expires_at <= Instant::now() || !bool::from(entry.digest.ct_eq(&digest)) {
            return None;
        }
        let first = !entry.acknowledged;
        entry.acknowledged = true;
        Some(first)
    }

    fn acknowledged_ids(&mut self, ids: &[String]) -> Vec<String> {
        self.prune_expired();
        ids.iter()
            .filter(|id| {
                self.entries
                    .get(id.as_str())
                    .is_some_and(|entry| entry.acknowledged)
            })
            .cloned()
            .collect()
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.order.retain(|(entry_id, expires_at)| {
            let keep = *expires_at > now;
            if !keep {
                self.entries.remove(entry_id);
            }
            keep
        });
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        owner
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .streams
            .remove(&self.id);
    }
}

fn valid_token_component(value: &str, size: usize) -> bool {
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(value) else {
        return false;
    };
    bytes.len() == size && URL_SAFE_NO_PAD.encode(bytes) == value
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
    for value in [request.image_url.as_deref(), request.url.as_deref()]
        .into_iter()
        .flatten()
    {
        if value.len() > 2048 {
            return Err(invalid_payload());
        }
        let Ok(url) = Url::parse(value) else {
            return Err(invalid_payload());
        };
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid_payload());
        }
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

fn device_unauthorized() -> NotifyError {
    NotifyError {
        status: StatusCode::UNAUTHORIZED,
        message: "A valid Chime device token is required.",
    }
}

fn unknown_acknowledgement() -> NotifyError {
    NotifyError {
        status: StatusCode::NOT_FOUND,
        message: "That notification acknowledgement is no longer available.",
    }
}

fn invalid_reconciliation() -> NotifyError {
    NotifyError {
        status: StatusCode::BAD_REQUEST,
        message: "The acknowledgement reconciliation request is invalid or too large.",
    }
}

fn stream_capacity_error() -> NotifyError {
    NotifyError {
        status: StatusCode::TOO_MANY_REQUESTS,
        message: "Chime has reached its event stream limit for this device or server.",
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
        Acknowledgements, MAX_ACKNOWLEDGEMENTS, MAX_BODY_BYTES, MAX_SENDS_PER_MINUTE, MAX_STREAMS,
        MAX_STREAMS_PER_DEVICE, MAX_TITLE_BYTES, NotifyState, STREAM_BUFFER, validate_request,
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
    use std::time::Duration;
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

    async fn send(
        state: &NotifyState,
        secret: String,
        request: NotificationRequest,
    ) -> Result<chime_protocol::DeliverySummary, super::NotifyError> {
        let authorized = state.authorize(secret).await?;
        let plan = state.prepare(authorized, request)?;
        state.dispatch(plan).await
    }

    async fn insert_device_token(store: &Store, device_id: &str, token: &str) {
        let digest = Sha256::digest(token.as_bytes()).to_vec();
        let id = device_id.to_owned();
        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "INSERT INTO receiving_devices
                     (id, name, endpoint, p256dh, auth, token_digest,
                      created_at, updated_at, revision)
                     VALUES (?1, 'test device', ?2, '', '', ?3, unixepoch(), unixepoch(), 1)",
                    rusqlite::params![id, format!("https://fcm.googleapis.com/test/{id}"), digest],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    fn test_id(value: u128) -> String {
        URL_SAFE_NO_PAD.encode(value.to_be_bytes())
    }

    fn test_capability(value: u8) -> String {
        URL_SAFE_NO_PAD.encode([value; 32])
    }

    #[test]
    fn validates_notification_text_by_utf8_bytes_before_delivery() {
        assert!(
            validate_request(&NotificationRequest {
                title: "Hello".into(),
                body: Some("Build complete".into()),
                image_url: None,
                url: None,
            })
            .is_ok()
        );
        assert!(
            validate_request(&NotificationRequest {
                title: " ".into(),
                body: None,
                image_url: None,
                url: None,
            })
            .is_err()
        );
        for url in [
            "http://example.com/image.png".to_owned(),
            "https://user@example.com/image.png".to_owned(),
            format!("https://example.com/{}", "x".repeat(2040)),
        ] {
            assert!(
                validate_request(&NotificationRequest {
                    title: "Hello".into(),
                    body: None,
                    image_url: Some(url),
                    url: None,
                })
                .is_err()
            );
        }
        assert!(
            validate_request(&NotificationRequest {
                title: "Hello".into(),
                body: None,
                image_url: Some("https://images.example/build.png".into()),
                url: Some("https://example.com/build".into()),
            })
            .is_ok()
        );
        assert!(
            validate_request(&NotificationRequest {
                title: "é".repeat(MAX_TITLE_BYTES / 2 + 1),
                body: None,
                image_url: None,
                url: None,
            })
            .is_err()
        );
        assert!(
            validate_request(&NotificationRequest {
                title: "Hello".into(),
                body: Some("é".repeat(MAX_BODY_BYTES / 2 + 1)),
                image_url: None,
                url: None,
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
        let summary = send(
            &state,
            secret,
            NotificationRequest {
                title: "Build complete".into(),
                body: Some("All checks passed".into()),
                image_url: None,
                url: None,
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
        let summary = send(
            &state,
            secret,
            NotificationRequest {
                title: "Hello".into(),
                body: None,
                image_url: None,
                url: None,
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
            image_url: None,
            url: None,
        };
        let unauthorized = send(&state, "invalid".into(), request.clone())
            .await
            .unwrap_err();
        assert_eq!(unauthorized.status, axum::http::StatusCode::UNAUTHORIZED);
        let limited = send(&state, secret, request).await.unwrap_err();
        assert_eq!(limited.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn fake_transport_cannot_bypass_production_endpoint_validation() {
        let (state, _store, fake, secret, _directory) =
            fixture("https://127.0.0.1/private", ProviderResult::Accepted).await;
        let error = send(
            &state,
            secret,
            NotificationRequest {
                title: "Hello".into(),
                body: None,
                image_url: None,
                url: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(fake.requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn oversized_payload_is_rejected_before_any_push_request() {
        let (state, _store, fake, secret, _directory) = fixture(
            "https://fcm.googleapis.com/fcm/send/test",
            ProviderResult::Accepted,
        )
        .await;
        let error = send(
            &state,
            secret,
            NotificationRequest {
                title: "Hello".into(),
                body: None,
                image_url: Some(format!("https://images.example/{}", "a".repeat(1800))),
                url: Some(format!("https://example.com/{}", "b".repeat(1000))),
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
        let summary = send(
            &state,
            secret,
            NotificationRequest {
                title: "Hello".into(),
                body: None,
                image_url: None,
                url: None,
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

    #[tokio::test]
    async fn acknowledgements_require_an_active_device_and_matching_capability() {
        let (state, store, _fake, _secret, _directory) =
            fixture("", ProviderResult::Accepted).await;
        let device_id = "BBBBBBBBBBBBBBBBBBBBBB";
        let token = format!("chime_dt_{}", URL_SAFE_NO_PAD.encode([8_u8; 32]));
        let id = test_id(1);
        let capability = test_capability(9);
        insert_device_token(&store, device_id, &token).await;
        state
            .remember_acknowledgement(id.clone(), &capability)
            .await;

        assert_eq!(
            state
                .acknowledge("chime_dt_wrong".into(), id.clone(), capability.clone())
                .await
                .unwrap_err()
                .status,
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            state
                .acknowledge(token.clone(), id.clone(), test_capability(10))
                .await
                .unwrap_err()
                .status,
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            state
                .acknowledge(token.clone(), test_id(2), capability.clone())
                .await
                .unwrap_err()
                .status,
            axum::http::StatusCode::NOT_FOUND
        );
        assert!(
            state
                .reconcile(token.clone(), vec![id.clone()])
                .await
                .unwrap()
                .is_empty()
        );

        state
            .acknowledge(token.clone(), id.clone(), capability.clone())
            .await
            .unwrap();
        state
            .acknowledge(token.clone(), id.clone(), capability)
            .await
            .unwrap();
        assert_eq!(
            state
                .reconcile(token.clone(), vec![id.clone()])
                .await
                .unwrap(),
            [id]
        );

        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "DELETE FROM receiving_devices WHERE id = ?1",
                    rusqlite::params![device_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            state
                .reconcile(token.clone(), vec![test_id(1)])
                .await
                .unwrap_err()
                .status,
            axum::http::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            state.event_stream(token).await.err().unwrap().status,
            axum::http::StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn acknowledgement_state_expires_evicts_and_is_not_persistent() {
        let mut acknowledgements = Acknowledgements::default();
        acknowledgements.insert(test_id(0), &test_capability(1), 0);
        assert!(
            acknowledgements
                .acknowledge(&test_id(0), &test_capability(1))
                .is_none()
        );

        acknowledgements.insert(test_id(1), &test_capability(1), 3600);
        assert_eq!(
            acknowledgements.acknowledge(&test_id(1), &test_capability(1)),
            Some(true)
        );
        for value in 2..=(MAX_ACKNOWLEDGEMENTS as u128 + 1) {
            acknowledgements.insert(test_id(value), &test_capability(1), 3600);
        }
        assert_eq!(acknowledgements.entries.len(), MAX_ACKNOWLEDGEMENTS);
        assert!(
            acknowledgements
                .acknowledge(&test_id(1), &test_capability(1))
                .is_none()
        );
        assert_eq!(
            acknowledgements.acknowledge(
                &test_id(MAX_ACKNOWLEDGEMENTS as u128 + 1),
                &test_capability(1)
            ),
            Some(true)
        );

        let restarted = Acknowledgements::default();
        assert!(restarted.entries.is_empty());
    }

    #[tokio::test]
    async fn initializing_notify_state_loses_transient_acknowledgements() {
        let (state, store, fake, _secret, _directory) = fixture("", ProviderResult::Accepted).await;
        let id = test_id(7);
        state
            .remember_acknowledgement(id.clone(), &test_capability(8))
            .await;
        let restarted = NotifyState::new(store, fake);
        assert!(
            restarted
                .0
                .acknowledgements
                .lock()
                .await
                .acknowledged_ids(&[id])
                .is_empty()
        );
    }

    #[tokio::test]
    async fn event_stream_limits_cancellation_removal_and_slow_clients_are_bounded() {
        let (state, _store, _fake, _secret, _directory) =
            fixture("", ProviderResult::Accepted).await;
        let first = state.register_stream("device-a".into()).unwrap();
        let mut second = state.register_stream("device-a".into()).unwrap();
        assert_eq!(
            state
                .register_stream("device-a".into())
                .err()
                .unwrap()
                .status,
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        drop(first);
        let replacement = state.register_stream("device-a".into()).unwrap();

        let other = state.register_stream("device-b".into()).unwrap();
        state.invalidate_removed_devices(&["device-b".to_owned()]);
        assert!(*second.closed.borrow());
        assert!(!*other.closed.borrow());
        assert!(second.closed.changed().await.is_err() || *second.closed.borrow());
        drop(replacement);
        drop(other);

        let mut streams = Vec::new();
        for device in 0..(MAX_STREAMS / MAX_STREAMS_PER_DEVICE) {
            streams.push(state.register_stream(format!("device-{device}")).unwrap());
            streams.push(state.register_stream(format!("device-{device}")).unwrap());
        }
        assert_eq!(streams.len(), MAX_STREAMS);
        assert_eq!(
            state
                .register_stream("one-more".into())
                .err()
                .unwrap()
                .status,
            axum::http::StatusCode::TOO_MANY_REQUESTS
        );
        drop(streams);
        assert!(state.register_stream("after-cancel".into()).is_ok());

        let mut slow = state.register_stream("slow-client".into()).unwrap();
        for value in 0..=STREAM_BUFFER {
            state.broadcast_acknowledgement(&test_id(value as u128));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), slow.closed.changed())
                .await
                .is_ok()
        );
        assert!(*slow.closed.borrow());
        state.invalidate_device("slow-client");
    }
}
