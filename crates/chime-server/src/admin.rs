use crate::{
    config::Origin,
    store::{
        AdminCompletion, AdminRejection, AuthenticatedOperation, PasskeyRegistrationResult, Store,
    },
};
use anyhow::Result;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use url::Url;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, RequestChallengeResponse, Uuid, Webauthn,
    WebauthnBuilder,
};

const CEREMONY_LIFETIME: Duration = Duration::from_secs(120);
const MAX_PENDING_CEREMONIES: usize = 128;
const ADMIN_USER_ID: u128 = 0x4348_494d_4500_4000_8000_0000_0001;
const ADMIN_USER_NAME: &str = "chime-admin";
const ADMIN_DISPLAY_NAME: &str = "Chime administrator";

#[derive(Clone)]
pub struct AdminState(Arc<AdminInner>);

struct AdminInner {
    store: Store,
    webauthn: Webauthn,
    origin: String,
    pending: PendingCeremonies,
    challenge_budget: Mutex<ChallengeBudget>,
}

struct ChallengeBudget {
    window_started: Instant,
    requests: u16,
}

struct PendingCeremonies {
    slots: Arc<Semaphore>,
    entries: Mutex<HashMap<String, PendingRecord>>,
}

struct PendingRecord {
    expires_at: Instant,
    _slot: OwnedSemaphorePermit,
    kind: PendingKind,
}

enum PendingKind {
    SetupRegistration {
        state: PasskeyRegistration,
        name: String,
    },
    AdminAuthentication {
        state: PasskeyAuthentication,
        operation: AdminOperation,
    },
    AddRegistration {
        state: PasskeyRegistration,
        name: String,
        authorizer_id: Vec<u8>,
    },
}

#[derive(Clone)]
enum AdminOperation {
    List,
    Add { name: String },
    Remove { credential_id: Vec<u8> },
}

