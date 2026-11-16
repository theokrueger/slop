use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::PaneConfig;

const LEGACY_VERSION: u32 = 1;
const INDEX_VERSION: u32 = 1;
const PROJECT_VERSION: u32 = 1;
const PROJECTS_DIR: &str = "projects";
const INDEX_FILE: &str = "index.json";
const PROJECT_FILE: &str = "project.json";
const LEGACY_FILE: &str = "projects.json";
const LEGACY_BACKUP: &str = "projects.json.migrated";

/// Durable description of a project. Running processes cannot be serialized,
/// so restoring a project starts fresh panes from these definitions.
///
/// The ID names the project's directory and stays stable across renames,
/// reorders, and restarts. Future per-project state can live beside
/// `project.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: Uuid,
    pub name: String,
    pub cwd: String,
    pub panes: Vec<PaneConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectFile {
    version: u32,
    #[serde(flatten)]
    project: ProjectRecord,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    projects: Vec<Uuid>,
}

/// The pre-directory format, kept only for one-time migration.
#[derive(Debug, Deserialize)]
struct LegacyStateFile {
    version: u32,
    projects: Vec<LegacyProjectRecord>,
}

#[derive(Debug, Deserialize)]
struct LegacyProjectRecord {
    name: String,
    cwd: String,
    panes: Vec<PaneConfig>,
}

/// `$XDG_STATE_HOME/mmux`.
pub fn default_state_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "mmux")
        .and_then(|dirs| dirs.state_dir().map(Path::to_owned))
}

/// Loads projects in sidebar order, migrating the old single-file state once.
pub fn load(state_dir: &Path) -> Result<Vec<ProjectRecord>> {
    let index = state_dir.join(PROJECTS_DIR).join(INDEX_FILE);
    if index.exists() {
        return load_indexed(state_dir);
    }
    if state_dir.join(LEGACY_FILE).exists() {
        return migrate_legacy(state_dir);
    }
    Ok(Vec::new())
}

fn load_indexed(state_dir: &Path) -> Result<Vec<ProjectRecord>> {
    let index_path = state_dir.join(PROJECTS_DIR).join(INDEX_FILE);
    let text = fs::read_to_string(&index_path)
        .with_context(|| format!("reading project index {}", index_path.display()))?;
    let index: IndexFile = serde_json::from_str(&text)
        .with_context(|| format!("parsing project index {}", index_path.display()))?;
    if index.version != INDEX_VERSION {
        bail!(
            "unsupported project index version {} in {} (expected {INDEX_VERSION})",
            index.version,
            index_path.display()
        );
    }

    let mut seen = HashSet::new();
    let mut projects = Vec::with_capacity(index.projects.len());
    for id in index.projects {
        if !seen.insert(id) {
            bail!("duplicate project id {id} in {}", index_path.display());
        }
        let path = project_file(state_dir, id);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading project {}", path.display()))?;
        let file: ProjectFile = serde_json::from_str(&text)
            .with_context(|| format!("parsing project {}", path.display()))?;
        if file.version != PROJECT_VERSION {
            bail!(
                "unsupported project version {} in {} (expected {PROJECT_VERSION})",
                file.version,
                path.display()
            );
        }
        if file.project.id != id {
            bail!(
                "project id {} does not match directory for {}",
                file.project.id,
                path.display()
            );
        }
        projects.push(file.project);
    }
    Ok(projects)
}

fn migrate_legacy(state_dir: &Path) -> Result<Vec<ProjectRecord>> {
    let path = state_dir.join(LEGACY_FILE);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("reading legacy projects from {}", path.display()))?;
    let state: LegacyStateFile = serde_json::from_str(&text)
        .with_context(|| format!("parsing legacy projects from {}", path.display()))?;
    if state.version != LEGACY_VERSION {
        bail!(
            "unsupported legacy projects version {} in {} (expected {LEGACY_VERSION})",
            state.version,
            path.display()
        );
    }

    let projects: Vec<ProjectRecord> = state
        .projects
        .into_iter()
        .map(|project| ProjectRecord {
            id: Uuid::new_v4(),
            name: project.name,
            cwd: project.cwd,
            panes: project.panes,
        })
        .collect();
    save(state_dir, &projects)?;

    // Keep the old file for recovery, but do not let a future load prefer it.
    let mut backup = state_dir.join(LEGACY_BACKUP);
    let mut suffix = 1;
    while backup.exists() {
        backup = state_dir.join(format!("{LEGACY_BACKUP}.{suffix}"));
        suffix += 1;
    }
    fs::rename(&path, &backup)
        .with_context(|| format!("renaming {} to {}", path.display(), backup.display()))?;
    tracing::info!(
        projects = projects.len(),
        state = %state_dir.display(),
        "migrated projects to per-project directories"
    );
    Ok(projects)
}

