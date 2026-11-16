pub mod pane;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, broadcast};
use uuid::Uuid;

use crate::config::{PaneConfig, Slot, validate_label};
use crate::persistence::{self, ProjectRecord};
use crate::theme::Appearance;
use pane::{Pane, PaneSpec, PaneStatusView, resolve_command};

/// A project identifier. Unique for the lifetime of the mmux server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(u64);

/// A pane identifier. Unique for the lifetime of the mmux server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(u64);

impl ProjectId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl PaneId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Broadcast whenever the shape of the session changes. Clients re-fetch the
/// authoritative state in response, which keeps the protocol idempotent.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    ProjectsChanged,
    PaneExited {
        project_id: ProjectId,
        pane_id: PaneId,
    },
    PaneRestarted {
        project_id: ProjectId,
        pane_id: PaneId,
    },
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("invalid project name: {0}")]
    InvalidName(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Serialized view of a project. IDs are opaque to the browser.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectView {
    pub id: ProjectId,
    pub name: String,
    pub cwd: String,
    pub panes: Vec<PaneView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaneView {
    pub id: PaneId,
    pub slot: Slot,
    pub status: PaneStatusView,
    pub command: String,
}

/// Owns all projects and panes. Dropping browser connections never touches this.
pub struct SessionStore {
    template: Vec<PaneConfig>,
    default_size: (u16, u16),
    appearance: Appearance,
    projects: RwLock<Vec<Project>>,
    events: broadcast::Sender<SessionEvent>,
    next_project: AtomicU64,
    next_pane: AtomicU64,
    state_dir: Option<PathBuf>,
    /// False after a failed load, so a partial or corrupt state is never
    /// overwritten by a save based on an incomplete in-memory picture.
    state_trusted: AtomicBool,
    /// Records that failed to start. Keep them in the index so a later restart
    /// can retry instead of losing the project definition.
    unrestored: Mutex<Vec<ProjectRecord>>,
    persist_lock: Mutex<()>,
    /// Static identity variables handed to every pane (currently `MMUX_CONFIG`
    /// when the server was started with a non-default config file). Settings
    /// never travel through the environment.
    identity_env: BTreeMap<String, String>,
}

struct Project {
    id: ProjectId,
    /// Stable identity for the project's persistence directory.
    storage_id: Uuid,
    name: String,
    cwd: PathBuf,
    panes: Vec<Arc<Pane>>,
    /// Pane definitions used to create this project; replayed after restart.
    pane_configs: Vec<PaneConfig>,
}

impl SessionStore {
    /// In-memory store, used by tests and by callers without persistence.
    pub fn new(template: Vec<PaneConfig>, default_size: (u16, u16)) -> Self {
        Self::build(template, default_size, None)
    }

    /// Store that saves projects under `state_dir` and can restore them.
    pub fn with_state_dir(
        template: Vec<PaneConfig>,
        default_size: (u16, u16),
        state_dir: PathBuf,
    ) -> Self {
        Self::build(template, default_size, Some(state_dir))
    }

    fn build(
        template: Vec<PaneConfig>,
        default_size: (u16, u16),
        state_dir: Option<PathBuf>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            template,
            default_size,
            appearance: Appearance::default(),
            projects: RwLock::new(Vec::new()),
            events,
            next_project: AtomicU64::new(1),
            next_pane: AtomicU64::new(1),
            state_dir,
            state_trusted: AtomicBool::new(true),
            unrestored: Mutex::new(Vec::new()),
            persist_lock: Mutex::new(()),
            identity_env: std::collections::BTreeMap::new(),
        }
    }

    /// Adds identity variables (not settings) to every pane this store spawns.
    pub fn with_identity_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.identity_env = env;
        self
    }

    pub fn with_appearance(mut self, appearance: Appearance) -> Self {
        self.appearance = appearance;
        self
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Restores persisted projects before the server starts accepting clients.
    /// Returns the number of projects restored.
    pub async fn restore(&self) -> usize {
        let Some(state_dir) = self.state_dir.as_ref() else {
            return 0;
        };
        let records = match persistence::load(state_dir) {
            Ok(records) => records,
            Err(err) => {
                self.state_trusted.store(false, Ordering::Relaxed);
                tracing::error!(
                    error = %err,
                    state = %state_dir.display(),
                    "could not restore projects; refusing to overwrite state"
                );
                return 0;
            }
        };
        self.state_trusted.store(true, Ordering::Relaxed);

        let mut restored = 0;
        let mut needs_save = false;
        for record in records {
            let retry = record.clone();
            match self.restore_project(record).await {
                Ok(resolved_sessions) => {
                    restored += 1;
                    needs_save |= resolved_sessions;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "could not restore saved project; keeping it");
                    self.unrestored.lock().await.push(retry);
                    needs_save = true;
                }
            }
        }
        if needs_save {
            // Older state may still contain placeholders; persist the resolved
            // commands so the same session IDs are used next time.
            self.persist().await;
        }
        if restored > 0 {
            tracing::info!(restored, state = %state_dir.display(), "restored projects");
        }
        restored
    }

    /// Returns whether any session placeholders were resolved.
    async fn restore_project(&self, record: ProjectRecord) -> Result<bool, SessionError> {
        let cwd = PathBuf::from(&record.cwd);
        let name = if validate_label(&record.name).is_ok() {
            record.name.trim().to_owned()
        } else {
            default_project_name(&cwd)
        };
        let panes = resolve_session_ids(&record.panes);
        let resolved_sessions = panes != record.panes;
        self.spawn_project(record.id, name, cwd, &panes).await?;
        Ok(resolved_sessions)
    }

    /// Creates a project in `cwd`, spawning the configured template panes.
    pub async fn create_project(
        &self,
        name: Option<String>,
        cwd: PathBuf,
    ) -> Result<ProjectView, SessionError> {
        let name = resolve_project_name(name, &cwd)?;
        let panes = resolve_session_ids(&self.template);
        let view = self
            .spawn_project(Uuid::new_v4(), name, cwd, &panes)
            .await?;
        self.persist().await;
        Ok(view)
    }

    async fn spawn_project(
        &self,
        storage_id: Uuid,
        name: String,
        cwd: PathBuf,
        pane_configs: &[PaneConfig],
    ) -> Result<ProjectView, SessionError> {
        let project_id = ProjectId::new(self.next_project.fetch_add(1, Ordering::Relaxed));

        // Resolve every command before spawning anything, so a bad template
        // cannot leave a partially-created project behind.
        let specs: Vec<(Slot, PaneSpec)> = pane_configs
            .iter()
            .map(|config| {
                let command = resolve_command(&config.command)?;
                let mut env =
                    self.pane_identity_env(project_id, &name, &cwd, config.slot, &command);
                // Explicit per-pane configuration wins over mmux defaults.
                env.extend(config.env.clone());
                Ok((
                    config.slot,
                    PaneSpec {
                        command,
                        env,
                        cwd: cwd.clone(),
                        appearance: self.appearance,
                    },
                ))
            })
            .collect::<anyhow::Result<_>>()?;

        let mut panes = Vec::with_capacity(specs.len());
        for (slot, spec) in specs {
            let pane_id = PaneId::new(self.next_pane.fetch_add(1, Ordering::Relaxed));
            panes.push(Pane::spawn(
                pane_id,
                project_id,
                slot,
                spec,
                self.default_size,
                self.events.clone(),
            ));
        }

        let project = Project {
            id: project_id,
            storage_id,
            name,
            cwd,
            panes,
            pane_configs: pane_configs.to_vec(),
        };
        let view = project.view();
        self.projects.write().await.push(project);
        self.broadcast();
        Ok(view)
    }

    /// Identity, not configuration: these tell a child which pane it is in and
    /// which agent is running, and every one of them degrades to a fallback.
    fn pane_identity_env(
        &self,
        project_id: ProjectId,
        name: &str,
        cwd: &Path,
        slot: Slot,
        command: &[String],
    ) -> BTreeMap<String, String> {
        let mut env = self.identity_env.clone();
        env.insert("MMUX_PROJECT_ID".into(), project_id.get().to_string());
        env.insert("MMUX_PROJECT_NAME".into(), name.to_owned());
        env.insert(
            "MMUX_PROJECT_CWD".into(),
            cwd.to_string_lossy().into_owned(),
        );
        env.insert("MMUX_PANE_SLOT".into(), slot.to_string());
        if let Some(session) = session_id_in(command) {
            env.insert("MMUX_SESSION_ID".into(), session);
        }
        if command
            .first()
            .and_then(|program| Path::new(program).file_name())
            .and_then(|name| name.to_str())
            == Some("pi")
        {
            env.insert("MMUX_AGENT".into(), "pi".into());
        }
        env
    }

    pub async fn rename_project(&self, id: ProjectId, name: &str) -> Result<bool, SessionError> {
        let name = name.trim();
        validate_label(name).map_err(SessionError::InvalidName)?;
        let mut changed = false;
        {
            let mut projects = self.projects.write().await;
            let Some(project) = projects.iter_mut().find(|project| project.id == id) else {
                return Ok(false);
            };
            if project.name != name {
                project.name = name.to_owned();
                changed = true;
            }
        }
        if changed {
            self.broadcast();
            self.persist().await;
        }
        Ok(true)
    }

    /// Reorders projects without restarting their panes. The order must be an
    /// exact permutation of the open projects so stale requests cannot drop
    /// projects created by another client.
    pub async fn reorder_projects(&self, ids: &[ProjectId]) -> bool {
        let mut projects = self.projects.write().await;
        let valid = ids.len() == projects.len()
            && ids.iter().enumerate().all(|(index, id)| {
                !ids[..index].contains(id) && projects.iter().any(|project| project.id == *id)
            });
        if !valid {
            return false;
        }
        if projects
            .iter()
            .map(|project| project.id)
            .eq(ids.iter().copied())
        {
            return true;
        }

        let mut reordered = Vec::with_capacity(projects.len());
        for id in ids {
            let index = projects
                .iter()
                .position(|project| project.id == *id)
                .expect("validated project order");
            reordered.push(projects.remove(index));
        }
        *projects = reordered;
        drop(projects);
        self.broadcast();
        self.persist().await;
        true
    }

    /// Closes a project: panes stop and the project moves to the archive.
    /// Closing is never destructive, and it never touches workspace memory.
    pub async fn archive_project(&self, id: ProjectId) -> bool {
        let (removed, position) = {
            let mut projects = self.projects.write().await;
            let Some(index) = projects.iter().position(|project| project.id == id) else {
                return false;
            };
            let position = index;
            (projects.remove(index), position)
        };
        for pane in &removed.panes {
            pane.terminate();
        }
        self.broadcast();
        // Persist the live index first: if the move fails, the directory is
        // still on disk and the archive scan will find it next time.
        if self.persist().await
            && let Some(state_dir) = self.state_dir.clone()
        {
            let record = removed.record();
            let archived = tokio::task::spawn_blocking(move || {
                persistence::archive(&state_dir, &record, position)
            })
            .await;
            match archived {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(error = %err, "could not move closed project to the archive");
                }
                Err(err) => {
                    tracing::warn!(error = %err, "archive task failed");
                }
            }
        }
        true
    }

    /// Archived projects, newest first. Empty when persistence is disabled.
    pub async fn archived_projects(&self) -> anyhow::Result<Vec<persistence::ArchivedProject>> {
        let Some(state_dir) = self.state_dir.clone() else {
            return Ok(Vec::new());
        };
        tokio::task::spawn_blocking(move || persistence::load_archive(&state_dir))
            .await
            .map_err(|err| anyhow::anyhow!("archive read task failed: {err}"))?
    }

    /// Permanently removes an archived project. Workspace memory is untouched.
    pub async fn delete_archived(&self, id: Uuid) -> anyhow::Result<()> {
        let Some(state_dir) = self.state_dir.clone() else {
            anyhow::bail!("project persistence is disabled");
        };
        tokio::task::spawn_blocking(move || persistence::delete_archived(&state_dir, id))
            .await
            .map_err(|err| anyhow::anyhow!("archive cleanup task failed: {err}"))?
    }

    /// Moves an archived project back and starts its panes from the saved
    /// definitions, reusing the same storage UUID.
    pub async fn restore_archived(&self, id: Uuid) -> Result<ProjectView, SessionError> {
        if self
            .projects
            .read()
            .await
            .iter()
            .any(|project| project.storage_id == id)
        {
            return Err(SessionError::Conflict("project is already open".into()));
        }
        let Some(state_dir) = self.state_dir.clone() else {
            return Err(SessionError::Other(anyhow::anyhow!(
                "project persistence is disabled"
            )));
        };
        let record =
            tokio::task::spawn_blocking(move || persistence::restore_archived(&state_dir, id))
                .await
                .map_err(|err| SessionError::Other(err.into()))?
                .map_err(SessionError::Other)?;

        let cwd = PathBuf::from(&record.cwd);
        let name = if validate_label(&record.name).is_ok() {
            record.name.trim().to_owned()
        } else {
            default_project_name(&cwd)
        };
        // Stored pane definitions normally already carry resolved session IDs;
        // resolve any placeholders defensively so an older record still resumes
        // instead of passing a literal `{session_id}` to the program.
        let panes = resolve_session_ids(&record.panes);
        let view = self.spawn_project(record.id, name, cwd, &panes).await?;
        self.persist().await;
        Ok(view)
    }

    pub async fn snapshot(&self) -> Vec<ProjectView> {
        let projects = self.projects.read().await;
        projects.iter().map(Project::view).collect()
    }

    pub async fn pane(&self, id: PaneId) -> Option<Arc<Pane>> {
        let projects = self.projects.read().await;
        projects
            .iter()
            .flat_map(|project| project.panes.iter())
            .find(|pane| pane.id() == id)
            .cloned()
    }

    pub async fn project_count(&self) -> usize {
        self.projects.read().await.len()
    }

    /// Saves project definitions after a change. Errors are logged, not
    /// propagated: losing durability must not break the live session. Returns
    /// whether the on-disk index now matches the live projects.
    async fn persist(&self) -> bool {
        let Some(state_dir) = self.state_dir.as_ref() else {
            return false;
        };
        if !self.state_trusted.load(Ordering::Relaxed) {
            tracing::warn!(
                state = %state_dir.display(),
                "skipping save because the saved state could not be loaded"
            );
            return false;
        }
        let state_dir = state_dir.clone();
        let state_display = state_dir.display().to_string();
        let _guard = self.persist_lock.lock().await;
        let mut records: Vec<ProjectRecord> = {
            let projects = self.projects.read().await;
            projects.iter().map(Project::record).collect()
        };
        records.extend(self.unrestored.lock().await.iter().cloned());
        let result =
            tokio::task::spawn_blocking(move || persistence::save(&state_dir, &records)).await;
        match result {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                tracing::warn!(error = %err, state = %state_display, "could not save projects");
                false
            }
            Err(err) => {
                tracing::warn!(error = %err, "project persistence task failed");
                false
            }
        }
    }

    /// Saves metadata and terminates panes for a clean process shutdown.
    pub async fn shutdown(&self) {
        self.persist().await;
        let panes: Vec<Arc<Pane>> = {
            let projects = self.projects.read().await;
            projects
                .iter()
                .flat_map(|project| project.panes.iter().cloned())
                .collect()
        };
        for pane in &panes {
            pane.terminate();
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1200);
        while tokio::time::Instant::now() < deadline && panes.iter().any(|pane| pane.is_running()) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        for pane in &panes {
            pane.kill();
        }
    }

    fn broadcast(&self) {
        let _ = self.events.send(SessionEvent::ProjectsChanged);
    }
}

