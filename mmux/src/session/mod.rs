pub mod pane;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    state_path: Option<PathBuf>,
    persist_lock: Mutex<()>,
}

struct Project {
    id: ProjectId,
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

    /// Store that saves projects to `state_path` and can restore them.
    pub fn with_state_file(
        template: Vec<PaneConfig>,
        default_size: (u16, u16),
        state_path: PathBuf,
    ) -> Self {
        Self::build(template, default_size, Some(state_path))
    }

    fn build(
        template: Vec<PaneConfig>,
        default_size: (u16, u16),
        state_path: Option<PathBuf>,
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
            state_path,
            persist_lock: Mutex::new(()),
        }
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
        let Some(path) = self.state_path.as_ref() else {
            return 0;
        };
        let records = match persistence::load(path) {
            Ok(records) => records,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    state = %path.display(),
                    "could not restore projects; starting empty"
                );
                return 0;
            }
        };

        let mut restored = 0;
        let mut needs_save = false;
        for record in records {
            match self.restore_project(record).await {
                Ok(resolved_sessions) => {
                    restored += 1;
                    needs_save |= resolved_sessions;
                }
                Err(err) => tracing::warn!(error = %err, "could not restore saved project"),
            }
        }
        if needs_save {
            // Older state may still contain placeholders; persist the resolved
            // commands so the same session IDs are used next time.
            self.persist().await;
        }
        if restored > 0 {
            tracing::info!(restored, state = %path.display(), "restored projects");
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
        self.spawn_project(name, cwd, &panes).await?;
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
        let view = self.spawn_project(name, cwd, &panes).await?;
        self.persist().await;
        Ok(view)
    }

    async fn spawn_project(
        &self,
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
                Ok((
                    config.slot,
                    PaneSpec {
                        command: resolve_command(&config.command)?,
                        env: config.env.clone(),
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

    /// Closes a project and terminates all of its panes.
    pub async fn close_project(&self, id: ProjectId) -> bool {
        let removed = {
            let mut projects = self.projects.write().await;
            let Some(index) = projects.iter().position(|project| project.id == id) else {
                return false;
            };
            projects.remove(index)
        };
        for pane in &removed.panes {
            pane.terminate();
        }
        self.broadcast();
        self.persist().await;
        true
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
    /// propagated: losing durability must not break the live session.
    async fn persist(&self) {
        let Some(path) = self.state_path.as_ref() else {
            return;
        };
        let path = path.clone();
        let path_display = path.display().to_string();
        let _guard = self.persist_lock.lock().await;
        let records: Vec<ProjectRecord> = {
            let projects = self.projects.read().await;
            projects.iter().map(Project::record).collect()
        };
        let result = tokio::task::spawn_blocking(move || persistence::save(&path, &records)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(error = %err, state = %path_display, "could not save projects");
            }
            Err(err) => {
                tracing::warn!(error = %err, "project persistence task failed");
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
        assert!(!store.close_project(ProjectId::new(9999)).await);
        assert!(store.close_project(view.id).await);
        assert_eq!(store.project_count().await, 0);
        store.shutdown().await;
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
        assert!(store.close_project(view.id).await);
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
        store.close_project(view.id).await;
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
        let state = root.path().join("state/projects.json");
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

        let store = SessionStore::with_state_file(template.clone(), (24, 80), state.clone());
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

        let restored = SessionStore::with_state_file(template, (24, 80), state);
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
        let state = root.path().join("state/projects.json");
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let template = template(vec!["sh", "-c", "pwd; sleep 30"]);

        let store = SessionStore::with_state_file(template.clone(), (24, 80), state.clone());
        let view = store
            .create_project(Some("api".into()), cwd.clone())
            .await
            .unwrap();
        store.rename_project(view.id, "api-work").await.unwrap();
        assert!(state.is_file());
        store.shutdown().await;

        let restored = SessionStore::with_state_file(template, (24, 80), state);
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
        let state = root.path().join("state/projects.json");
        let template = template(vec!["sleep", "30"]);
        let store = SessionStore::with_state_file(template.clone(), (24, 80), state.clone());
        let view = store
            .create_project(None, root.path().to_path_buf())
            .await
            .unwrap();
        store.close_project(view.id).await;
        store.shutdown().await;

        let restored = SessionStore::with_state_file(template, (24, 80), state);
        assert_eq!(restored.restore().await, 0);
        assert!(restored.snapshot().await.is_empty());
        restored.shutdown().await;
    }

    #[tokio::test]
    async fn corrupt_state_starts_with_no_projects() {
        let root = tempfile::TempDir::new().unwrap();
        let state = root.path().join("projects.json");
        std::fs::write(&state, "not json").unwrap();
        let store = SessionStore::with_state_file(template(vec!["sleep", "10"]), (24, 80), state);
        assert_eq!(store.restore().await, 0);
        assert!(store.snapshot().await.is_empty());
        store.shutdown().await;
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