/// Saves projects in the given order. Unlisted project directories are left
/// untouched: callers remove a project explicitly after the new index is
/// durable, so a partial or failed load can never delete unrelated projects.
pub fn save(state_dir: &Path, projects: &[ProjectRecord]) -> Result<()> {
    let mut seen = HashSet::new();
    for project in projects {
        if !seen.insert(project.id) {
            bail!("duplicate project id {}", project.id);
        }
    }

    let projects_dir = state_dir.join(PROJECTS_DIR);
    create_private_dir(&projects_dir)?;

    for project in projects {
        let dir = project_dir(state_dir, project.id);
        create_private_dir(&dir)?;
        let file = ProjectFile {
            version: PROJECT_VERSION,
            project: project.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).context("serializing project")?;
        write_atomic(&dir.join(PROJECT_FILE), &bytes)?;
    }

    let index = IndexFile {
        version: INDEX_VERSION,
        projects: projects.iter().map(|project| project.id).collect(),
    };
    let bytes = serde_json::to_vec_pretty(&index).context("serializing project index")?;
    write_atomic(&projects_dir.join(INDEX_FILE), &bytes)?;
    Ok(())
}

/// Removes a closed project's directory. Call only after an index that no
/// longer references the project has been saved.
pub fn delete_project(state_dir: &Path, id: Uuid) -> Result<()> {
    let dir = project_dir(state_dir, id);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", dir.display())),
    }
}

/// Directory holding `project.json` and any future per-project state.
pub fn project_dir(state_dir: &Path, id: Uuid) -> PathBuf {
    state_dir.join(PROJECTS_DIR).join(id.to_string())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn project_file(state_dir: &Path, id: Uuid) -> PathBuf {
    project_dir(state_dir, id).join(PROJECT_FILE)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
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
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing {}", temporary.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
    }
    fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("syncing directory {}", parent.display()))?;
    }
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
            id: Uuid::new_v4(),
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
        assert!(load(&dir.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn round_trips_projects_in_order_with_stable_ids() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        let cwd = dir.path().join("workspace");
        let projects = vec![record("api", &cwd), record("web", &cwd)];
        save(&state, &projects).unwrap();

        let loaded = load(&state).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, projects[0].id);
        assert_eq!(loaded[1].id, projects[1].id);
        assert_eq!(loaded[0].name, "api");
        assert_eq!(loaded[0].cwd, cwd.to_string_lossy().as_ref());
        assert_eq!(loaded[0].panes.len(), 1);
        for project in &projects {
            assert!(
                state
                    .join(PROJECTS_DIR)
                    .join(project.id.to_string())
                    .join(PROJECT_FILE)
                    .is_file()
            );
        }
        assert!(state.join(PROJECTS_DIR).join(INDEX_FILE).is_file());
    }

    #[test]
    fn saving_preserves_unlisted_directories_until_deleted() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        let cwd = dir.path().join("workspace");
        let projects = vec![record("api", &cwd), record("web", &cwd)];
        save(&state, &projects).unwrap();
        let removed = project_dir(&state, projects[1].id);
        assert!(removed.is_dir());

        save(&state, &projects[..1]).unwrap();
        assert!(removed.is_dir(), "save must not prune unlisted projects");
        assert_eq!(load(&state).unwrap()[0].name, "api");

        delete_project(&state, projects[1].id).unwrap();
        delete_project(&state, projects[1].id).unwrap();
        assert!(!removed.exists());
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("workspace");
        let project = record("api", &cwd);
        assert!(save(&dir.path().join("state"), &[project.clone(), project]).is_err());
    }

    #[test]
    fn legacy_state_is_migrated_in_order() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join(LEGACY_FILE),
            r#"{"version":1,"projects":[
                {"name":"api","cwd":"/tmp/api","panes":[]},
                {"name":"web","cwd":"/tmp/web","panes":[]}
            ]}"#,
        )
        .unwrap();

        let projects = load(&state).unwrap();
        assert_eq!(
            projects
                .iter()
                .map(|project| project.name.as_str())
                .collect::<Vec<_>>(),
            vec!["api", "web"]
        );
        assert_ne!(projects[0].id, projects[1].id);
        assert!(state.join(LEGACY_BACKUP).is_file());
        assert!(!state.join(LEGACY_FILE).exists());
        assert_eq!(load(&state).unwrap()[1].name, "web");
    }

    #[test]
    fn corrupt_index_is_an_error() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        let projects_dir = state.join(PROJECTS_DIR);
        fs::create_dir_all(&projects_dir).unwrap();
        fs::write(projects_dir.join(INDEX_FILE), "not json").unwrap();
        assert!(load(&state).is_err());
    }

    #[test]
    fn corrupt_project_is_an_error() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        let project = record("api", &dir.path().join("workspace"));
        save(&state, std::slice::from_ref(&project)).unwrap();
        fs::write(project_file(&state, project.id), "not json").unwrap();
        assert!(load(&state).is_err());
    }

    #[test]
    fn mismatched_project_metadata_is_an_error() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        let project = record("api", &dir.path().join("workspace"));
        save(&state, std::slice::from_ref(&project)).unwrap();
        let path = project_file(&state, project.id);
        let mut file: ProjectFile =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        file.project.id = Uuid::new_v4();
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(load(&state).is_err());
    }

    #[test]
    fn unsupported_versions_are_errors() {
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("state");
        let projects_dir = state.join(PROJECTS_DIR);
        fs::create_dir_all(&projects_dir).unwrap();
        fs::write(
            projects_dir.join(INDEX_FILE),
            r#"{"version":99,"projects":[]}"#,
        )
        .unwrap();
        assert!(load(&state).is_err());

        let legacy = dir.path().join("legacy");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(LEGACY_FILE), r#"{"version":99,"projects":[]}"#).unwrap();
        assert!(load(&legacy).is_err());
    }
}