impl Project {
    fn view(&self) -> ProjectView {
        let mut panes: Vec<_> = self.panes.iter().map(|pane| pane.view()).collect();
        panes.sort_by_key(|pane| pane.slot);
        ProjectView {
            id: self.id,
            name: self.name.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            panes,
        }
    }

    fn record(&self) -> ProjectRecord {
        ProjectRecord {
            id: self.storage_id,
            name: self.name.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            panes: self.pane_configs.clone(),
        }
    }
}

impl Pane {
    fn view(&self) -> PaneView {
        let command = if self.command().is_empty() {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        } else {
            self.command().join(" ")
        };
        PaneView {
            id: self.id(),
            slot: self.slot(),
            status: self.status().view(),
            command,
        }
    }
}

/// Replaces `{session_id}` in pane commands with a fresh UUID. A pane gets one
/// ID per project creation, and the resolved command is persisted with the
/// project, so programs such as pi can resume the same session after a restart.
fn resolve_session_ids(panes: &[PaneConfig]) -> Vec<PaneConfig> {
    panes
        .iter()
        .map(|pane| {
            if !pane
                .command
                .iter()
                .any(|arg| arg.contains(SESSION_ID_PLACEHOLDER))
            {
                return pane.clone();
            }
            let id = Uuid::new_v4().to_string();
            let mut resolved = pane.clone();
            for arg in &mut resolved.command {
                *arg = arg.replace(SESSION_ID_PLACEHOLDER, &id);
            }
            resolved
        })
        .collect()
}

