//! Watches the vault for changes made outside this server — an agent writing
//! through MCP, or `mmux mem` from a shell — and tells connected browsers to
//! re-fetch.
//!
//! A fingerprint poll is used instead of inotify: the vault is a few hundred
//! small markdown files, editors write via rename, and this keeps mmux
//! dependency-free. Nothing is read except file metadata.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use super::Memory;
use crate::session::SessionEvent;

/// Generous on purpose: a two-second lag on a memory list is invisible, and a
/// slower poll costs nothing when the vault is idle.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub fn spawn(memory: Arc<Memory>, events: broadcast::Sender<SessionEvent>) {
    spawn_with_interval(memory, events, POLL_INTERVAL);
}

pub fn spawn_with_interval(
    memory: Arc<Memory>,
    events: broadcast::Sender<SessionEvent>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = fingerprint(&memory);
        loop {
            ticker.tick().await;
            let memory = Arc::clone(&memory);
            let Ok(current) = tokio::task::spawn_blocking(move || fingerprint(&memory)).await
            else {
                continue;
            };
            if current != last {
                last = current;
                let _ = events.send(SessionEvent::MemoryChanged);
            }
        }
    });
}

type FileStamp = (PathBuf, u64, u128);
type WorkspaceStamps = (String, Vec<FileStamp>);

/// Order-independent hash of every document's path, size, and mtime across all
/// known workspaces. A doc being written, edited, or deleted changes it.
fn fingerprint(memory: &Memory) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let Ok(workspaces) = memory.known_workspaces() else {
        return hash;
    };

    let mut workspaces: Vec<WorkspaceStamps> = workspaces
        .into_iter()
        .map(|workspace| {
            let mut files = Vec::new();
            for dir in [memory.docs_dir(&workspace), memory.archive_dir(&workspace)] {
                let Ok(entries) = fs::read_dir(dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                        continue;
                    }
                    let Ok(meta) = entry.metadata() else {
                        continue;
                    };
                    let modified = meta
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos())
                        .unwrap_or(0);
                    files.push((path, meta.len(), modified));
                }
            }
            files.sort();
            (workspace.key, files)
        })
        .collect();
    workspaces.sort_by(|left, right| left.0.cmp(&right.0));

    for (key, files) in workspaces {
        mix(&mut hash, key.as_bytes());
        for (path, len, modified) in files {
            mix(&mut hash, path.to_string_lossy().as_bytes());
            mix(&mut hash, &len.to_le_bytes());
            mix(&mut hash, &modified.to_le_bytes());
        }
    }
    hash
}

fn mix(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Author, NewDoc};

    #[tokio::test]
    async fn writing_a_document_emits_memory_changed() {
        let dir = tempfile::TempDir::new().unwrap();
        let memory = Arc::new(Memory::new(dir.path().join("memory")));
        let workspace = memory.workspace_for(dir.path()).unwrap();
        let (events, mut receiver) = broadcast::channel(8);
        spawn_with_interval(Arc::clone(&memory), events, Duration::from_millis(30));
        // Let the watcher take its baseline fingerprint first.
        tokio::time::sleep(Duration::from_millis(80)).await;

        memory
            .add(
                &workspace,
                NewDoc {
                    title: "watch me".into(),
                    body: "changed outside the server".into(),
                    ..Default::default()
                },
                &Author::user(),
            )
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("no event within two seconds")
            .expect("broadcast closed");
        assert!(matches!(event, SessionEvent::MemoryChanged));
    }

    #[test]
    fn fingerprint_is_stable_without_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let memory = Memory::new(dir.path().join("memory"));
        let workspace = memory.workspace_for(dir.path()).unwrap();
        assert_eq!(fingerprint(&memory), fingerprint(&memory));
        memory
            .add(
                &workspace,
                NewDoc {
                    title: "one".into(),
                    body: "body".into(),
                    ..Default::default()
                },
                &Author::user(),
            )
            .unwrap();
        let after = fingerprint(&memory);
        assert_eq!(after, fingerprint(&memory));
        memory.archive(&workspace, "one").unwrap();
        assert_ne!(after, fingerprint(&memory));
    }
}
