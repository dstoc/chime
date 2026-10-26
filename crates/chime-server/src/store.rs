use crate::devices::ValidatedSubscription;
use crate::{data_dir, vapid::VapidIdentity};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use webauthn_rs::prelude::{AuthenticationResult, Passkey};

const DATABASE_FILE: &str = "chime.sqlite3";
const DATABASE_CONCURRENCY: usize = 4;
const SCHEMA_VERSION: i64 = 4;
const SETUP_CODE_LIFETIME_SECONDS: i64 = 15 * 60;
const MAX_ADMIN_PASSKEYS: usize = 16;
const MAX_SEND_KEYS: usize = 128;
const MAX_RECEIVING_DEVICES: usize = 64;

#[derive(Clone)]
pub struct Store {
    database: Database,
    // Serialize device-token checks with admin enrollment/removal and stream invalidation.
    device_authority: Arc<AsyncMutex<()>>,
    // Loaded and checked here; later milestones consume the persistent identity for push delivery.
    #[allow(dead_code)]
    vapid: VapidIdentity,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let directory = data_dir::prepare_private_directory(path)?;
        let lock = data_dir::DataDirLock::acquire(&directory)?;
        let database_path = directory.join(DATABASE_FILE);
        data_dir::reject_symlink_or_non_file(&database_path)?;

        let mut connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .with_context(|| format!("open SQLite database {}", database_path.display()))?;
        data_dir::set_private_file_permissions(&database_path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        configure_connection(&connection)?;
        initialize_schema(&mut connection)?;
        let vapid = read_vapid_identity(&connection)?;
        drop(connection);

        Ok(Self {
            database: Database {
                path: database_path,
                lock: Arc::new(lock),
                slots: Arc::new(Semaphore::new(DATABASE_CONCURRENCY)),
            },
            device_authority: Arc::new(AsyncMutex::new(())),
            vapid,
        })
    }

    #[allow(dead_code)]
    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn image_directory(&self) -> PathBuf {
        self.database
            .path
            .parent()
            .expect("database path always has a parent")
            .join("images")
    }

    #[allow(dead_code)]
    pub fn vapid(&self) -> &VapidIdentity {
        &self.vapid
    }

    pub async fn authorize_send(&self, secret: String) -> Result<Option<Vec<StoredSubscription>>> {
        let digest = Sha256::digest(secret.as_bytes()).to_vec();
        self.database
            .run(move |connection| {
                let transaction = connection.transaction()?;
                let mut statement = transaction
                    .prepare("SELECT secret_digest FROM send_keys WHERE revoked_at IS NULL")?;
                let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
                let mut valid = false;
                for row in rows {
                    let stored = row?;
                    valid |= stored.len() == digest.len()
                        && bool::from(stored.as_slice().ct_eq(&digest));
                }
                drop(statement);
                if !valid {
                    transaction.commit()?;
                    return Ok(None);
                }
                let subscriptions = read_subscriptions(&transaction)?;
                transaction.commit()?;
                Ok(Some(subscriptions))
            })
            .await
    }

    // The caller holds lock_device_authority through the authorized operation.
    pub(crate) async fn authorize_device(&self, secret: String) -> Result<Option<String>> {
        let Some(encoded) = secret.strip_prefix("chime_dt_") else {
            return Ok(None);
        };
        let Ok(bytes) = URL_SAFE_NO_PAD.decode(encoded) else {
            return Ok(None);
        };
        if bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != encoded {
            return Ok(None);
        }
        let digest = Sha256::digest(secret.as_bytes()).to_vec();
        self.database
            .run(move |connection| {
                let mut statement =
                    connection.prepare("SELECT id, token_digest FROM receiving_devices")?;
                let mut rows = statement.query([])?;
                while let Some(row) = rows.next()? {
                    let id: String = row.get(0)?;
                    let stored: Vec<u8> = row.get(1)?;
                    if stored.len() == digest.len() && bool::from(stored.as_slice().ct_eq(&digest))
                    {
                        return Ok(Some(id));
                    }
                }
                Ok(None)
            })
            .await
    }