pub const SESSION_ID_PLACEHOLDER: &str = "{session_id}";

/// The resolved `--session-id` a pane command carries, if any.
fn session_id_in(command: &[String]) -> Option<String> {
    command
        .iter()
        .position(|arg| arg == "--session-id")
        .and_then(|index| command.get(index + 1))
        .cloned()
}

fn resolve_project_name(name: Option<String>, cwd: &Path) -> Result<String, SessionError> {
    if let Some(name) = name {
        let name = name.trim();
        if !name.is_empty() {
            validate_label(name).map_err(SessionError::InvalidName)?;
            return Ok(name.to_owned());
        }
    }
    Ok(default_project_name(cwd))
}

fn default_project_name(cwd: &Path) -> String {
    let raw = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let sanitized: String = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(80)
        .collect();
    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        "workspace".to_owned()
    } else {
        sanitized.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::pane::PaneStatus;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    fn template(command: Vec<&str>) -> Vec<PaneConfig> {
        vec![PaneConfig {
            slot: Slot::TopLeft,
            command: command.into_iter().map(str::to_owned).collect(),
            env: BTreeMap::new(),
        }]
    }

    fn store(command: Vec<&str>) -> SessionStore {
        SessionStore::new(template(command), (24, 80))
    }

    async fn wait_for(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        predicate()
    }

    #[tokio::test]
    async fn starts_with_no_projects() {
        let store = store(vec!["sleep", "10"]);
        assert_eq!(store.project_count().await, 0);
        assert!(store.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn creates_a_project_from_the_template_in_the_chosen_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sh", "-c", "pwd; sleep 10"]);
        let view = store
            .create_project(None, dir.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(view.name, dir.path().file_name().unwrap().to_string_lossy());
        assert_eq!(view.panes.len(), 1);
        assert_eq!(view.cwd, dir.path().to_string_lossy());
        assert_eq!(view.panes[0].slot, Slot::TopLeft);

        let pane = store.pane(view.panes[0].id).await.unwrap();
        let expected = dir.path().to_string_lossy().into_owned();
        assert!(
            wait_for(
                || pane.contents().contains(&expected),
                Duration::from_secs(5)
            )
            .await,
            "contents: {:?}",
            pane.contents()
        );
        assert_eq!(store.project_count().await, 1);
        store.shutdown().await;
    }

    #[tokio::test]
    async fn renames_and_closes_projects() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sleep", "10"]);
        let view = store
            .create_project(Some("agents".into()), dir.path().to_path_buf())
            .await
            .unwrap();
        assert!(store.rename_project(view.id, "work").await.unwrap());
        assert_eq!(store.snapshot().await[0].name, "work");
        assert!(!store.archive_project(ProjectId::new(9999)).await);
        assert!(store.archive_project(view.id).await);
        assert_eq!(store.project_count().await, 0);
        store.shutdown().await;
    }

    #[tokio::test]
    async fn reorders_projects_and_persists_the_order() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        let template = template(vec!["sleep", "30"]);
        let store = SessionStore::with_state_dir(template.clone(), (24, 80), state.clone());
        let first = store
            .create_project(Some("first".into()), root.path().to_path_buf())
            .await
            .unwrap();
        let second = store
            .create_project(Some("second".into()), root.path().to_path_buf())
            .await
            .unwrap();
        let third = store
            .create_project(Some("third".into()), root.path().to_path_buf())
            .await
            .unwrap();

        assert!(
            store
                .reorder_projects(&[third.id, first.id, second.id])
                .await
        );
        assert_eq!(
            store
                .snapshot()
                .await
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>(),
            vec![third.id, first.id, second.id]
        );
        assert!(
            !store
                .reorder_projects(&[first.id, first.id, second.id])
                .await
        );
        assert!(!store.reorder_projects(&[second.id, first.id]).await);
        assert_eq!(persistence::load(&state).unwrap()[0].name, "third");
        store.shutdown().await;

        let restored = SessionStore::with_state_dir(template, (24, 80), state);
        assert_eq!(restored.restore().await, 3);
        assert_eq!(
            restored
                .snapshot()
                .await
                .iter()
                .map(|project| project.name.as_str())
                .collect::<Vec<_>>(),
            vec!["third", "first", "second"]
        );
        restored.shutdown().await;
    }

    #[tokio::test]
    async fn rejects_invalid_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sleep", "10"]);
        let err = store
            .create_project(Some("bad\nname".into()), dir.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidName(_)));
        store.shutdown().await;
    }

    #[tokio::test]
    async fn closing_a_project_terminates_its_panes() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sleep", "60"]);
        let view = store
            .create_project(None, dir.path().to_path_buf())
            .await
            .unwrap();
        let pane = store.pane(view.panes[0].id).await.unwrap();
        assert!(pane.is_running());
        assert!(store.archive_project(view.id).await);
        assert!(
            wait_for(|| !pane.is_running(), Duration::from_secs(4)).await,
            "pane did not stop"
        );
        store.shutdown().await;
    }

    #[tokio::test]
    async fn broadcasts_project_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        // No pane template: this test only observes project-level events, and
        // avoids depending on PTY spawn timing under parallel test load.
        let store = SessionStore::new(Vec::new(), (24, 80));
        let mut events = store.subscribe();
        let view = store
            .create_project(None, dir.path().to_path_buf())
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            SessionEvent::ProjectsChanged
        ));
        store.rename_project(view.id, "renamed").await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            SessionEvent::ProjectsChanged
        ));
        store.archive_project(view.id).await;
        assert!(matches!(
            events.recv().await.unwrap(),
            SessionEvent::ProjectsChanged
        ));
        store.shutdown().await;
    }

    #[test]
    fn resolves_session_placeholders_per_pane() {
        let panes = vec![
            PaneConfig {
                slot: Slot::TopLeft,
                command: vec!["pi".into(), "--session-id".into(), "{session_id}".into()],
                env: BTreeMap::new(),
            },
            PaneConfig {
                slot: Slot::TopRight,
                command: vec!["pi".into(), "--session-id".into(), "{session_id}".into()],
                env: BTreeMap::new(),
            },
            PaneConfig {
                slot: Slot::BottomLeft,
                command: vec!["sh".into()],
                env: BTreeMap::new(),
            },
        ];
        let resolved = resolve_session_ids(&panes);
        assert!(resolved[0].command[2].parse::<uuid::Uuid>().is_ok());
        assert!(resolved[1].command[2].parse::<uuid::Uuid>().is_ok());
        assert_ne!(resolved[0].command[2], resolved[1].command[2]);
        assert!(
            resolved
                .iter()
                .all(|pane| !pane.command.iter().any(|arg| arg.contains("{session_id}")))
        );
        assert_eq!(resolved[2].command, panes[2].command);
    }

    fn printed_session(contents: &str) -> Option<String> {
        contents
            .lines()
            .find_map(|line| line.trim().strip_prefix("SESSION=").map(str::to_owned))
    }

    #[tokio::test]
    async fn session_ids_persist_and_resume_across_restarts() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let template = vec![PaneConfig {
            slot: Slot::TopLeft,
            command: vec![
                "sh".into(),
                "-c".into(),
                "printf 'SESSION=%s\\n' \"$1\"; sleep 20".into(),
                "mmux".into(),
                "{session_id}".into(),
            ],
            env: BTreeMap::new(),
        }];

        let store = SessionStore::with_state_dir(template.clone(), (24, 80), state.clone());
        let view = store.create_project(None, cwd.clone()).await.unwrap();
        let pane = store.pane(view.panes[0].id).await.unwrap();
        assert!(
            wait_for(
                || printed_session(&pane.contents()).is_some(),
                Duration::from_secs(5)
            )
            .await,
            "contents: {:?}",
            pane.contents()
        );
        let first = printed_session(&pane.contents()).unwrap();
        assert!(first.parse::<uuid::Uuid>().is_ok(), "session id: {first}");
        store.shutdown().await;

        let records = persistence::load(&state).unwrap();
        assert!(
            records[0].panes[0].command.iter().any(|arg| arg == &first),
            "resolved session id was not persisted"
        );

        let restored = SessionStore::with_state_dir(template, (24, 80), state);
        assert_eq!(restored.restore().await, 1);
        let view = restored.snapshot().await.remove(0);
        let pane = restored.pane(view.panes[0].id).await.unwrap();
        assert!(
            wait_for(
                || printed_session(&pane.contents()).is_some(),
                Duration::from_secs(5)
            )
            .await,
            "contents: {:?}",
            pane.contents()
        );
        assert_eq!(
            printed_session(&pane.contents()).as_deref(),
            Some(first.as_str())
        );
        restored.shutdown().await;
    }

    #[tokio::test]
    async fn persists_and_restores_projects_across_stores() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let template = template(vec!["sh", "-c", "pwd; sleep 30"]);

        let store = SessionStore::with_state_dir(template.clone(), (24, 80), state.clone());
        let view = store
            .create_project(Some("api".into()), cwd.clone())
            .await
            .unwrap();
        store.rename_project(view.id, "api-work").await.unwrap();
        assert!(state.join("projects/index.json").is_file());
        store.shutdown().await;

        let restored = SessionStore::with_state_dir(template, (24, 80), state);
        assert_eq!(restored.restore().await, 1);
        let projects = restored.snapshot().await;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "api-work");
        assert_eq!(projects[0].cwd, cwd.to_string_lossy());
        assert_eq!(projects[0].panes.len(), 1);
        restored.shutdown().await;
    }

    #[tokio::test]
    async fn closing_a_project_persists_the_removal() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        let template = template(vec!["sleep", "30"]);
        let store = SessionStore::with_state_dir(template.clone(), (24, 80), state.clone());
        let view = store
            .create_project(None, root.path().to_path_buf())
            .await
            .unwrap();
        let storage_id = persistence::load(&state).unwrap()[0].id;
        let project_dir = persistence::project_dir(&state, storage_id);
        assert!(project_dir.is_dir());
        store.archive_project(view.id).await;
        assert!(
            !project_dir.exists(),
            "closed project must leave the live directory"
        );
        let archived_dir = persistence::archive_dir(&state, storage_id);
        assert!(
            archived_dir.is_dir(),
            "closed project must be preserved in the archive"
        );
        assert_eq!(store.archived_projects().await.unwrap().len(), 1);
        store.shutdown().await;

        let restored = SessionStore::with_state_dir(template, (24, 80), state);
        assert_eq!(restored.restore().await, 0);
        assert!(restored.snapshot().await.is_empty());
        assert_eq!(restored.archived_projects().await.unwrap().len(), 1);
        restored.shutdown().await;
    }

    #[tokio::test]
    async fn corrupt_state_starts_with_no_projects_and_is_not_overwritten() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("projects.json"), "not json").unwrap();
        let store =
            SessionStore::with_state_dir(template(vec!["sleep", "10"]), (24, 80), state.clone());
        assert_eq!(store.restore().await, 0);
        assert!(store.snapshot().await.is_empty());
        store.shutdown().await;
        assert!(!state.join("projects/index.json").exists());
        assert_eq!(
            std::fs::read_to_string(state.join("projects.json")).unwrap(),
            "not json"
        );
    }

    #[tokio::test]
    async fn load_failure_does_not_delete_or_rewrite_existing_projects() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        let panes = template(vec!["sleep", "10"]);
        let records = vec![
            ProjectRecord {
                id: Uuid::new_v4(),
                name: "first".into(),
                cwd: root.path().to_string_lossy().into_owned(),
                panes: panes.clone(),
            },
            ProjectRecord {
                id: Uuid::new_v4(),
                name: "second".into(),
                cwd: root.path().to_string_lossy().into_owned(),
                panes,
            },
        ];
        persistence::save(&state, &records).unwrap();
        let index_path = state.join("projects/index.json");
        let index_before = std::fs::read_to_string(&index_path).unwrap();
        std::fs::write(
            persistence::project_dir(&state, records[1].id).join("project.json"),
            "not json",
        )
        .unwrap();

        let store =
            SessionStore::with_state_dir(template(vec!["sleep", "10"]), (24, 80), state.clone());
        assert_eq!(store.restore().await, 0);
        store.shutdown().await;

        for record in &records {
            assert!(persistence::project_dir(&state, record.id).is_dir());
        }
        assert_eq!(std::fs::read_to_string(index_path).unwrap(), index_before);
    }

    #[tokio::test]
    async fn records_that_fail_to_restore_stay_in_the_index() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("state");
        let store =
            SessionStore::with_state_dir(template(vec!["sleep", "10"]), (24, 80), state.clone());
        let retained = ProjectRecord {
            id: Uuid::new_v4(),
            name: "broken".into(),
            cwd: root.path().to_string_lossy().into_owned(),
            panes: Vec::new(),
        };
        store.unrestored.lock().await.push(retained.clone());
        store.persist().await;
        store.shutdown().await;

        let records = persistence::load(&state).unwrap();
        assert!(
            records.iter().any(|record| record.id == retained.id),
            "failed project was dropped from the index"
        );
    }

    #[test]
    fn status_view_serialization_is_tagged() {
        let json = serde_json::to_value(PaneStatus::Running.view()).unwrap();
        assert_eq!(json["state"], "running");
        let json = serde_json::to_value(
            PaneStatus::Exited {
                code: Some(3),
                signal: None,
            }
            .view(),
        )
        .unwrap();
        assert_eq!(json["state"], "exited");
        assert_eq!(json["code"], 3);
    }
}