#[derive(Debug)]
pub struct AdminError {
    pub status: axum::http::StatusCode,
    pub message: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminRequest {
    ListPasskeys,
    AddPasskey { name: String },
    RemovePasskey { id: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupStartRequest {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupFinishRequest {
    pub request_id: String,
    pub credential: RegisterPublicKeyCredential,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminFinishRequest {
    pub request_id: String,
    pub credential: PublicKeyCredential,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddFinishRequest {
    pub request_id: String,
    pub credential: RegisterPublicKeyCredential,
}

#[derive(Debug, Serialize)]
pub struct SetupStatusResponse {
    pub setup_required: bool,
    pub setup_available: bool,
}

#[derive(Debug, Serialize)]
pub struct RegistrationChallenge {
    pub request_id: String,
    pub options: CreationChallengeResponse,
}

#[derive(Debug, Serialize)]
pub struct AuthenticationChallenge {
    pub request_id: String,
    pub options: RequestChallengeResponse,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AdminResponse {
    Passkeys {
        passkeys: Vec<PasskeySummary>,
    },
    AddRegistration {
        request_id: String,
        options: Box<CreationChallengeResponse>,
    },
}

#[derive(Debug, Serialize)]
pub struct PasskeySummary {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

impl AdminState {
    pub fn new(store: Store, origin: &Origin) -> Result<Self> {
        let origin_url = Url::parse(origin.as_str())?;
        let webauthn = WebauthnBuilder::new(origin.rp_id(), &origin_url)?
            .allow_subdomains(false)
            .allow_any_port(false)
            .rp_name("Chime")
            .build()?;
        Ok(Self(Arc::new(AdminInner {
            store,
            webauthn,
            origin: origin.as_str().to_owned(),
            pending: PendingCeremonies {
                slots: Arc::new(Semaphore::new(MAX_PENDING_CEREMONIES)),
                entries: Mutex::new(HashMap::new()),
            },
            challenge_budget: Mutex::new(ChallengeBudget {
                window_started: Instant::now(),
                requests: 0,
            }),
        })))
    }

    pub fn expected_origin(&self) -> &str {
        &self.0.origin
    }

    pub async fn setup_status(&self) -> Result<SetupStatusResponse> {
        let status = self.0.store.setup_status().await?;
        Ok(SetupStatusResponse {
            setup_required: !status.initialized,
            setup_available: !status.initialized && status.available,
        })
    }

    pub async fn start_setup(
        &self,
        request: SetupStartRequest,
    ) -> Result<RegistrationChallenge, AdminError> {
        self.check_challenge_budget().await?;
        let name = validate_name(request.name)?;
        let status = self.0.store.setup_status().await.map_err(internal_error)?;
        if status.initialized || !status.available {
            return Err(setup_unavailable());
        }

        let slot = self.reserve_slot().await?;
        let (options, state) = self
            .0
            .webauthn
            .start_passkey_registration(
                Uuid::from_u128(ADMIN_USER_ID),
                ADMIN_USER_NAME,
                ADMIN_DISPLAY_NAME,
                None,
            )
            .map_err(|_| internal_error(()))?;
        let remaining = self
            .0
            .store
            .consume_setup_code(request.code)
            .await
            .map_err(internal_error)?;
        let Some(remaining_seconds) = remaining else {
            return Err(AdminError {
                status: axum::http::StatusCode::FORBIDDEN,
                message: "The setup code is invalid, expired, or already used.",
            });
        };

        let request_id = new_request_id().map_err(|_| internal_error(()))?;
        self.insert_pending(
            request_id.clone(),
            slot,
            Duration::from_secs(remaining_seconds).min(CEREMONY_LIFETIME),
            PendingKind::SetupRegistration { state, name },
        )
        .await;
        Ok(RegistrationChallenge {
            request_id,
            options,
        })
    }

    pub async fn finish_setup(&self, request: SetupFinishRequest) -> Result<(), AdminError> {
        let pending = self.take_pending(&request.request_id).await?;
        let PendingKind::SetupRegistration { state, name } = pending else {
            return Err(expired_error());
        };
        let passkey = self
            .0
            .webauthn
            .finish_passkey_registration(&request.credential, &state)
            .map_err(|_| invalid_registration())?;
        self.0
            .store
            .register_first_passkey(passkey, name)
            .await
            .map_err(|_| setup_unavailable())
    }

    pub async fn start_admin_operation(
        &self,
        request: AdminRequest,
    ) -> Result<AuthenticationChallenge, AdminError> {
        let operation = match request {
            AdminRequest::ListPasskeys => AdminOperation::List,
            AdminRequest::AddPasskey { name } => AdminOperation::Add {
                name: validate_name(name)?,
            },
            AdminRequest::RemovePasskey { id } => {
                let bytes = URL_SAFE_NO_PAD
                    .decode(id)
                    .map_err(|_| invalid_operation())?;
                if bytes.is_empty() || bytes.len() > 1024 {
                    return Err(invalid_operation());
                }
                AdminOperation::Remove {
                    credential_id: bytes,
                }
            }
        };
        self.check_challenge_budget().await?;
        let passkeys = self.0.store.passkeys().await.map_err(internal_error)?;
        if passkeys.is_empty() {
            return Err(setup_unavailable());
        }
        let credentials: Vec<Passkey> = passkeys.into_iter().map(|entry| entry.passkey).collect();
        let (options, state) = self
            .0
            .webauthn
            .start_passkey_authentication(&credentials)
            .map_err(|_| internal_error(()))?;
        let slot = self.reserve_slot().await?;
        let request_id = new_request_id().map_err(|_| internal_error(()))?;
        self.insert_pending(
            request_id.clone(),
            slot,
            CEREMONY_LIFETIME,
            PendingKind::AdminAuthentication { state, operation },
        )
        .await;
        Ok(AuthenticationChallenge {
            request_id,
            options,
        })
    }

    pub async fn finish_admin_operation(
        &self,
        request: AdminFinishRequest,
    ) -> Result<AdminResponse, AdminError> {
        let pending = self.take_pending(&request.request_id).await?;
        let PendingKind::AdminAuthentication { state, operation } = pending else {
            return Err(expired_error());
        };
        let authentication = self
            .0
            .webauthn
            .finish_passkey_authentication(&request.credential, &state)
            .map_err(|_| invalid_assertion())?;
        let operation = match operation {
            AdminOperation::List => AuthenticatedOperation::List,
            AdminOperation::Add { name } => AuthenticatedOperation::Add(name),
            AdminOperation::Remove { credential_id } => {
                AuthenticatedOperation::Remove(credential_id)
            }
        };
        let completion = self
            .0
            .store
            .complete_admin_operation(authentication, operation)
            .await
            .map_err(|_| invalid_assertion())?;
        match completion {
            AdminCompletion::Listed(passkeys) => Ok(AdminResponse::Passkeys {
                passkeys: summaries(passkeys),
            }),
            AdminCompletion::Rejected(rejection) => Err(rejection_error(rejection)),
            AdminCompletion::AddAuthorized {
                name,
                authorizer_id,
                exclude_credentials,
            } => {
                let (options, state) = self
                    .0
                    .webauthn
                    .start_passkey_registration(
                        Uuid::from_u128(ADMIN_USER_ID),
                        ADMIN_USER_NAME,
                        ADMIN_DISPLAY_NAME,
                        Some(exclude_credentials),
                    )
                    .map_err(|_| internal_error(()))?;
                let slot = self.reserve_slot().await?;
                let request_id = new_request_id().map_err(|_| internal_error(()))?;
                self.insert_pending(
                    request_id.clone(),
                    slot,
                    CEREMONY_LIFETIME,
                    PendingKind::AddRegistration {
                        state,
                        name,
                        authorizer_id,
                    },
                )
                .await;
                Ok(AdminResponse::AddRegistration {
                    request_id,
                    options: Box::new(options),
                })
            }
        }
    }

    pub async fn finish_add_passkey(
        &self,
        request: AddFinishRequest,
    ) -> Result<Vec<PasskeySummary>, AdminError> {
        let pending = self.take_pending(&request.request_id).await?;
        let PendingKind::AddRegistration {
            state,
            name,
            authorizer_id,
        } = pending
        else {
            return Err(expired_error());
        };
        let passkey = self
            .0
            .webauthn
            .finish_passkey_registration(&request.credential, &state)
            .map_err(|_| invalid_registration())?;
        let outcome = self
            .0
            .store
            .register_authorized_passkey(passkey, name, authorizer_id)
            .await
            .map_err(internal_error)?;
        match outcome {
            PasskeyRegistrationResult::Added => {}
            PasskeyRegistrationResult::Duplicate => {
                return Err(AdminError {
                    status: axum::http::StatusCode::CONFLICT,
                    message: "That passkey is already registered.",
                });
            }
            PasskeyRegistrationResult::AuthorizerRemoved => return Err(invalid_assertion()),
            PasskeyRegistrationResult::Limit => {
                return Err(AdminError {
                    status: axum::http::StatusCode::CONFLICT,
                    message: "Chime can store at most 16 administrator passkeys.",
                });
            }
        }
        let passkeys = self.0.store.passkeys().await.map_err(internal_error)?;
        Ok(summaries(passkeys))
    }

    async fn reserve_slot(&self) -> Result<OwnedSemaphorePermit, AdminError> {
        let mut entries = self.0.pending.entries.lock().await;
        entries.retain(|_, entry| entry.expires_at > Instant::now());
        drop(entries);
        self.0
            .pending
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AdminError {
                status: axum::http::StatusCode::TOO_MANY_REQUESTS,
                message: "Too many passkey prompts are active. Wait and try again.",
            })
    }

    async fn check_challenge_budget(&self) -> Result<(), AdminError> {
        let mut budget = self.0.challenge_budget.lock().await;
        let now = Instant::now();
        if now.duration_since(budget.window_started) >= Duration::from_secs(60) {
            budget.window_started = now;
            budget.requests = 0;
        }
        if budget.requests >= 120 {
            return Err(AdminError {
                status: axum::http::StatusCode::TOO_MANY_REQUESTS,
                message: "Too many passkey prompts. Wait a minute and try again.",
            });
        }
        budget.requests += 1;
        Ok(())
    }

    async fn insert_pending(
        &self,
        request_id: String,
        slot: OwnedSemaphorePermit,
        lifetime: Duration,
        kind: PendingKind,
    ) {
        let mut entries = self.0.pending.entries.lock().await;
        entries.retain(|_, entry| entry.expires_at > Instant::now());
        entries.insert(
            request_id,
            PendingRecord {
                expires_at: Instant::now() + lifetime,
                _slot: slot,
                kind,
            },
        );
    }

    async fn take_pending(&self, request_id: &str) -> Result<PendingKind, AdminError> {
        let mut entries = self.0.pending.entries.lock().await;
        entries.retain(|_, entry| entry.expires_at > Instant::now());
        entries
            .remove(request_id)
            .map(|record| record.kind)
            .ok_or_else(expired_error)
    }
}

fn summaries(passkeys: Vec<crate::store::AdminPasskey>) -> Vec<PasskeySummary> {
    passkeys
        .into_iter()
        .map(|passkey| PasskeySummary {
            id: passkey.id,
            name: passkey.name,
            created_at: passkey.created_at,
        })
        .collect()
}

fn validate_name(name: String) -> Result<String, AdminError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return Err(invalid_operation());
    }
    Ok(name.to_owned())
}

fn new_request_id() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn expired_error() -> AdminError {
    AdminError {
        status: axum::http::StatusCode::CONFLICT,
        message: "This passkey prompt expired or was already used. Start a new prompt.",
    }
}

fn setup_unavailable() -> AdminError {
    AdminError {
        status: axum::http::StatusCode::CONFLICT,
        message: "Setup is unavailable. An operator can replace the code with chime-server reset-admin while Chime is stopped.",
    }
}

fn invalid_operation() -> AdminError {
    AdminError {
        status: axum::http::StatusCode::BAD_REQUEST,
        message: "The passkey operation is invalid.",
    }
}

fn invalid_registration() -> AdminError {
    AdminError {
        status: axum::http::StatusCode::BAD_REQUEST,
        message: "Passkey registration failed or was cancelled. Start a new prompt.",
    }
}

fn invalid_assertion() -> AdminError {
    AdminError {
        status: axum::http::StatusCode::FORBIDDEN,
        message: "Passkey authorization failed or was revoked. Start a new prompt.",
    }
}

fn rejection_error(rejection: AdminRejection) -> AdminError {
    match rejection {
        AdminRejection::LastPasskey => AdminError {
            status: axum::http::StatusCode::CONFLICT,
            message: "Keep at least one administrator passkey registered.",
        },
        AdminRejection::PasskeyNotFound => AdminError {
            status: axum::http::StatusCode::NOT_FOUND,
            message: "That administrator passkey is no longer present.",
        },
        AdminRejection::PasskeyLimit => AdminError {
            status: axum::http::StatusCode::CONFLICT,
            message: "Chime can store at most 16 administrator passkeys.",
        },
    }
}

fn internal_error<E>(_error: E) -> AdminError {
    AdminError {
        status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        message: "Chime could not complete this request.",
    }
}

#[cfg(test)]
mod tests {
    use super::{AdminState, SetupStartRequest};
    use crate::{config::Origin, store::Store};
    use anyhow::{Context, Result};
    use std::str::FromStr;

    #[tokio::test]
    async fn pending_registration_ceremony_is_consumed_once_under_concurrency() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = Store::open(&directory.path().join("state"))?;
        let code = store
            .initialize_setup_code()
            .await?
            .context("first startup should create the setup code")?;
        let origin = Origin::from_str("https://chime.example-tailnet.ts.net")?;
        let state = AdminState::new(store, &origin)?;
        let challenge = state
            .start_setup(SetupStartRequest {
                code,
                name: "Primary passkey".to_owned(),
            })
            .await
            .map_err(|error| anyhow::anyhow!(error.message))?;

        let first_state = state.clone();
        let first_id = challenge.request_id.clone();
        let second_state = state.clone();
        let second_id = challenge.request_id.clone();
        let (first, second) = tokio::join!(
            first_state.take_pending(&first_id),
            second_state.take_pending(&second_id)
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert!(state.take_pending(&challenge.request_id).await.is_err());
        Ok(())
    }

    #[test]
    fn operation_arguments_are_typed_and_unknown_arguments_are_rejected() {
        assert!(
            serde_json::from_str::<super::AdminRequest>(
                r#"{"kind":"remove_passkey","id":"AQ","name":"mutated"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<super::AdminFinishRequest>(
            r#"{"request_id":"one-use-id","credential":{},"operation":{"kind":"remove_passkey","id":"AQ"}}"#
        )
        .is_err());
    }
}
