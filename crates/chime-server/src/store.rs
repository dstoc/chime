use crate::{data_dir, vapid::VapidIdentity};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const DATABASE_FILE: &str = "chime.sqlite3";
const DATABASE_CONCURRENCY: usize = 4;
const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    database: Database,
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
            vapid,
        })
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn vapid(&self) -> &VapidIdentity {
        &self.vapid
    }
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
            version => bail!("no database migration is available after version {version}"),
        }
        transaction.commit()?;
        current += 1;
    }

    Ok(())
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
    use rusqlite::{Connection, TransactionBehavior};
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
        assert_eq!(version, 1);
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