    pub(crate) async fn lock_device_authority(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.device_authority).lock_owned().await
    }

    pub async fn remove_expired_subscription(&self, id: String, revision: i64) -> Result<bool> {
        self.database
            .run(move |connection| {
                let changed = connection.execute(
                    "DELETE FROM receiving_devices WHERE id = ?1 AND revision = ?2",
                    params![id, revision],
                )?;
                Ok(changed == 1)
            })
            .await
    }

    /// Create and persist the first-use setup code once, returning it only to the foreground startup.
    pub async fn initialize_setup_code(&self) -> Result<Option<String>> {
        let needs_code = self
            .database
            .run(|connection| {
                connection.query_row(
                    "SELECT initialized = 0 AND setup_digest IS NULL AND setup_consumed = 0
                     FROM admin_bootstrap WHERE singleton = 1",
                    [],
                    |row| row.get::<_, bool>(0),
                )
            })
            .await?;
        if !needs_code {
            return Ok(None);
        }
        let code = generate_setup_code()?;
        let digest = Sha256::digest(code.as_bytes()).to_vec();
        let expires_at = unix_now()? + SETUP_CODE_LIFETIME_SECONDS;
        self.database
            .run(move |connection| {
                let changed = connection.execute(
                    "UPDATE admin_bootstrap
                     SET setup_digest = ?1, setup_expires_at = ?2
                     WHERE singleton = 1 AND initialized = 0
                       AND setup_digest IS NULL AND setup_consumed = 0",
                    params![digest, expires_at],
                )?;
                Ok((changed == 1).then_some(code))
            })
            .await
    }

    pub async fn setup_status(&self) -> Result<SetupStatus> {
        self.database
            .run(|connection| {
                connection.query_row(
                    "SELECT initialized, setup_digest IS NOT NULL AND setup_consumed = 0
                         AND setup_expires_at > unixepoch()
                     FROM admin_bootstrap WHERE singleton = 1",
                    [],
                    |row| {
                        Ok(SetupStatus {
                            initialized: row.get::<_, i64>(0)? != 0,
                            available: row.get::<_, i64>(1)? != 0,
                        })
                    },
                )
            })
            .await
    }

    pub async fn consume_setup_code(&self, code: String) -> Result<Option<u64>> {
        let code_digest = Sha256::digest(code.as_bytes()).to_vec();
        let now = unix_now()?;
        self.database
            .run(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let (initialized, consumed, expires_at, stored_digest): (
                    i64,
                    i64,
                    Option<i64>,
                    Option<Vec<u8>>,
                ) = transaction.query_row(
                    "SELECT initialized, setup_consumed, setup_expires_at, setup_digest
                     FROM admin_bootstrap WHERE singleton = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
                let valid = initialized == 0
                    && consumed == 0
                    && expires_at.is_some_and(|expiry| expiry > now)
                    && stored_digest
                        .as_deref()
                        .is_some_and(|stored| bool::from(stored.ct_eq(&code_digest)));
                if valid {
                    transaction.execute(
                        "UPDATE admin_bootstrap SET setup_consumed = 1
                         WHERE singleton = 1 AND initialized = 0 AND setup_consumed = 0",
                        [],
                    )?;
                }
                transaction.commit()?;
                Ok(if valid {
                    expires_at.map(|expiry| (expiry - now) as u64)
                } else {
                    None
                })
            })
            .await
    }

    pub async fn passkeys(&self) -> Result<Vec<AdminPasskey>> {
        self.database
            .run(|connection| read_passkeys(connection))
            .await
    }

    pub async fn complete_admin_operation(
        &self,
        result: AuthenticationResult,
        operation: AuthenticatedOperation,
    ) -> Result<AdminCompletion> {
        self.database
            .run(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let credential_id = result.cred_id().as_ref().to_vec();
                let current_json: Option<String> = transaction
                    .query_row(
                        "SELECT passkey_json FROM admin_passkeys WHERE credential_id = ?1",
                        params![credential_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(current_json) = current_json else {
                    return Err(store_error("authorizing passkey is no longer registered"));
                };
                let mut passkey: Passkey =
                    serde_json::from_str(&current_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                let changed = passkey.update_credential(&result).unwrap_or(false);
                if changed {
                    let encoded = serde_json::to_string(&passkey).map_err(|error| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                    })?;
                    transaction.execute(
                        "UPDATE admin_passkeys SET passkey_json = ?1 WHERE credential_id = ?2",
                        params![encoded, credential_id],
                    )?;
                }

                let completion = match operation {
                    AuthenticatedOperation::List => {
                        AdminCompletion::Listed(read_passkeys(&transaction)?)
                    }
                    AuthenticatedOperation::Remove(credential_id) => {
                        let count: i64 = transaction.query_row(
                            "SELECT COUNT(*) FROM admin_passkeys",
                            [],
                            |row| row.get(0),
                        )?;
                        if count <= 1 {
                            AdminCompletion::Rejected(AdminRejection::LastPasskey)
                        } else {
                            let removed = transaction.execute(
                                "DELETE FROM admin_passkeys WHERE credential_id = ?1",
                                params![credential_id],
                            )?;
                            if removed != 1 {
                                AdminCompletion::Rejected(AdminRejection::PasskeyNotFound)
                            } else {
                                AdminCompletion::Listed(read_passkeys(&transaction)?)
                            }
                        }
                    }
                    AuthenticatedOperation::Add(name) => {
                        let count: i64 = transaction.query_row(
                            "SELECT COUNT(*) FROM admin_passkeys",
                            [],
                            |row| row.get(0),
                        )?;
                        if count >= MAX_ADMIN_PASSKEYS as i64 {
                            AdminCompletion::Rejected(AdminRejection::PasskeyLimit)
                        } else {
                            AdminCompletion::AddAuthorized {
                                name,
                                authorizer_id: result.cred_id().as_ref().to_vec(),
                                exclude_credentials: read_passkeys(&transaction)?
                                    .into_iter()
                                    .map(|entry| entry.passkey.cred_id().clone())
                                    .collect(),
                            }
                        }
                    }
                    AuthenticatedOperation::ListSendKeys => {
                        AdminCompletion::SendKeys(read_send_keys(&transaction)?)
                    }
                    AuthenticatedOperation::CreateSendKey(name) => {
                        let count: i64 =
                            transaction.query_row("SELECT COUNT(*) FROM send_keys", [], |row| {
                                row.get(0)
                            })?;
                        if count >= MAX_SEND_KEYS as i64 {
                            AdminCompletion::Rejected(AdminRejection::SendKeyLimit)
                        } else {
                            let id = new_record_id().map_err(|error| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(
                                    std::io::Error::other(error.to_string()),
                                ))
                            })?;
                            let mut bytes = [0_u8; 32];
                            getrandom::fill(&mut bytes).map_err(|error| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                            })?;
                            let secret = format!("chime_sk_{}", URL_SAFE_NO_PAD.encode(bytes));
                            let digest = Sha256::digest(secret.as_bytes()).to_vec();
                            transaction.execute(
                                "INSERT INTO send_keys (id, name, secret_digest, created_at)
                                 VALUES (?1, ?2, ?3, unixepoch())",
                                params![id, name, digest],
                            )?;
                            let created_at = transaction.query_row(
                                "SELECT created_at FROM send_keys WHERE id = ?1",
                                params![id],
                                |row| row.get(0),
                            )?;
                            let key = SendKeySummary {
                                id,
                                name,
                                created_at,
                                revoked_at: None,
                            };
                            AdminCompletion::CreatedSendKey {
                                created: CreatedSendKey { secret, key },
                                keys: read_send_keys(&transaction)?,
                            }
                        }
                    }
                    AuthenticatedOperation::RevokeSendKey(id) => {
                        let removed = transaction.execute(
                            "UPDATE send_keys SET revoked_at = unixepoch()
                             WHERE id = ?1 AND revoked_at IS NULL",
                            params![id],
                        )?;
                        if removed == 0 {
                            AdminCompletion::Rejected(AdminRejection::SendKeyNotFound)
                        } else {
                            AdminCompletion::SendKeys(read_send_keys(&transaction)?)
                        }
                    }
                    AuthenticatedOperation::ListDevices => {
                        AdminCompletion::Devices(read_devices(&transaction)?)
                    }
                    AuthenticatedOperation::EnrollDevice { name, subscription } => {
                        let token = generate_secret("chime_dt_").map_err(|error| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(
                                std::io::Error::other(error.to_string()),
                            ))
                        })?;
                        let token_digest = Sha256::digest(token.as_bytes()).to_vec();
                        let existing_id: Option<String> = transaction
                            .query_row(
                                "SELECT id FROM receiving_devices WHERE endpoint = ?1",
                                params![subscription.endpoint],
                                |row| row.get(0),
                            )
                            .optional()?;
                        let id = if let Some(id) = existing_id {
                            transaction.execute(
                                "UPDATE receiving_devices
                                 SET name = ?1, p256dh = ?2, auth = ?3, token_digest = ?4,
                                     updated_at = unixepoch(), revision = revision + 1
                                 WHERE id = ?5",
                                params![
                                    name,
                                    subscription.p256dh,
                                    subscription.auth,
                                    token_digest,
                                    id
                                ],
                            )?;
                            id
                        } else {
                            let count: i64 = transaction.query_row(
                                "SELECT COUNT(*) FROM receiving_devices",
                                [],
                                |row| row.get(0),
                            )?;
                            if count >= MAX_RECEIVING_DEVICES as i64 {
                                return Ok(AdminCompletion::Rejected(AdminRejection::DeviceLimit));
                            }
                            let id = new_record_id().map_err(|error| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(
                                    std::io::Error::other(error.to_string()),
                                ))
                            })?;
                            transaction.execute(
                                "INSERT INTO receiving_devices
                                 (id, name, endpoint, p256dh, auth, token_digest,
                                  created_at, updated_at, revision)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch(), unixepoch(), 1)",
                                params![
                                    id,
                                    name,
                                    subscription.endpoint,
                                    subscription.p256dh,
                                    subscription.auth,
                                    token_digest
                                ],
                            )?;
                            id
                        };
                        let device = transaction.query_row(
                            "SELECT id, name, created_at, updated_at
                             FROM receiving_devices WHERE id = ?1",
                            params![id],
                            read_device_summary,
                        )?;
                        AdminCompletion::EnrolledDevice {
                            enrolled: EnrolledDevice { token, device },
                            devices: read_devices(&transaction)?,
                        }
                    }
                    AuthenticatedOperation::RemoveDevice(id) => {
                        let removed = transaction
                            .execute("DELETE FROM receiving_devices WHERE id = ?1", params![id])?;
                        if removed == 0 {
                            AdminCompletion::Rejected(AdminRejection::DeviceNotFound)
                        } else {
                            AdminCompletion::Devices(read_devices(&transaction)?)
                        }
                    }
                };
                transaction.commit()?;
                Ok(completion)
            })
            .await
    }

    pub async fn register_first_passkey(&self, passkey: Passkey, name: String) -> Result<()> {
        let now = unix_now()?;
        self.database
            .run(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let (initialized, consumed, expires_at): (i64, i64, Option<i64>) = transaction
                    .query_row(
                        "SELECT initialized, setup_consumed, setup_expires_at
                     FROM admin_bootstrap WHERE singleton = 1",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                if initialized != 0
                    || consumed == 0
                    || expires_at.is_none_or(|expiry| expiry <= now)
                {
                    return Err(store_error("first-use setup is no longer available"));
                }
                insert_passkey(&transaction, &passkey, &name)?;
                transaction.execute(
                    "UPDATE admin_bootstrap SET initialized = 1, setup_digest = NULL,
                         setup_expires_at = NULL, setup_consumed = 1
                     WHERE singleton = 1 AND initialized = 0 AND setup_consumed = 1",
                    [],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    pub async fn register_authorized_passkey(
        &self,
        passkey: Passkey,
        name: String,
        authorizer_id: Vec<u8>,
    ) -> Result<PasskeyRegistrationResult> {
        self.database
            .run(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let initialized: i64 = transaction.query_row(
                    "SELECT initialized FROM admin_bootstrap WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )?;
                let authorizer_exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM admin_passkeys WHERE credential_id = ?1)",
                    params![authorizer_id],
                    |row| row.get(0),
                )?;
                if initialized == 0 || !authorizer_exists {
                    transaction.commit()?;
                    return Ok(PasskeyRegistrationResult::AuthorizerRemoved);
                }
                let duplicate: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM admin_passkeys WHERE credential_id = ?1)",
                    params![passkey.cred_id().as_ref()],
                    |row| row.get(0),
                )?;
                if duplicate {
                    transaction.commit()?;
                    return Ok(PasskeyRegistrationResult::Duplicate);
                }
                let count: i64 =
                    transaction
                        .query_row("SELECT COUNT(*) FROM admin_passkeys", [], |row| row.get(0))?;
                if count >= MAX_ADMIN_PASSKEYS as i64 {
                    transaction.commit()?;
                    return Ok(PasskeyRegistrationResult::Limit);
                }
                insert_passkey(&transaction, &passkey, &name)?;
                transaction.commit()?;
                Ok(PasskeyRegistrationResult::Added)
            })
            .await
    }

    /// Replace administrator credentials while the caller holds the exclusive data-directory lock.
    pub fn reset_admin(path: &Path) -> Result<String> {
        let directory = data_dir::prepare_private_directory(path)?;
        let _lock = data_dir::DataDirLock::acquire(&directory)?;
        let database_path = directory.join(DATABASE_FILE);
        data_dir::reject_symlink_or_non_file(&database_path)?;
        if !database_path.exists() {
            bail!("no Chime database exists in {}", directory.display());
        }
        let mut connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .with_context(|| format!("open SQLite database {}", database_path.display()))?;
        configure_connection(&connection)?;
        initialize_schema(&mut connection)?;

        let code = generate_setup_code()?;
        let digest = Sha256::digest(code.as_bytes()).to_vec();
        let expires_at = unix_now()? + SETUP_CODE_LIFETIME_SECONDS;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM admin_passkeys", [])?;
        transaction.execute(
            "UPDATE admin_bootstrap SET initialized = 0, setup_digest = ?1,
                 setup_expires_at = ?2, setup_consumed = 0 WHERE singleton = 1",
            params![digest, expires_at],
        )?;
        transaction.commit()?;
        Ok(code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupStatus {
    pub initialized: bool,
    pub available: bool,
}

#[derive(Clone)]
pub struct AdminPasskey {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub passkey: Passkey,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SendKeySummary {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

pub struct CreatedSendKey {
    pub secret: String,
    pub key: SendKeySummary,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReceivingDeviceSummary {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct EnrolledDevice {
    pub token: String,
    pub device: ReceivingDeviceSummary,
}

#[derive(Debug, Clone)]
pub struct StoredSubscription {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub revision: i64,
}

pub enum AuthenticatedOperation {
    List,
    Add(String),
    Remove(Vec<u8>),
    ListSendKeys,
    CreateSendKey(String),
    RevokeSendKey(String),
    ListDevices,
    EnrollDevice {
        name: String,
        subscription: ValidatedSubscription,
    },
    RemoveDevice(String),
}

pub enum AdminCompletion {
    Listed(Vec<AdminPasskey>),
    Rejected(AdminRejection),
    SendKeys(Vec<SendKeySummary>),
    CreatedSendKey {
        created: CreatedSendKey,
        keys: Vec<SendKeySummary>,
    },
    Devices(Vec<ReceivingDeviceSummary>),
    EnrolledDevice {
        enrolled: EnrolledDevice,
        devices: Vec<ReceivingDeviceSummary>,
    },
    AddAuthorized {
        name: String,
        authorizer_id: Vec<u8>,
        exclude_credentials: Vec<webauthn_rs::prelude::CredentialID>,
    },
}

pub enum AdminRejection {
    LastPasskey,
    PasskeyNotFound,
    PasskeyLimit,
    SendKeyNotFound,
    SendKeyLimit,
    DeviceNotFound,
    DeviceLimit,
}

pub enum PasskeyRegistrationResult {
    Added,
    Duplicate,
    AuthorizerRemoved,
    Limit,
}

#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    lock: Arc<data_dir::DataDirLock>,
    slots: Arc<Semaphore>,
}

impl Database {
    pub async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let permit = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .context("SQLite operation queue is closed")?;
        let path = self.path.clone();
        let lock = Arc::clone(&self.lock);

        tokio::task::spawn_blocking(move || run_blocking(&path, permit, lock, operation))
            .await
            .context("SQLite blocking worker failed")?
    }
}

fn run_blocking<T, F>(
    path: &Path,
    _permit: OwnedSemaphorePermit,
    _lock: Arc<data_dir::DataDirLock>,
    operation: F,
) -> Result<T>
where
    F: FnOnce(&mut Connection) -> rusqlite::Result<T>,
{
    (|| {
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_FULL_MUTEX,
        )
        .with_context(|| format!("open SQLite connection {}", path.display()))?;
        configure_connection(&connection)?;
        operation(&mut connection).context("run SQLite operation")
    })()
}

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(())
}

fn initialize_schema(connection: &mut Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;

    let mut current: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if current > SCHEMA_VERSION {
        bail!("database schema version {current} is newer than this server supports");
    }

    while current < SCHEMA_VERSION {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        match current {
            0 => {
                let vapid = VapidIdentity::generate()?;
                apply_migration_one(&transaction, &vapid)?;
            }
            1 => apply_migration_two(&transaction)?,
            2 => apply_migration_three(&transaction)?,
            3 => apply_migration_four(&transaction)?,
            version => bail!("no database migration is available after version {version}"),
        }
        transaction.commit()?;
        current += 1;
    }

    Ok(())
}

fn apply_migration_two(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE admin_bootstrap (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            initialized INTEGER NOT NULL CHECK (initialized IN (0, 1)),
            setup_digest BLOB CHECK (setup_digest IS NULL OR length(setup_digest) = 32),
            setup_expires_at INTEGER,
            setup_consumed INTEGER NOT NULL CHECK (setup_consumed IN (0, 1)),
            CHECK ((setup_digest IS NULL) = (setup_expires_at IS NULL))
        );
        INSERT INTO admin_bootstrap
            (singleton, initialized, setup_digest, setup_expires_at, setup_consumed)
            VALUES (1, 0, NULL, NULL, 0);
        CREATE TABLE admin_passkeys (
            credential_id BLOB PRIMARY KEY CHECK (length(credential_id) BETWEEN 1 AND 1024),
            name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
            passkey_json TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        INSERT INTO schema_migrations (version, applied_at) VALUES (2, unixepoch());",
    )?;
    Ok(())
}

fn apply_migration_three(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE send_keys (
            id TEXT PRIMARY KEY CHECK (length(id) = 22),
            name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
            secret_digest BLOB NOT NULL CHECK (length(secret_digest) = 32),
            created_at INTEGER NOT NULL,
            revoked_at INTEGER
        );
        CREATE TABLE receiving_devices (
            id TEXT PRIMARY KEY CHECK (length(id) = 22),
            name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
            endpoint TEXT NOT NULL UNIQUE CHECK (length(endpoint) BETWEEN 1 AND 2048),
            p256dh TEXT NOT NULL,
            auth TEXT NOT NULL,
            token_digest BLOB NOT NULL CHECK (length(token_digest) = 32),
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0)
        );
        CREATE INDEX send_keys_active_idx ON send_keys (revoked_at);
        INSERT INTO schema_migrations (version, applied_at) VALUES (3, unixepoch());",
    )?;
    Ok(())
}

