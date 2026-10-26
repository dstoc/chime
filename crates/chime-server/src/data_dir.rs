use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const LOCK_FILE: &str = ".chime.lock";

pub struct DataDirLock {
    _file: File,
}

impl DataDirLock {
    pub fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(LOCK_FILE);
        reject_symlink_or_non_file(&path)?;

        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .with_context(|| format!("open data directory lock {}", path.display()))?;
        set_private_file_permissions(&path)?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                bail!("data directory is already in use: {}", directory.display())
            }
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("lock data directory {}", directory.display()))
            }
        }
    }
}

pub fn prepare_private_directory(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("data directory path must not be empty");
    }

    fs::create_dir_all(path)
        .with_context(|| format!("create data directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect data directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("data directory must be a directory and must not be a symbolic link");
    }

    set_private_directory_permissions(path)?;
    fs::canonicalize(path).with_context(|| format!("resolve data directory {}", path.display()))
}

pub fn reject_symlink_or_non_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!(
                "{} must be a regular file and must not be a symbolic link",
                path.display()
            )
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
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

#[cfg(unix)]
pub fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set private permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DataDirLock, prepare_private_directory};
    use anyhow::Result;

    #[test]
    fn a_directory_has_one_exclusive_lock_owner() -> Result<()> {
        let root = tempfile::tempdir()?;
        let directory = prepare_private_directory(&root.path().join("state"))?;
        let _lock = DataDirLock::acquire(&directory)?;

        assert!(DataDirLock::acquire(&directory).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn data_directory_and_lock_file_are_private() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir()?;
        let directory = prepare_private_directory(&root.path().join("state"))?;
        let _lock = DataDirLock::acquire(&directory)?;

        assert_eq!(
            std::fs::metadata(&directory)?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(directory.join(".chime.lock"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        Ok(())
    }
}
