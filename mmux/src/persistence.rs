use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::PaneConfig;

const VERSION: u32 = 1;

/// Durable description of a project. Running processes cannot be serialized,
/// so restoring a project starts fresh panes from these definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub name: String,
    pub cwd: String,
    pub panes: Vec<PaneConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    projects: Vec<ProjectRecord>,
}

/// `$XDG_STATE_HOME/mmux/projects.json`.
pub fn default_state_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "mmux")
        .and_then(|dirs| dirs.state_dir().map(|dir| dir.join("projects.json")))
}

pub fn load(path: &Path) -> Result<Vec<ProjectRecord>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("reading projects from {}", path.display()));
        }
    };
    let state: StateFile = serde_json::from_str(&text)
        .with_context(|| format!("parsing projects from {}", path.display()))?;
    if state.version != VERSION {
        bail!(
            "unsupported projects state version {} in {} (expected {VERSION})",
            state.version,
            path.display()
        );
    }
    Ok(state.projects)
}

pub fn save(path: &Path, projects: &[ProjectRecord]) -> Result<()> {
    let state = StateFile {
        version: VERSION,
        projects: projects.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&state).context("serializing projects")?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating state directory {}", parent.display()))?;
    }

    let temporary = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("opening {}", temporary.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&temporary, &bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
    }
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Slot;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn record(name: &str, cwd: &Path) -> ProjectRecord {
        ProjectRecord {
            name: name.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            panes: vec![PaneConfig {
                slot: Slot::TopLeft,
                command: vec!["sh".into()],
                env: BTreeMap::new(),
            }],
        }
    }

    #[test]
    fn missing_state_is_empty() {
        let dir = TempDir::new().unwrap();
        assert!(load(&dir.path().join("missing.json")).unwrap().is_empty());
    }

    #[test]
    fn round_trips_projects() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state/projects.json");
        let cwd = dir.path().join("workspace");
        let projects = vec![record("api", &cwd), record("web", &cwd)];
        save(&path, &projects).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].name, "api");
        assert_eq!(loaded[0].cwd, cwd.to_string_lossy().as_ref());
        assert_eq!(loaded[0].panes.len(), 1);
    }

    #[test]
    fn corrupt_state_is_an_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.json");
        fs::write(&path, "not json").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn unsupported_version_is_an_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.json");
        fs::write(&path, r#"{"version":99,"projects":[]}"#).unwrap();
        assert!(load(&path).is_err());
    }
}
