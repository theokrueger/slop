use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// A directory browser confined to a configured root.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirListing {
    pub root: PathBuf,
    pub path: PathBuf,
    /// `None` at the workspace root.
    pub parent: Option<PathBuf>,
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("path does not exist")]
    NotFound,
    #[error("path is not a directory")]
    NotADirectory,
    #[error("path is outside the workspace root")]
    OutsideRoot,
    #[error("permission denied")]
    PermissionDenied,
    #[error("could not inspect path: {0}")]
    Io(#[from] io::Error),
}

impl Workspace {
    /// Creates a workspace rooted at `root`. `~` is expanded first.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = expand_tilde(root.as_ref());
        let root = fs::canonicalize(&root).map_err(map_io)?;
        if !root.is_dir() {
            return Err(WorkspaceError::NotADirectory);
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a user-supplied path and verifies it is a directory inside
    /// the workspace root. Relative paths are interpreted from the root.
    pub fn resolve(&self, requested: &Path) -> Result<PathBuf, WorkspaceError> {
        let candidate = if requested.as_os_str().is_empty() {
            self.root.clone()
        } else if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };

        let resolved = fs::canonicalize(&candidate).map_err(map_io)?;
        if !resolved.starts_with(&self.root) {
            return Err(WorkspaceError::OutsideRoot);
        }
        if !resolved.is_dir() {
            return Err(WorkspaceError::NotADirectory);
        }
        Ok(resolved)
    }

    /// Lists the immediate subdirectories of `requested` (defaults to root).
    /// Symlinks are followed only when their target stays inside the root.
    pub fn list(&self, requested: Option<&Path>) -> Result<DirListing, WorkspaceError> {
        let path = self.resolve(requested.unwrap_or(Path::new("")))?;

        let mut entries = Vec::new();
        for entry in fs::read_dir(&path).map_err(map_io)? {
            let Ok(entry) = entry else { continue };
            // Follow symlinks here; canonicalization below keeps the
            // navigation confined to the workspace root.
            let Ok(metadata) = fs::metadata(entry.path()) else {
                continue;
            };
            if !metadata.is_dir() {
                continue;
            }
            let Ok(resolved) = fs::canonicalize(entry.path()) else {
                continue;
            };
            if !resolved.starts_with(&self.root) {
                continue;
            }
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: resolved,
            });
        }
        entries.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });

        let parent = if path == self.root {
            None
        } else {
            path.parent()
                .filter(|parent| parent.starts_with(&self.root))
                .map(Path::to_path_buf)
        };

        Ok(DirListing {
            root: self.root.clone(),
            path,
            parent,
            entries,
        })
    }
}

/// Expands a leading `~` without invoking the shell.
pub fn expand_tilde(path: &Path) -> PathBuf {
    match path.to_str() {
        Some(value) if value.starts_with('~') => {
            PathBuf::from(shellexpand::tilde(value).into_owned())
        }
        _ => path.to_path_buf(),
    }
}

fn map_io(err: io::Error) -> WorkspaceError {
    match err.kind() {
        io::ErrorKind::NotFound => WorkspaceError::NotFound,
        io::ErrorKind::PermissionDenied => WorkspaceError::PermissionDenied,
        io::ErrorKind::NotADirectory => WorkspaceError::NotADirectory,
        _ => WorkspaceError::Io(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Workspace) {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join("alpha")).unwrap();
        fs::create_dir(temp.path().join("zeta")).unwrap();
        fs::create_dir(temp.path().join("zeta/nested")).unwrap();
        fs::write(temp.path().join("file.txt"), "not a directory").unwrap();
        let workspace = Workspace::new(temp.path()).unwrap();
        (temp, workspace)
    }

    #[test]
    fn lists_directories_sorted_and_skips_files() {
        let (_temp, workspace) = fixture();
        let listing = workspace.list(None).unwrap();
        assert_eq!(listing.path, workspace.root());
        assert_eq!(listing.parent, None);
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn resolves_relative_and_absolute_paths() {
        let (_temp, workspace) = fixture();
        let relative = workspace.resolve(Path::new("zeta/nested")).unwrap();
        assert!(relative.ends_with("zeta/nested"));
        let absolute = workspace.resolve(&relative).unwrap();
        assert_eq!(relative, absolute);
    }

    #[test]
    fn rejects_escape_via_parent() {
        let (_temp, workspace) = fixture();
        let err = workspace.resolve(Path::new("..")).unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideRoot));
    }

    #[test]
    fn rejects_files_and_missing_paths() {
        let (_temp, workspace) = fixture();
        assert!(matches!(
            workspace.resolve(Path::new("file.txt")).unwrap_err(),
            WorkspaceError::NotADirectory
        ));
        assert!(matches!(
            workspace.resolve(Path::new("missing")).unwrap_err(),
            WorkspaceError::NotFound
        ));
    }

    #[cfg(unix)]
    #[test]
    fn excludes_symlinks_pointing_outside_the_root() {
        let (_temp, workspace) = fixture();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.root().join("escape")).unwrap();
        let listing = workspace.list(None).unwrap();
        assert!(!listing.entries.iter().any(|entry| entry.name == "escape"));
        assert!(matches!(
            workspace.resolve(Path::new("escape")).unwrap_err(),
            WorkspaceError::OutsideRoot
        ));
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_that_stay_inside_the_root() {
        let (_temp, workspace) = fixture();
        std::os::unix::fs::symlink(
            workspace.root().join("alpha"),
            workspace.root().join("alias"),
        )
        .unwrap();
        let listing = workspace.list(None).unwrap();
        let alias = listing
            .entries
            .iter()
            .find(|entry| entry.name == "alias")
            .expect("alias should be listed");
        assert!(alias.path.ends_with("alpha"));
    }

    #[test]
    fn parent_stops_at_root() {
        let (_temp, workspace) = fixture();
        let nested = workspace.list(Some(Path::new("zeta/nested"))).unwrap();
        assert_eq!(
            nested.parent.as_deref(),
            Some(workspace.root().join("zeta").as_path())
        );
        let root = workspace.list(None).unwrap();
        assert_eq!(root.parent, None);
    }

    #[test]
    fn expands_tilde() {
        let home = directories::UserDirs::new()
            .unwrap()
            .home_dir()
            .to_path_buf();
        assert_eq!(expand_tilde(Path::new("~")), home);
        assert_eq!(expand_tilde(Path::new("~/code")), home.join("code"));
        assert_eq!(expand_tilde(Path::new("/tmp")), PathBuf::from("/tmp"));
    }
}
