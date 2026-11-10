use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Width of one stored usage bucket.
pub const BUCKET_SECS: u64 = 60 * 60;
/// How long usage history is kept.
pub const RETENTION_DAYS: u64 = 30;
const RETENTION_SECS: u64 = RETENTION_DAYS * 24 * 60 * 60;
const PRUNE_INTERVAL_SECS: u64 = 60 * 60;
const FLUSH_INTERVAL_SECS: u64 = 5 * 60;
/// Compact the append-only journal into the snapshot once it reaches this size.
const COMPACT_BYTES: u64 = 4 << 20;

type Counts = BTreeMap<String, BTreeMap<String, u64>>;

/// In-memory request counters with optional JSON persistence.
///
/// Frequent flushes append only the buckets that changed to a journal file.
/// The full snapshot is rewritten only when the journal grows large, when
/// expired history is pruned, or on shutdown.
#[derive(Clone)]
pub struct Usage {
    inner: Arc<Mutex<State>>,
    path: Option<PathBuf>,
    journal: Option<PathBuf>,
}

#[derive(Default)]
struct State {
    buckets: BTreeMap<u64, Counts>,
    dirty: BTreeSet<u64>,
    last_prune: u64,
}

#[derive(Deserialize)]
struct Persisted {
    #[serde(default)]
    buckets: BTreeMap<u64, Counts>,
}

#[derive(Serialize)]
struct PersistedRef<'a> {
    buckets: &'a BTreeMap<u64, Counts>,
}

#[derive(Serialize)]
struct JournalRef<'a> {
    start: u64,
    counts: &'a Counts,
}

#[derive(Deserialize)]
struct JournalEntry {
    start: u64,
    counts: Counts,
}

/// JSON payload served to the dashboard.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub bucket_secs: u64,
    pub retention_days: u64,
    pub generated_at: u64,
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Serialize)]
pub struct Bucket {
    pub start: u64,
    pub counts: BTreeMap<String, BTreeMap<String, u64>>,
}

impl Usage {
    /// Counters that are never written to disk. Used by tests and embedders.
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(State::default())),
            path: None,
            journal: None,
        }
    }

    /// Load persisted counters, ignoring an unreadable file.
    pub fn load(path: PathBuf) -> Self {
        let now = now_secs();
        let (mut buckets, snapshot_exists, snapshot_valid) = read_snapshot(&path);
        let journal = journal_path(&path);
        let journal_len = read_journal(&journal, &mut buckets);
        let pruned = prune(&mut buckets, now);
        let usage = Self {
            inner: Arc::new(Mutex::new(State {
                buckets,
                dirty: BTreeSet::new(),
                last_prune: now,
            })),
            path: Some(path),
            journal: Some(journal),
        };
        if (journal_len > 0 || (snapshot_exists && !snapshot_valid) || pruned)
            && let Err(error) = usage.save()
        {
            tracing::warn!(%error, "failed to compact usage data");
        }
        usage
    }

    /// Record one forwarded request.
    pub fn record(&self, user: &str, model: &str) {
        self.record_at(user, model, now_secs());
    }

    /// Record one forwarded request at a specific time. Exposed for tests.
    pub fn record_at(&self, user: &str, model: &str, now: u64) {
        let bucket = now / BUCKET_SECS * BUCKET_SECS;
        let mut state = self.lock();
        state
            .buckets
            .entry(bucket)
            .or_default()
            .entry(user.to_string())
            .or_default()
            .entry(model.to_string())
            .and_modify(|count| *count += 1)
            .or_insert(1);
        state.dirty.insert(bucket);
        if now.saturating_sub(state.last_prune) >= PRUNE_INTERVAL_SECS {
            prune(&mut state.buckets, now);
            state.last_prune = now;
        }
    }

    /// Snapshot the retained history as of now.
    pub fn snapshot_now(&self) -> Snapshot {
        self.snapshot_at(now_secs())
    }

    /// Snapshot the retained history as of a specific time. Exposed for tests.
    pub fn snapshot_at(&self, now: u64) -> Snapshot {
        let state = self.lock();
        let cutoff = now.saturating_sub(RETENTION_SECS);
        Snapshot {
            bucket_secs: BUCKET_SECS,
            retention_days: RETENTION_DAYS,
            generated_at: now,
            buckets: state
                .buckets
                .range(cutoff..)
                .map(|(start, counts)| Bucket {
                    start: *start,
                    counts: counts.clone(),
                })
                .collect(),
        }
    }

    /// Write changed counters to the journal, or compact when needed.
    pub fn save_if_dirty(&self) -> Result<bool> {
        self.persist(false)
    }

    /// Write the counters and compact them into the snapshot.
    pub fn save(&self) -> Result<()> {
        self.persist(true).map(|_| ())
    }

    /// Periodically flush dirty counters until the task is aborted.
    pub async fn flush_loop(self) {
        if self.path.is_none() {
            return;
        }
        let mut interval = tokio::time::interval(Duration::from_secs(FLUSH_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(error) = self.save_if_dirty() {
                tracing::warn!(%error, "failed to save usage data");
            }
        }
    }

    fn persist(&self, force: bool) -> Result<bool> {
        let (Some(path), Some(journal)) = (&self.path, &self.journal) else {
            return Ok(false);
        };
        let mut state = self.lock();

        // Expired buckets never need to be written again.
        let pruned = prune(&mut state.buckets, now_secs());
        if !force && state.dirty.is_empty() && !pruned {
            return Ok(false);
        }

        // Serialize only the buckets that changed since the last flush.
        let mut data = Vec::new();
        for start in &state.dirty {
            if let Some(counts) = state.buckets.get(start) {
                serde_json::to_writer(
                    &mut data,
                    &JournalRef {
                        start: *start,
                        counts,
                    },
                )
                .context("serialize usage journal")?;
                data.push(b'\n');
            }
        }

        let journal_len = fs::metadata(journal)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let compact =
            force || pruned || journal_len.saturating_add(data.len() as u64) > COMPACT_BYTES;
        // Journal the new values before writing the snapshot. If the journal
        // cannot be truncated afterwards, replaying it stays idempotent.
        if !data.is_empty() {
            append_journal(journal, &data)?;
        }
        if compact {
            let snapshot = serde_json::to_vec(&PersistedRef {
                buckets: &state.buckets,
            })
            .context("serialize usage data")?;
            write_atomic(path, &snapshot)?;
            truncate_journal(journal)?;
        }
        state.dirty.clear();
        Ok(true)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn read_snapshot(path: &Path) -> (BTreeMap<u64, Counts>, bool, bool) {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Persisted>(&bytes) {
            Ok(persisted) => (persisted.buckets, true, true),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "ignoring unreadable usage data"
                );
                (BTreeMap::new(), true, false)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (BTreeMap::new(), false, false)
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to read usage data"
            );
            (BTreeMap::new(), true, false)
        }
    }
}

