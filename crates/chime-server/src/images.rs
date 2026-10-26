use crate::store::Store;
use anyhow::{Context, Result};
use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_IMAGE_STORAGE_BYTES: i64 = 256 * 1024 * 1024;
const IMAGE_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    Png,
    Jpeg,
    Webp,
}

impl ImageType {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    pub fn matches_signature(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(b"\xff\xd8\xff"),
            Self::Webp => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
        }
    }
}

#[derive(Debug)]
pub struct ImageError {
    pub status: StatusCode,
    pub message: &'static str,
}

#[derive(Clone)]
pub struct ImageStore {
    store: Store,
    directory: PathBuf,
}

pub struct ImageCapability {
    pub token: String,
    pub url: String,
}

struct ImageRecord {
    token: String,
    content_type: String,
    byte_length: i64,
    expires_at: i64,
}

impl ImageStore {
    pub fn new(store: Store) -> Result<Self> {
        let directory = store.image_directory();
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                anyhow::bail!("image directory must be a directory and must not be a symbolic link")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&directory)
                    .with_context(|| format!("create image directory {}", directory.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", directory.display()));
            }
        }
        set_private_directory_permissions(&directory)?;
        Ok(Self { store, directory })
    }

    pub fn capability(&self, origin: &str) -> Result<ImageCapability, ImageError> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| internal_error())?;
        let token = URL_SAFE_NO_PAD.encode(bytes);
        Ok(ImageCapability {
            url: format!("{origin}/api/images/{token}"),
            token,
        })
    }

    pub async fn save(
        &self,
        capability: &ImageCapability,
        image_type: ImageType,
        bytes: Vec<u8>,
    ) -> Result<(), ImageError> {
        if bytes.is_empty()
            || bytes.len() > MAX_IMAGE_BYTES
            || !image_type.matches_signature(&bytes)
        {
            return Err(invalid_image());
        }
        self.cleanup_expired().await.map_err(|_| internal_error())?;
        let byte_length = i64::try_from(bytes.len()).map_err(|_| invalid_image())?;
        let now = unix_now().map_err(|_| internal_error())?;
        let expires_at = now + IMAGE_TTL_SECONDS;
        let token = capability.token.clone();
        let content_type = image_type.as_str().to_owned();
        let reserved = self
            .store
            .database()
            .run(move |connection| {
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                let used: i64 = transaction.query_row(
                    "SELECT COALESCE(SUM(byte_length), 0) FROM uploaded_images WHERE expires_at > ?1",
                    [now],
                    |row| row.get(0),
                )?;
                if used.saturating_add(byte_length) > MAX_IMAGE_STORAGE_BYTES {
                    return Ok(false);
                }
                transaction.execute(
                    "INSERT INTO uploaded_images (capability, content_type, byte_length, expires_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![token, content_type, byte_length, expires_at],
                )?;
                transaction.commit()?;
                Ok(true)
            })
            .await
            .map_err(|_| internal_error())?;
        if !reserved {
            return Err(capacity_error());
        }

        let directory = self.directory.clone();
        let token = capability.token.clone();
        let write_result =
            tokio::task::spawn_blocking(move || write_atomic(&directory, &token, &bytes)).await;
        if !matches!(write_result, Ok(Ok(()))) {
            self.remove_record(capability.token.clone())
                .await
                .map_err(|_| internal_error())?;
            return Err(internal_error());
        }
        Ok(())
    }

    pub async fn load(&self, token: String) -> Result<Option<(Vec<u8>, &'static str)>> {
        if !valid_capability(&token) {
            return Ok(None);
        }
        let now = unix_now()?;
        let lookup_token = token.clone();
        let record = self
            .store
            .database()
            .run(move |connection| {
                connection
                    .query_row(
                        "SELECT capability, content_type, byte_length, expires_at
                         FROM uploaded_images WHERE capability = ?1 AND expires_at > ?2",
                        params![lookup_token, now],
                        |row| {
                            Ok(ImageRecord {
                                token: row.get(0)?,
                                content_type: row.get(1)?,
                                byte_length: row.get(2)?,
                                expires_at: row.get(3)?,
                            })
                        },
                    )
                    .optional()
            })
            .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        let Some(image_type) = ImageType::parse(&record.content_type) else {
            return Ok(None);
        };
        if record.expires_at <= now {
            return Ok(None);
        }
        let path = self.file_path(&record.token);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
            _ => return Ok(None),
        };
        if i64::try_from(metadata.len()).ok() != Some(record.byte_length) {
            return Ok(None);
        }
        let bytes = tokio::task::spawn_blocking(move || fs::read(path)).await??;
        if bytes.is_empty() || !image_type.matches_signature(&bytes) {
            return Ok(None);
        }
        Ok(Some((bytes, image_type.as_str())))
    }

    pub async fn cleanup_expired(&self) -> Result<()> {
        let now = unix_now()?;
        let records = self
            .store
            .database()
            .run(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT capability, content_type, byte_length, expires_at
                     FROM uploaded_images WHERE expires_at <= ?1",
                )?;
                let rows = statement.query_map([now], |row| {
                    Ok(ImageRecord {
                        token: row.get(0)?,
                        content_type: row.get(1)?,
                        byte_length: row.get(2)?,
                        expires_at: row.get(3)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        for record in records {
            self.remove_record(record.token).await?;
        }
        Ok(())
    }

    pub async fn cleanup_startup(&self) -> Result<()> {
        self.cleanup_expired().await?;
        let records = self
            .store
            .database()
            .run(|connection| {
                let mut statement = connection.prepare(
                    "SELECT capability, content_type, byte_length, expires_at FROM uploaded_images",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok(ImageRecord {
                        token: row.get(0)?,
                        content_type: row.get(1)?,
                        byte_length: row.get(2)?,
                        expires_at: row.get(3)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        let mut live = HashSet::new();
        for record in records {
            let path = self.file_path(&record.token);
            if fs::symlink_metadata(&path).is_ok_and(|metadata| {
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && i64::try_from(metadata.len()).ok() == Some(record.byte_length)
            }) {
                live.insert(format!("{}.img", record.token));
            } else {
                self.remove_record(record.token).await?;
            }
        }

        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".tmp") || (name.ends_with(".img") && !live.contains(name.as_ref())) {
                remove_file_if_present(&entry.path())?;
            }
        }
        Ok(())
    }

    async fn remove_record(&self, token: String) -> Result<()> {
        let file_path = self.file_path(&token);
        let temporary_path = self.temporary_path(&token);
        let database_token = token;
        remove_file_if_present(&file_path)?;
        remove_file_if_present(&temporary_path)?;
        self.store
            .database()
            .run(move |connection| {
                connection.execute(
                    "DELETE FROM uploaded_images WHERE capability = ?1",
                    [database_token],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    fn file_path(&self, token: &str) -> PathBuf {
        self.directory.join(format!("{token}.img"))
    }

    fn temporary_path(&self, token: &str) -> PathBuf {
        self.directory.join(format!(".{token}.tmp"))
    }
}

pub fn validate_upload(bytes: &[u8], image_type: ImageType) -> bool {
    !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES && image_type.matches_signature(bytes)
}

fn write_atomic(directory: &Path, token: &str, bytes: &[u8]) -> Result<()> {
    let temporary = directory.join(format!(".{token}.tmp"));
    let target = directory.join(format!("{token}.img"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create temporary image {}", temporary.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("write temporary image");
    }
    drop(file);
    fs::rename(&temporary, &target)
        .with_context(|| format!("publish image {}", target.display()))?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn valid_capability(token: &str) -> bool {
    if token.len() != 43 {
        return false;
    }
    URL_SAFE_NO_PAD
        .decode(token)
        .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(bytes) == token)
}

fn unix_now() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?)
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove image file {}", path.display())),
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set private permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn invalid_image() -> ImageError {
    ImageError {
        status: StatusCode::BAD_REQUEST,
        message: "The image is empty, too large, or does not match its declared type.",
    }
}

fn capacity_error() -> ImageError {
    ImageError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "Chime has reached its temporary image storage limit. Try again later.",
    }
}

fn internal_error() -> ImageError {
    ImageError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "Chime could not store or load this image.",
    }
}

#[cfg(test)]
mod tests {
    use super::{ImageStore, ImageType, MAX_IMAGE_BYTES, validate_upload};
    use crate::store::Store;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsmall png fixture";

    #[test]
    fn validates_only_supported_declared_image_signatures_and_bounds() {
        assert!(validate_upload(PNG, ImageType::Png));
        assert!(validate_upload(
            b"\xff\xd8\xffjpeg fixture",
            ImageType::Jpeg
        ));
        assert!(validate_upload(b"RIFFxxxxWEBPfixture", ImageType::Webp));
        assert!(!validate_upload(PNG, ImageType::Jpeg));
        assert!(!validate_upload(b"<svg>", ImageType::Png));
        assert!(!validate_upload(
            &vec![0; MAX_IMAGE_BYTES + 1],
            ImageType::Png
        ));
        assert!(ImageType::parse("image/svg+xml").is_none());
    }

    #[tokio::test]
    async fn uploaded_capability_survives_restart_and_expired_rows_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        let store = Store::open(&path).unwrap();
        let images = ImageStore::new(store.clone()).unwrap();
        let capability = images.capability("https://chime.example").unwrap();
        images
            .save(&capability, ImageType::Png, PNG.to_vec())
            .await
            .unwrap();
        let file = path
            .join("images")
            .join(format!("{}.img", capability.token));
        assert!(file.is_file());
        drop(images);
        drop(store);

        let restarted_store = Store::open(&path).unwrap();
        let restarted = ImageStore::new(restarted_store.clone()).unwrap();
        assert_eq!(
            restarted
                .load(capability.token.clone())
                .await
                .unwrap()
                .unwrap()
                .0,
            PNG
        );
        restarted_store
            .database()
            .run(|connection| {
                connection.execute(
                    "UPDATE uploaded_images SET expires_at = unixepoch() - 1",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        restarted.cleanup_expired().await.unwrap();
        assert!(!file.exists());
        assert!(restarted.load(capability.token).await.unwrap().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn image_directory_and_files_have_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("state")).unwrap();
        let images = ImageStore::new(store).unwrap();
        let capability = images.capability("https://chime.example").unwrap();
        images
            .save(&capability, ImageType::Png, PNG.to_vec())
            .await
            .unwrap();
        let directory_mode = std::fs::metadata(root.path().join("state/images"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(
            root.path()
                .join("state/images")
                .join(format!("{}.img", capability.token)),
        )
        .unwrap()
        .permissions()
        .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[tokio::test]
    async fn periodic_cleanup_keeps_live_reservations_and_startup_removes_abandoned_ones() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("state")).unwrap();
        let images = ImageStore::new(store.clone()).unwrap();
        let capability = images.capability("https://chime.example").unwrap();
        let token = capability.token.clone();
        store
            .database()
            .run(move |connection| {
                connection.execute(
                    "INSERT INTO uploaded_images (capability, content_type, byte_length, expires_at)
                     VALUES (?1, 'image/png', 10, unixepoch() + 86400)",
                    [token],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        images.cleanup_expired().await.unwrap();
        let still_reserved = store
            .database()
            .run(|connection| {
                connection.query_row("SELECT COUNT(*) FROM uploaded_images", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(still_reserved, 1);

        images.cleanup_startup().await.unwrap();
        let reclaimed = store
            .database()
            .run(|connection| {
                connection.query_row("SELECT COUNT(*) FROM uploaded_images", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(reclaimed, 0);
    }

    #[tokio::test]
    async fn cleanup_removes_orphan_temporary_files_and_reclaims_expired_quota() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("state")).unwrap();
        let images = ImageStore::new(store.clone()).unwrap();
        let full_tokens: Vec<String> = (1_u8..=52)
            .map(|byte| URL_SAFE_NO_PAD.encode([byte; 32]))
            .collect();
        for token in &full_tokens {
            std::fs::write(
                root.path()
                    .join("state/images")
                    .join(format!("{token}.img")),
                PNG,
            )
            .unwrap();
        }
        let insert_tokens = full_tokens.clone();
        store
            .database()
            .run(move |connection| {
                for token in insert_tokens {
                    connection.execute(
                        "INSERT INTO uploaded_images (capability, content_type, byte_length, expires_at)
                         VALUES (?1, 'image/png', 5242880, unixepoch() + 86400)",
                        [token],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let capability = images.capability("https://chime.example").unwrap();
        let full = images
            .save(&capability, ImageType::Png, PNG.to_vec())
            .await
            .unwrap_err();
        assert_eq!(full.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        let expired_tokens = full_tokens.clone();
        store
            .database()
            .run(move |connection| {
                for token in expired_tokens {
                    connection.execute(
                        "UPDATE uploaded_images SET expires_at = unixepoch() - 1 WHERE capability = ?1",
                        [token],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let temp = root
            .path()
            .join("state/images")
            .join(format!(".{}.tmp", capability.token));
        std::fs::write(&temp, b"partial").unwrap();
        images.cleanup_startup().await.unwrap();
        assert!(!temp.exists());
        for token in full_tokens {
            assert!(
                !root
                    .path()
                    .join("state/images")
                    .join(format!("{token}.img"))
                    .exists()
            );
        }
        images
            .save(&capability, ImageType::Png, PNG.to_vec())
            .await
            .unwrap();
    }
}
