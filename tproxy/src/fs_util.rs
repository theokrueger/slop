//! Filesystem helpers shared by the config and usage stores.

use anyhow::{Context, Result};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Create the parent directory of `path` when it is missing.
///
/// Newly created directories are private to the current user so contents are
/// not enumerable by other accounts; existing directories are left untouched.
pub fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    Ok(())
}

/// Replace `path` with `data` atomically using a private temporary file.
///
/// The temporary file is written with mode 0600, synced to disk, and renamed
/// over `path`, so a crash leaves either the previous or the new contents.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    // Follow an existing symlink so the link itself is preserved.
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    create_parent(&path)?;
    let tmp = temp_path(&path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("open {}", tmp.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set permissions on {}", tmp.display()))?;
    file.write_all(data)
        .with_context(|| format!("write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    tmp.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn writes_atomically_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn creates_private_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/data.json");
        write_atomic(&path, b"data").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"data");
        let mode = fs::metadata(dir.path().join("nested/deeper"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn preserves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        let link = dir.path().join("link.json");
        fs::write(&target, b"old").unwrap();
        symlink(&target, &link).unwrap();

        write_atomic(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }
}