fn read_journal(path: &Path, buckets: &mut BTreeMap<u64, Counts>) -> u64 {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "failed to read usage journal"
            );
            return 0;
        }
    };
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<JournalEntry>(line) {
            Ok(entry) => {
                buckets.insert(entry.start, entry.counts);
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "ignoring invalid usage journal line"
                );
            }
        }
    }
    bytes.len() as u64
}

fn prune(buckets: &mut BTreeMap<u64, Counts>, now: u64) -> bool {
    let cutoff = now.saturating_sub(RETENTION_SECS);
    let retained = buckets.len();
    buckets.retain(|start, _| *start >= cutoff);
    buckets.len() != retained
}

fn journal_path(path: &Path) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(".log");
    PathBuf::from(path)
}

fn append_journal(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let before = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if before == 0 {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set permissions on {}", path.display()))?;
    }
    // A crash can leave a partial trailing line. Terminate it so the next
    // append starts on a fresh line; the partial line is ignored on load.
    if before > 0
        && !ends_with_newline(path, before)
        && let Err(error) = file.write_all(b"\n")
    {
        let _ = file.set_len(before);
        return Err(error).with_context(|| format!("append {}", path.display()));
    }
    if let Err(error) = file.write_all(data) {
        let _ = file.set_len(before);
        return Err(error).with_context(|| format!("append {}", path.display()));
    }
    Ok(())
}

fn ends_with_newline(path: &Path, len: u64) -> bool {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return true,
    };
    if file.seek(SeekFrom::Start(len - 1)).is_err() {
        return true;
    }
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).is_ok() && byte[0] == b'\n'
}

