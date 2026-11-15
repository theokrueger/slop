pub mod pane;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{RwLock, broadcast};

use crate::config::{PaneConfig, Slot, validate_label};
use pane::{Pane, PaneSpec, PaneStatusView, resolve_command};

/// A tab identifier. Unique for the lifetime of the mmux server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TabId(u64);

/// A pane identifier. Unique for the lifetime of the mmux server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaneId(u64);

impl TabId {
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
    TabsChanged,
    PaneExited { tab_id: TabId, pane_id: PaneId },
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("invalid tab name: {0}")]
    InvalidName(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Serialized view of a tab. IDs are opaque to the browser.
#[derive(Debug, Clone, Serialize)]
pub struct TabView {
    pub id: TabId,
    pub name: String,
    pub cwd: String,
    pub panes: Vec<PaneView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaneView {
    pub id: PaneId,
    pub name: String,
    pub slot: Slot,
    pub status: PaneStatusView,
    pub command: String,
}

/// Owns all tabs and panes. Dropping browser connections never touches this.
pub struct SessionStore {
    template: Vec<PaneConfig>,
    default_size: (u16, u16),
    tabs: RwLock<Vec<Tab>>,
    events: broadcast::Sender<SessionEvent>,
    next_tab: AtomicU64,
    next_pane: AtomicU64,
}

struct Tab {
    id: TabId,
    name: String,
    cwd: PathBuf,
    panes: Vec<Arc<Pane>>,
}

impl SessionStore {
    pub fn new(template: Vec<PaneConfig>, default_size: (u16, u16)) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            template,
            default_size,
            tabs: RwLock::new(Vec::new()),
            events,
            next_tab: AtomicU64::new(1),
            next_pane: AtomicU64::new(1),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Creates a tab in `cwd`, spawning the configured template panes.
    pub async fn create_tab(
        &self,
        name: Option<String>,
        cwd: PathBuf,
    ) -> Result<TabView, SessionError> {
        let name = resolve_tab_name(name, &cwd)?;
        let tab_id = TabId::new(self.next_tab.fetch_add(1, Ordering::Relaxed));

        // Resolve every command before spawning anything, so a bad template
        // cannot leave a partially-created tab behind.
        let mut specs = Vec::with_capacity(self.template.len());
        for config in &self.template {
            specs.push((
                config.slot,
                PaneSpec {
                    name: config.display_name(),
                    command: resolve_command(&config.command)?,
                    env: config.env.clone(),
                    cwd: cwd.clone(),
                },
            ));
        }

        let mut panes = Vec::with_capacity(specs.len());
        for (slot, spec) in specs {
            let pane_id = PaneId::new(self.next_pane.fetch_add(1, Ordering::Relaxed));
            panes.push(Pane::spawn(
                pane_id,
                tab_id,
                slot,
                spec,
                self.default_size,
                self.events.clone(),
            ));
        }

        let tab = Tab {
            id: tab_id,
            name,
            cwd,
            panes,
        };
        let view = tab.view();
        self.tabs.write().await.push(tab);
        self.broadcast();
        Ok(view)
    }

    pub async fn rename_tab(&self, id: TabId, name: &str) -> Result<bool, SessionError> {
        let name = name.trim();
        validate_label(name).map_err(SessionError::InvalidName)?;
        let mut tabs = self.tabs.write().await;
        let Some(tab) = tabs.iter_mut().find(|tab| tab.id == id) else {
            return Ok(false);
        };
        if tab.name != name {
            tab.name = name.to_owned();
            self.broadcast();
        }
        Ok(true)
    }

    /// Closes a tab and terminates all of its panes.
    pub async fn close_tab(&self, id: TabId) -> bool {
        let removed = {
            let mut tabs = self.tabs.write().await;
            let Some(index) = tabs.iter().position(|tab| tab.id == id) else {
                return false;
            };
            tabs.remove(index)
        };
        for pane in &removed.panes {
            pane.terminate();
        }
        self.broadcast();
        true
    }

    pub async fn snapshot(&self) -> Vec<TabView> {
        let tabs = self.tabs.read().await;
        tabs.iter().map(Tab::view).collect()
    }

    pub async fn pane(&self, id: PaneId) -> Option<Arc<Pane>> {
        let tabs = self.tabs.read().await;
        tabs.iter()
            .flat_map(|tab| tab.panes.iter())
            .find(|pane| pane.id() == id)
            .cloned()
    }

    pub async fn tab_count(&self) -> usize {
        self.tabs.read().await.len()
    }

    /// Terminates panes for a clean process shutdown.
    pub async fn shutdown(&self) {
        let panes: Vec<Arc<Pane>> = {
            let tabs = self.tabs.read().await;
            tabs.iter()
                .flat_map(|tab| tab.panes.iter().cloned())
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
        let _ = self.events.send(SessionEvent::TabsChanged);
    }
}

impl Tab {
    fn view(&self) -> TabView {
        let mut panes: Vec<_> = self.panes.iter().map(|pane| pane.view()).collect();
        panes.sort_by_key(|pane| pane.slot);
        TabView {
            id: self.id,
            name: self.name.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            panes,
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
            name: self.name().to_owned(),
            slot: self.slot(),
            status: self.status().view(),
            command,
        }
    }
}

fn resolve_tab_name(name: Option<String>, cwd: &Path) -> Result<String, SessionError> {
    if let Some(name) = name {
        let name = name.trim();
        if !name.is_empty() {
            validate_label(name).map_err(SessionError::InvalidName)?;
            return Ok(name.to_owned());
        }
    }
    Ok(default_tab_name(cwd))
}

fn default_tab_name(cwd: &Path) -> String {
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
            name: Some("test".into()),
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
    async fn starts_with_no_tabs() {
        let store = store(vec!["sleep", "10"]);
        assert_eq!(store.tab_count().await, 0);
        assert!(store.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn creates_a_tab_from_the_template_in_the_chosen_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sh", "-c", "pwd; sleep 10"]);
        let view = store
            .create_tab(None, dir.path().to_path_buf())
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
        assert_eq!(store.tab_count().await, 1);
        store.shutdown().await;
    }

    #[tokio::test]
    async fn renames_and_closes_tabs() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sleep", "10"]);
        let view = store
            .create_tab(Some("agents".into()), dir.path().to_path_buf())
            .await
            .unwrap();
        assert!(store.rename_tab(view.id, "work").await.unwrap());
        assert_eq!(store.snapshot().await[0].name, "work");
        assert!(!store.close_tab(TabId::new(9999)).await);
        assert!(store.close_tab(view.id).await);
        assert_eq!(store.tab_count().await, 0);
        store.shutdown().await;
    }

    #[tokio::test]
    async fn rejects_invalid_names() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sleep", "10"]);
        let err = store
            .create_tab(Some("bad\nname".into()), dir.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::InvalidName(_)));
        store.shutdown().await;
    }

    #[tokio::test]
    async fn closing_a_tab_terminates_its_panes() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sleep", "60"]);
        let view = store
            .create_tab(None, dir.path().to_path_buf())
            .await
            .unwrap();
        let pane = store.pane(view.panes[0].id).await.unwrap();
        assert!(pane.is_running());
        assert!(store.close_tab(view.id).await);
        assert!(
            wait_for(|| !pane.is_running(), Duration::from_secs(4)).await,
            "pane did not stop"
        );
        store.shutdown().await;
    }

    #[tokio::test]
    async fn broadcasts_tab_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = store(vec!["sleep", "10"]);
        let mut events = store.subscribe();
        let view = store
            .create_tab(None, dir.path().to_path_buf())
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            SessionEvent::TabsChanged
        ));
        store.rename_tab(view.id, "renamed").await.unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            SessionEvent::TabsChanged
        ));
        store.close_tab(view.id).await;
        assert!(matches!(
            events.recv().await.unwrap(),
            SessionEvent::TabsChanged
        ));
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