fn apply_migration_four(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE uploaded_images (
            capability TEXT PRIMARY KEY CHECK (length(capability) = 43),
            content_type TEXT NOT NULL CHECK (content_type IN ('image/png', 'image/jpeg', 'image/webp')),
            byte_length INTEGER NOT NULL CHECK (byte_length BETWEEN 1 AND 5242880),
            expires_at INTEGER NOT NULL
        );
        CREATE INDEX uploaded_images_expiry_idx ON uploaded_images (expires_at);
        INSERT INTO schema_migrations (version, applied_at) VALUES (4, unixepoch());",
    )?;
    Ok(())
}

fn read_send_keys(connection: &Connection) -> rusqlite::Result<Vec<SendKeySummary>> {
    let mut statement = connection.prepare(
        "SELECT id, name, created_at, revoked_at FROM send_keys ORDER BY created_at, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(SendKeySummary {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            revoked_at: row.get(3)?,
        })
    })?;
    rows.collect()
}

fn read_devices(connection: &Connection) -> rusqlite::Result<Vec<ReceivingDeviceSummary>> {
    let mut statement = connection.prepare(
        "SELECT id, name, created_at, updated_at
         FROM receiving_devices ORDER BY created_at, id",
    )?;
    let rows = statement.query_map([], read_device_summary)?;
    rows.collect()
}