fn truncate_journal(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set permissions on {}", path.display()))?;
    Ok(())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
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
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Default usage data file: `$XDG_DATA_HOME/tproxy_usage.json`, falling back
/// to `~/.local/share/tproxy_usage.json`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("tproxy_usage.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_per_user_and_model() {
        let usage = Usage::in_memory();
        let now = 1_700_000_000;
        usage.record_at("alice", "m1", now);
        usage.record_at("alice", "m1", now + 10);
        usage.record_at("bob", "m2", now + 10);

        let snapshot = usage.snapshot_at(now + 10);
        assert_eq!(snapshot.buckets.len(), 1);
        assert_eq!(snapshot.buckets[0].counts["alice"]["m1"], 2);
        assert_eq!(snapshot.buckets[0].counts["bob"]["m2"], 1);
    }

    #[test]
    fn buckets_by_hour() {
        let usage = Usage::in_memory();
        let base = 1_700_000_000 / BUCKET_SECS * BUCKET_SECS;
        usage.record_at("a", "m", base);
        usage.record_at("a", "m", base + BUCKET_SECS - 1);
        usage.record_at("a", "m", base + BUCKET_SECS);

        let snapshot = usage.snapshot_at(base + BUCKET_SECS);
        assert_eq!(snapshot.buckets.len(), 2);
        assert_eq!(snapshot.buckets[0].counts["a"]["m"], 2);
        assert_eq!(snapshot.buckets[1].counts["a"]["m"], 1);
    }

    #[test]
    fn snapshot_drops_expired_buckets() {
        let usage = Usage::in_memory();
        let now = 1_700_000_000;
        usage.record_at("a", "m", now - RETENTION_SECS - BUCKET_SECS);
        usage.record_at("a", "m", now);

        let snapshot = usage.snapshot_at(now);
        assert_eq!(snapshot.buckets.len(), 1);
        assert_eq!(snapshot.buckets[0].start, now / BUCKET_SECS * BUCKET_SECS);
    }

    #[test]
    fn flush_writes_only_changed_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let journal = journal_path(&path);
        let now = now_secs();
        let usage = Usage::load(path.clone());

        usage.record_at("alice", "m1", now);
        assert!(usage.save_if_dirty().unwrap());
        assert!(!path.exists(), "small flush must not rewrite the snapshot");
        assert_eq!(fs::read_to_string(&journal).unwrap().lines().count(), 1);

        usage.record_at("bob", "m2", now + BUCKET_SECS);
        assert!(usage.save_if_dirty().unwrap());
        assert!(!path.exists(), "small flush must not rewrite the snapshot");
        assert_eq!(fs::read_to_string(&journal).unwrap().lines().count(), 2);

        usage.record_at("alice", "m1", now);
        assert!(usage.save_if_dirty().unwrap());
        assert_eq!(fs::read_to_string(&journal).unwrap().lines().count(), 3);

        usage.save().unwrap();
        assert!(path.exists());
        assert_eq!(fs::metadata(&journal).unwrap().len(), 0);
        let persisted: Persisted = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.buckets.len(), 2);
        assert_eq!(
            persisted.buckets[&(now / BUCKET_SECS * BUCKET_SECS)]["alice"]["m1"],
            2
        );
        assert_eq!(
            persisted.buckets[&((now + BUCKET_SECS) / BUCKET_SECS * BUCKET_SECS)]["bob"]["m2"],
            1
        );
    }

    #[test]
    fn replays_journal_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let journal = journal_path(&path);
        let now = now_secs();
        let usage = Usage::load(path.clone());
        usage.record_at("alice", "m1", now);
        usage.save_if_dirty().unwrap();
        assert!(!path.exists());

        let loaded = Usage::load(path.clone());
        let snapshot = loaded.snapshot_at(now);
        assert_eq!(snapshot.buckets.len(), 1);
        assert_eq!(snapshot.buckets[0].counts["alice"]["m1"], 1);
        assert!(path.exists(), "load should compact the journal");
        assert_eq!(fs::metadata(&journal).unwrap().len(), 0);
    }

    #[test]
    fn stale_journal_does_not_override_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let journal = journal_path(&path);
        let now = now_secs();
        let hour = now / BUCKET_SECS * BUCKET_SECS;
        fs::write(
            &path,
            format!(r#"{{"buckets":{{"{hour}":{{"alice":{{"m1":2}}}}}}}}"#),
        )
        .unwrap();
        fs::write(
            &journal,
            format!(
                "{{\"start\":{hour},\"counts\":{{\"alice\":{{\"m1\":1}}}}}}\n\
                 {{\"start\":{hour},\"counts\":{{\"alice\":{{\"m1\":2}}}}}}\n"
            ),
        )
        .unwrap();

        let usage = Usage::load(path);
        let snapshot = usage.snapshot_at(now);
        assert_eq!(snapshot.buckets.len(), 1);
        assert_eq!(snapshot.buckets[0].counts["alice"]["m1"], 2);
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let now = now_secs();
        let usage = Usage::load(path.clone());
        usage.record_at("alice", "m1", now);
        usage.save().unwrap();

        let loaded = Usage::load(path);
        let snapshot = loaded.snapshot_at(now);
        assert_eq!(snapshot.buckets.len(), 1);
        assert_eq!(snapshot.buckets[0].counts["alice"]["m1"], 1);
    }

    #[test]
    fn saving_prunes_expired_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let now = now_secs();
        let usage = Usage::load(path.clone());
        usage.record_at("old", "m", now - RETENTION_SECS - BUCKET_SECS);
        usage.record_at("new", "m", now);
        usage.save().unwrap();

        let persisted: Persisted = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted.buckets.len(), 1);
        let (start, counts) = persisted.buckets.iter().next().unwrap();
        assert_eq!(*start, now / BUCKET_SECS * BUCKET_SECS);
        assert!(counts.contains_key("new"));
        assert!(!counts.contains_key("old"));
    }

    #[test]
    fn ignores_corrupt_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        fs::write(&path, b"not json").unwrap();
        let usage = Usage::load(path);
        assert!(usage.snapshot_now().buckets.is_empty());
    }

    #[test]
    fn save_sets_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let journal = journal_path(&path);
        let usage = Usage::load(path.clone());
        usage.record_at("a", "m", now_secs());
        usage.save().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mode = fs::metadata(&journal).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