fn read_device_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReceivingDeviceSummary> {
    Ok(ReceivingDeviceSummary {
        id: row.get(0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
    })
}

fn read_subscriptions(connection: &Connection) -> rusqlite::Result<Vec<StoredSubscription>> {
    let mut statement = connection.prepare(
        "SELECT id, endpoint, p256dh, auth, revision
         FROM receiving_devices ORDER BY created_at, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(StoredSubscription {
            id: row.get(0)?,
            endpoint: row.get(1)?,
            p256dh: row.get(2)?,
            auth: row.get(3)?,
            revision: row.get(4)?,
        })
    })?;
    rows.collect()
}

fn new_record_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("generate record ID")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn generate_secret(prefix: &str) -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).context("generate bearer secret")?;
    Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn read_passkeys(connection: &Connection) -> rusqlite::Result<Vec<AdminPasskey>> {
    let mut statement = connection.prepare(
        "SELECT credential_id, name, passkey_json, created_at
         FROM admin_passkeys ORDER BY created_at, credential_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    rows.map(|row| {
        let (credential_id, name, passkey_json, created_at) = row?;
        let passkey: Passkey = serde_json::from_str(&passkey_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        Ok(AdminPasskey {
            id: URL_SAFE_NO_PAD.encode(credential_id),
            name,
            created_at,
            passkey,
        })
    })
    .collect()
}

fn insert_passkey(
    transaction: &Transaction<'_>,
    passkey: &Passkey,
    name: &str,
) -> rusqlite::Result<()> {
    let credential_id = passkey.cred_id().as_ref();
    let passkey_json = serde_json::to_string(passkey)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    transaction.execute(
        "INSERT INTO admin_passkeys (credential_id, name, passkey_json, created_at)
         VALUES (?1, ?2, ?3, unixepoch())",
        params![credential_id, name, passkey_json],
    )?;
    Ok(())
}

fn store_error(message: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message.to_owned())
}

fn generate_setup_code() -> Result<String> {
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).context("generate first-use setup code")?;
    Ok(URL_SAFE_NO_PAD.encode(secret))
}

fn unix_now() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    i64::try_from(elapsed.as_secs()).context("system clock value is out of range")
}

fn apply_migration_one(transaction: &Transaction<'_>, vapid: &VapidIdentity) -> Result<()> {
    apply_migration_one_with_keys(
        transaction,
        vapid.private_key_bytes(),
        vapid.public_key_bytes(),
    )
}

fn apply_migration_one_with_keys(
    transaction: &Transaction<'_>,
    private_key: &[u8],
    public_key: &[u8],
) -> Result<()> {
    transaction.execute_batch(
        "CREATE TABLE vapid_identity (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            private_key BLOB NOT NULL CHECK (length(private_key) = 32),
            public_key BLOB NOT NULL CHECK (length(public_key) = 65),
            created_at INTEGER NOT NULL
        );",
    )?;
    transaction.execute(
        "INSERT INTO vapid_identity (singleton, private_key, public_key, created_at)
         VALUES (1, ?1, ?2, unixepoch())",
        params![private_key, public_key],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (1, unixepoch())",
        [],
    )?;
    Ok(())
}

fn read_vapid_identity(connection: &Connection) -> Result<VapidIdentity> {
    let private_key: Vec<u8> = connection.query_row(
        "SELECT private_key FROM vapid_identity WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let identity = VapidIdentity::from_private_key(private_key)?;
    let stored_public_key: Vec<u8> = connection.query_row(
        "SELECT public_key FROM vapid_identity WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if stored_public_key != identity.public_key_bytes() {
        bail!("stored VAPID public key does not match its private key");
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::{Database, Store, apply_migration_one_with_keys, configure_connection};
    use crate::vapid::VapidIdentity;
    use anyhow::{Context, Result};
    use rusqlite::{Connection, TransactionBehavior, params};
    use std::{
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    fn assert_directory_locked(path: &Path) {
        let error = match Store::open(path) {
            Ok(_) => panic!("another Store acquired a directory that is still in use"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("already in use"),
            "expected the directory lock to reject another Store, got: {error:#}"
        );
    }

    async fn wait_until_directory_opens(path: &Path) -> Result<Store> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match Store::open(path) {
                    Ok(store) => return Ok(store),
                    Err(error) if error.to_string().contains("already in use") => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await
        .context("directory lock was not released after the blocking worker finished")?
    }

    #[test]
    fn migrations_are_atomic() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        configure_connection(&connection)?;
        connection.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );",
        )?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let invalid_identity = VapidIdentity::generate()?;

        assert!(
            apply_migration_one_with_keys(
                &transaction,
                &[0; 31],
                invalid_identity.public_key_bytes()
            )
            .is_err()
        );
        drop(transaction);

        let table_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'vapid_identity'",
            [],
            |row| row.get(0),
        )?;
        let version_count: i64 =
            connection.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })?;
        assert_eq!(table_count, 0);
        assert_eq!(version_count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn restart_keeps_vapid_identity_and_database_state() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let first = Store::open(&path)?;
        let private_key = first.vapid().private_key_bytes().to_vec();
        let public_key = first.vapid().public_key_base64();
        drop(first);

        let second = Store::open(&path)?;
        assert_eq!(second.vapid().private_key_bytes(), private_key);
        assert_eq!(second.vapid().public_key_base64(), public_key);
        let version = second
            .database()
            .run(|connection| {
                connection.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await?;
        assert_eq!(version, 4);
        Ok(())
    }

    #[tokio::test]
    async fn sender_keys_are_digested_and_revoked_independently() -> Result<()> {
        use sha2::{Digest, Sha256};

        let root = tempfile::tempdir()?;
        let store = Store::open(&root.path().join("state"))?;
        let first = "chime_sk_first-test-secret".to_owned();
        let second = "chime_sk_second-test-secret".to_owned();
        let first_digest = Sha256::digest(first.as_bytes()).to_vec();
        let second_digest = Sha256::digest(second.as_bytes()).to_vec();
        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "INSERT INTO send_keys (id, name, secret_digest, created_at)
                     VALUES ('AAAAAAAAAAAAAAAAAAAAAA', 'first', ?1, unixepoch())",
                    params![first_digest],
                )?;
                connection.execute(
                    "INSERT INTO send_keys (id, name, secret_digest, created_at)
                     VALUES ('BBBBBBBBBBBBBBBBBBBBBB', 'second', ?1, unixepoch())",
                    params![second_digest],
                )?;
                Ok(())
            })
            .await?;

        let stored = store
            .database()
            .run(|connection| {
                connection.query_row(
                    "SELECT secret_digest FROM send_keys WHERE name = 'first'",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
            })
            .await?;
        assert_eq!(stored, Sha256::digest(first.as_bytes()).to_vec());
        assert_ne!(stored, first.as_bytes());
        assert!(store.authorize_send(first.clone()).await?.is_some());
        assert!(store.authorize_send(second.clone()).await?.is_some());

        store
            .database()
            .run(|connection| {
                connection.execute(
                    "UPDATE send_keys SET revoked_at = unixepoch()
                     WHERE id = 'AAAAAAAAAAAAAAAAAAAAAA'",
                    [],
                )?;
                Ok(())
            })
            .await?;
        assert!(store.authorize_send(first).await?.is_none());
        assert!(store.authorize_send(second).await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn old_provider_expiry_cannot_remove_a_rotated_device_record() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(&root.path().join("state"))?;
        store
            .database()
            .run(|connection| {
                connection.execute(
                    "INSERT INTO receiving_devices
                     (id, name, endpoint, p256dh, auth, token_digest,
                      created_at, updated_at, revision)
                     VALUES ('CCCCCCCCCCCCCCCCCCCCCC', 'phone',
                       'https://fcm.googleapis.com/fcm/send/test', 'public', 'auth',
                       zeroblob(32), unixepoch(), unixepoch(), 2)",
                    [],
                )?;
                Ok(())
            })
            .await?;

        assert!(
            !store
                .remove_expired_subscription("CCCCCCCCCCCCCCCCCCCCCC".to_owned(), 1)
                .await?
        );
        assert!(
            store
                .remove_expired_subscription("CCCCCCCCCCCCCCCCCCCCCC".to_owned(), 2)
                .await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn setup_code_is_hashed_disclosed_once_and_consumed_once() -> Result<()> {
        use sha2::{Digest, Sha256};

        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let store = Store::open(&path)?;
        let code = store
            .initialize_setup_code()
            .await?
            .context("first startup should create the setup code")?;
        assert!(store.initialize_setup_code().await?.is_none());

        let stored_digest = store
            .database()
            .run(|connection| {
                connection.query_row(
                    "SELECT setup_digest FROM admin_bootstrap WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
            })
            .await?;
        assert_eq!(stored_digest, Sha256::digest(code.as_bytes()).to_vec());
        assert_ne!(stored_digest, code.as_bytes());

        assert!(
            store
                .consume_setup_code("incorrect".to_owned())
                .await?
                .is_none()
        );
        assert!(store.consume_setup_code(code.clone()).await?.is_some());
        assert!(store.consume_setup_code(code).await?.is_none());
        assert!(!store.setup_status().await?.available);
        Ok(())
    }

    #[tokio::test]
    async fn setup_code_expires_and_concurrent_replays_only_consume_once() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let store = Arc::new(Store::open(&path)?);
        let code = store
            .initialize_setup_code()
            .await?
            .context("first startup should create the setup code")?;

        let first_store = Arc::clone(&store);
        let first_code = code.clone();
        let first = tokio::spawn(async move { first_store.consume_setup_code(first_code).await });
        let second_store = Arc::clone(&store);
        let second = tokio::spawn(async move { second_store.consume_setup_code(code).await });
        let first = first.await??;
        let second = second.await??;
        assert_eq!(
            usize::from(first.is_some()) + usize::from(second.is_some()),
            1
        );

        let expired = Store::open(&root.path().join("expired"))?;
        let expired_code = expired
            .initialize_setup_code()
            .await?
            .context("first startup should create the setup code")?;
        expired
            .database()
            .run(|connection| {
                connection.execute(
                    "UPDATE admin_bootstrap SET setup_expires_at = unixepoch() - 1 WHERE singleton = 1",
                    [],
                )?;
                Ok(())
            })
            .await?;
        assert!(expired.consume_setup_code(expired_code).await?.is_none());
        assert!(!expired.setup_status().await?.available);
        Ok(())
    }

    #[tokio::test]
    async fn initialized_restart_does_not_create_a_new_setup_code() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let store = Store::open(&path)?;
        store.initialize_setup_code().await?;
        store
            .database()
            .run(|connection| {
                connection.execute(
                    "UPDATE admin_bootstrap SET initialized = 1, setup_digest = NULL,
                         setup_expires_at = NULL, setup_consumed = 1 WHERE singleton = 1",
                    [],
                )?;
                Ok(())
            })
            .await?;
        drop(store);

        let restarted = Store::open(&path)?;
        assert!(restarted.initialize_setup_code().await?.is_none());
        assert!(restarted.setup_status().await?.initialized);
        Ok(())
    }

    #[tokio::test]
    async fn reset_requires_exclusive_lock_and_preserves_non_admin_state() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let store = Store::open(&path)?;
        store.initialize_setup_code().await?;
        let original_vapid_private = store.vapid().private_key_bytes().to_vec();
        let original_vapid_public = store.vapid().public_key_bytes().to_vec();
        store
            .database()
            .run(|connection| {
                connection.execute_batch(
                    "CREATE TABLE subscriptions (id INTEGER PRIMARY KEY, value TEXT);",
                )?;
                connection.execute(
                    "INSERT INTO send_keys (id, name, secret_digest, created_at)
                     VALUES ('AAAAAAAAAAAAAAAAAAAAAA', 'keep', zeroblob(32), unixepoch())",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO receiving_devices
                     (id, name, endpoint, p256dh, auth, token_digest,
                      created_at, updated_at, revision)
                     VALUES ('BBBBBBBBBBBBBBBBBBBBBB', 'keep',
                       'https://fcm.googleapis.com/fcm/send/keep', 'public', 'auth',
                       zeroblob(32), unixepoch(), unixepoch(), 1)",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO subscriptions (id, value) VALUES (1, 'keep')",
                    [],
                )?;
                connection.execute(
                    "INSERT INTO admin_passkeys (credential_id, name, passkey_json, created_at)
                     VALUES (x'01', 'test', '{}', unixepoch())",
                    [],
                )?;
                Ok(())
            })
            .await?;
        let locked = Store::reset_admin(&path).expect_err("reset must refuse a live Store lock");
        assert!(locked.to_string().contains("already in use"));
        drop(store);

        let new_code = Store::reset_admin(&path)?;
        let restarted = Store::open(&path)?;
        assert!(restarted.setup_status().await?.available);
        assert!(restarted.consume_setup_code(new_code).await?.is_some());
        assert_eq!(
            restarted.vapid().private_key_bytes(),
            original_vapid_private
        );
        assert_eq!(restarted.vapid().public_key_bytes(), original_vapid_public);
        let preserved = restarted
            .database()
            .run(|connection| {
                let keys: i64 =
                    connection.query_row("SELECT COUNT(*) FROM send_keys", [], |row| row.get(0))?;
                let devices: i64 =
                    connection.query_row("SELECT COUNT(*) FROM receiving_devices", [], |row| {
                        row.get(0)
                    })?;
                let passkeys: i64 =
                    connection
                        .query_row("SELECT COUNT(*) FROM admin_passkeys", [], |row| row.get(0))?;
                Ok((keys, devices, passkeys))
            })
            .await?;
        assert_eq!(preserved, (1, 1, 0));
        Ok(())
    }

    #[tokio::test]
    async fn cloned_database_keeps_directory_locked_until_dropped() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let first = Store::open(&path)?;
        let database = first.database().clone();
        drop(first);

        assert_directory_locked(&path);
        drop(database);

        let _second = Store::open(&path)?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_caller_keeps_directory_locked_until_worker_finishes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let first = Store::open(&path)?;
        let database = first.database().clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();

        let task = tokio::spawn(async move {
            database
                .run(move |connection| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    let result = connection
                        .execute_batch("CREATE TABLE cancelled_owner_writes (id INTEGER)");
                    finished_tx.send(()).unwrap();
                    result
                })
                .await
        });
        started_rx.await?;
        task.abort();
        assert!(
            task.await
                .expect_err("aborted task should not finish normally")
                .is_cancelled()
        );
        drop(first);

        assert_directory_locked(&path);
        release_tx.send(()).unwrap();
        finished_rx.await?;

        let second = wait_until_directory_opens(&path).await?;
        let table_count = second
            .database()
            .run(|connection| {
                connection.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'cancelled_owner_writes'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await?;
        assert_eq!(table_count, 1);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_operations_run_on_a_blocking_thread() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(&root.path().join("state"))?;
        let executor_thread = thread::current().id();
        let operation_thread = store.database().run(|_| Ok(thread::current().id())).await?;

        assert_ne!(operation_thread, executor_thread);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_blocking_work_is_semaphore_bounded() -> Result<()> {
        let root = tempfile::tempdir()?;
        let store = Store::open(&root.path().join("state"))?;
        let database = store.database().clone();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for _ in 0..12 {
            let database: Database = database.clone();
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tasks.push(tokio::spawn(async move {
                database
                    .run(move |_| {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .await
            }));
        }
        for task in tasks {
            task.await??;
        }

        assert!(maximum.load(Ordering::SeqCst) <= 4);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_database_file_is_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let store = Store::open(&path)?;
        assert_eq!(
            std::fs::metadata(path.join("chime.sqlite3"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(store);
        Ok(())
    }
}
