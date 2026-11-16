use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sysinfo::Networks;

const MONTH: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Samples kept in memory and on disk: one month plus a baseline sample so the
/// month window can still be computed between polls.
pub const RETENTION: Duration = Duration::from_secs(32 * 24 * 60 * 60);

/// History is saved at most this often.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(60);

/// Samples newer than this keep full resolution; older ones are bucketed so
/// the state file stays small however long the process runs.
const RAW_HORIZON: Duration = Duration::from_secs(2 * 60 * 60);
const FINE_HORIZON: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const FINE_BUCKET: u64 = 5 * 60 * 1000;
const COARSE_BUCKET: u64 = 60 * 60 * 1000;

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Sample {
    /// Unix timestamp in milliseconds. Wall-clock rather than `Instant` so the
    /// history can be reloaded after a restart.
    at: u64,
    rx: u64,
    tx: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    pub rx: f64,
    pub tx: f64,
}

/// Bytes transferred over a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Transfer {
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    /// Current speed, derived from the last two samples.
    pub now: Rate,
    pub five_min: Transfer,
    pub hour: Transfer,
    pub day: Transfer,
    pub month: Transfer,
    pub all_time: Transfer,
}

/// Rolling totals of all non-loopback interfaces, used to derive throughput.
#[derive(Debug)]
pub struct NetworkHistory {
    inner: Mutex<History>,
}

#[derive(Debug)]
struct History {
    samples: Vec<Sample>,
    /// Everything transferred since the history was created, including traffic
    /// seen by earlier runs of the process. Saved alongside the samples.
    lifetime: Transfer,
    max_age: Duration,
    path: Option<PathBuf>,
    dirty: bool,
}

/// On-disk form of the history.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    version: u32,
    #[serde(default)]
    lifetime: Transfer,
    #[serde(default)]
    samples: Vec<Sample>,
}

impl NetworkHistory {
    /// History that lives only in memory.
    pub fn new(max_age: Duration) -> Self {
        Self {
            inner: Mutex::new(History {
                samples: Vec::new(),
                lifetime: Transfer::default(),
                max_age,
                path: None,
                dirty: false,
            }),
        }
    }

    /// Load the history stored at `path` (if it exists) and persist future
    /// samples there.
    pub fn with_state(max_age: Duration, path: PathBuf) -> Self {
        let mut history = History {
            samples: Vec::new(),
            lifetime: Transfer::default(),
            max_age,
            path: Some(path.clone()),
            dirty: false,
        };
        match load(&path) {
            Ok(saved) => history.restore(saved),
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), %err, "ignoring network history");
            }
            Err(_) => {}
        }
        history.prune(now_ms());
        Self {
            inner: Mutex::new(history),
        }
    }

    pub fn push(&self, rx: u64, tx: u64) {
        self.push_at(now_ms(), rx, tx);
    }

    pub fn snapshot(&self) -> Snapshot {
        let history = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let samples = &history.samples;
        let Some(latest) = samples.last().copied() else {
            return Snapshot::default();
        };

        let now_rate = if samples.len() >= 2 {
            rate_between(&samples[samples.len() - 2], &latest)
        } else {
            Rate::default()
        };

        let now = now_ms().max(latest.at);
        let window = |duration: Duration| {
            let cutoff = now.saturating_sub(millis(duration));
            let index = samples.partition_point(|sample| sample.at <= cutoff);
            let base = if index == 0 {
                &samples[0]
            } else {
                &samples[index - 1]
            };
            transfer_between(base, &latest)
        };

        Snapshot {
            now: now_rate,
            five_min: window(Duration::from_secs(5 * 60)),
            hour: window(Duration::from_secs(60 * 60)),
            day: window(Duration::from_secs(24 * 60 * 60)),
            month: window(MONTH),
            all_time: history.lifetime,
        }
    }

    /// Write the history to disk. Returns `Ok(())` when there is nothing to
    /// write or persistence is disabled.
    pub fn save(&self) -> io::Result<()> {
        let mut history = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(path) = history.path.clone() else {
            return Ok(());
        };
        if !history.dirty {
            return Ok(());
        }
        let saved = Saved {
            version: STATE_VERSION,
            lifetime: history.lifetime,
            samples: history.samples.clone(),
        };
        write_atomic(&path, &saved)?;
        history.dirty = false;
        Ok(())
    }

    fn push_at(&self, at: u64, rx: u64, tx: u64) {
        let mut history = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Keep the timeline monotonic if the wall clock steps backwards.
        let at = history.samples.last().map_or(at, |last| at.max(last.at));
        let sample = Sample { at, rx, tx };
        if let Some(last) = history.samples.last().copied() {
            let delta = transfer_between(&last, &sample);
            history.lifetime.rx = history.lifetime.rx.saturating_add(delta.rx);
            history.lifetime.tx = history.lifetime.tx.saturating_add(delta.tx);
        }
        history.samples.push(sample);
        history.dirty = true;
        history.prune(at);
    }
}

impl History {
    fn restore(&mut self, saved: Saved) {
        if saved.version != STATE_VERSION {
            tracing::warn!(
                version = saved.version,
                "ignoring network history with an unknown version"
            );
            return;
        }
        let mut samples = saved.samples;
        samples.sort_by_key(|sample| sample.at);
        samples.dedup_by_key(|sample| sample.at);
        self.samples = samples;
        self.lifetime = saved.lifetime;
    }

    fn prune(&mut self, now: u64) {
        if self.samples.is_empty() {
            return;
        }
        let cutoff = now.saturating_sub(millis(self.max_age));
        let keep_from = self.samples.partition_point(|sample| sample.at < cutoff);
        if keep_from > 1 {
            self.samples.drain(..keep_from - 1);
        }
        compact(&mut self.samples, now);
    }
}

/// Downsample old samples in place: full resolution for the last two hours,
/// five-minute buckets up to two days, and hourly buckets beyond that.
/// Counters are cumulative, so a kept sample stands in for the span it covers.
fn compact(samples: &mut Vec<Sample>, now: u64) {
    if samples.len() < 3 {
        return;
    }
    let raw_from = now.saturating_sub(millis(RAW_HORIZON));
    let fine_from = now.saturating_sub(millis(FINE_HORIZON));

    let mut compacted = Vec::with_capacity(samples.len().min(4096));
    // The oldest sample is the baseline for the widest window; always keep it.
    compacted.push(samples[0]);
    let mut pending: Option<((u8, u64), Sample)> = None;
    for &sample in &samples[1..] {
        let key = if sample.at >= raw_from {
            (2, sample.at)
        } else if sample.at >= fine_from {
            (1, sample.at / FINE_BUCKET)
        } else {
            (0, sample.at / COARSE_BUCKET)
        };
        match pending {
            Some((previous, _)) if previous == key => pending = Some((key, sample)),
            Some((_, previous)) => {
                compacted.push(previous);
                pending = Some((key, sample));
            }
            None => pending = Some((key, sample)),
        }
    }
    if let Some((_, sample)) = pending {
        compacted.push(sample);
    }
    *samples = compacted;
}

fn rate_between(a: &Sample, b: &Sample) -> Rate {
    let seconds = b.at.saturating_sub(a.at) as f64 / 1000.0;
    if seconds <= 0.0 {
        return Rate::default();
    }
    let transfer = transfer_between(a, b);
    Rate {
        rx: transfer.rx as f64 / seconds,
        tx: transfer.tx as f64 / seconds,
    }
}

fn transfer_between(a: &Sample, b: &Sample) -> Transfer {
    Transfer {
        rx: delta(a.rx, b.rx),
        tx: delta(a.tx, b.tx),
    }
}

/// Counters can reset when an interface is recreated or the host reboots; treat
/// a decrease as a fresh counter rather than underflowing.
fn delta(old: u64, new: u64) -> u64 {
    if new >= old { new - old } else { new }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn load(path: &Path) -> io::Result<Saved> {
    let text = fs::read_to_string(path)?;
    serde_yaml_ng::from_str(&text).map_err(io::Error::other)
}

fn write_atomic(path: &Path, saved: &Saved) -> io::Result<()> {
    let yaml = serde_yaml_ng::to_string(saved).map_err(io::Error::other)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, yaml)?;
    fs::rename(&tmp, path)
}

/// Sample network counters in the background. The first sample is taken
/// immediately so the panel has a baseline, and the history is written to disk
/// periodically.
pub fn spawn_sampler(
    history: Arc<NetworkHistory>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut networks = Networks::new_with_refreshed_list();
        let (rx, tx) = totals(&networks);
        history.push(rx, tx);

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        let mut saved_at = Instant::now();
        loop {
            ticker.tick().await;
            networks.refresh(true);
            let (rx, tx) = totals(&networks);
            history.push(rx, tx);
            if saved_at.elapsed() >= SAVE_INTERVAL {
                save(&history);
                saved_at = Instant::now();
            }
        }
    })
}

/// Save the history, logging instead of failing: a read-only state directory
/// should not stop the proxy from running.
pub fn save(history: &NetworkHistory) {
    if let Err(err) = history.save() {
        tracing::warn!(%err, "saving network history");
    }
}

fn totals(networks: &Networks) -> (u64, u64) {
    networks
        .iter()
        .filter(|(name, _)| !is_loopback(name))
        .fold((0, 0), |(rx, tx), (_, data)| {
            (
                rx.saturating_add(data.total_received()),
                tx.saturating_add(data.total_transmitted()),
            )
        })
}

fn is_loopback(name: &str) -> bool {
    name == "lo" || name == "lo0" || name.starts_with("lo:") || name.starts_with("Loopback")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ago(now: u64, seconds: u64) -> u64 {
        now.saturating_sub(seconds * 1000)
    }

    fn temp_path(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("rpanel-net-{}-{name}.yaml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn computes_rates_over_windows() {
        let history = NetworkHistory::new(MONTH);
        let now = now_ms();
        history.push_at(ago(now, 20), 0, 0);
        history.push_at(ago(now, 10), 1_000, 2_000);
        history.push_at(now, 3_000, 6_000);

        let stats = history.snapshot();
        assert_eq!(
            stats.now,
            Rate {
                rx: 200.0,
                tx: 400.0
            }
        );
        assert_eq!(
            stats.five_min,
            Transfer {
                rx: 3_000,
                tx: 6_000
            }
        );
        assert_eq!(
            stats.all_time,
            Transfer {
                rx: 3_000,
                tx: 6_000
            }
        );
    }

    #[test]
    fn handles_counter_reset() {
        let history = NetworkHistory::new(MONTH);
        let now = now_ms();
        history.push_at(ago(now, 10), 1_000, 1_000);
        history.push_at(now, 500, 500);

        let stats = history.snapshot();
        assert_eq!(stats.now, Rate { rx: 50.0, tx: 50.0 });
        assert_eq!(stats.all_time, Transfer { rx: 500, tx: 500 });
    }

    #[test]
    fn empty_history_is_zero() {
        let history = NetworkHistory::new(MONTH);
        let stats = history.snapshot();
        assert_eq!(stats.now, Rate::default());
        assert_eq!(stats.all_time, Transfer::default());
    }

    #[test]
    fn prunes_old_samples_but_keeps_one_baseline() {
        let history = NetworkHistory::new(Duration::from_secs(10));
        let now = now_ms();
        history.push_at(ago(now, 40), 0, 0);
        history.push_at(ago(now, 20), 1, 1);
        history.push_at(now, 2, 2);

        let inner = history.inner.lock().unwrap();
        assert_eq!(inner.samples.len(), 2);
        assert_eq!(inner.samples[0].at, ago(now, 20));
    }

    #[test]
    fn detects_loopback_interfaces() {
        assert!(is_loopback("lo"));
        assert!(is_loopback("lo0"));
        assert!(is_loopback("lo:1"));
        assert!(is_loopback("Loopback Pseudo-Interface 1"));
        assert!(!is_loopback("eth0"));
    }

    #[test]
    fn all_time_survives_pruning() {
        let history = NetworkHistory::new(Duration::from_secs(10));
        let now = now_ms();
        history.push_at(ago(now, 100), 0, 0);
        history.push_at(ago(now, 20), 1_000, 1_000);
        history.push_at(now, 2_000, 2_000);

        let stats = history.snapshot();
        assert_eq!(
            stats.all_time,
            Transfer {
                rx: 2_000,
                tx: 2_000
            }
        );
    }

    #[test]
    fn saves_and_restores_history() {
        let path = temp_path("roundtrip");
        let now = now_ms();

        let history = NetworkHistory::with_state(MONTH, path.clone());
        history.push_at(ago(now, 120), 0, 0);
        history.push_at(ago(now, 60), 1_000, 2_000);
        history.push_at(now, 3_000, 6_000);
        history.save().unwrap();

        let restored = NetworkHistory::with_state(MONTH, path.clone());
        let stats = restored.snapshot();
        assert_eq!(
            stats.all_time,
            Transfer {
                rx: 3_000,
                tx: 6_000
            }
        );
        assert_eq!(
            stats.hour,
            Transfer {
                rx: 3_000,
                tx: 6_000
            }
        );
        // The rate is still derived from the two most recent saved samples
        // until the sampler records a fresh one.
        assert_eq!(
            stats.now,
            Rate {
                rx: 2_000.0 / 60.0,
                tx: 4_000.0 / 60.0
            }
        );

        // New samples continue from the restored totals.
        restored.push_at(now + 10_000, 4_000, 8_000);
        assert_eq!(
            restored.snapshot().all_time,
            Transfer {
                rx: 4_000,
                tx: 8_000
            }
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_without_a_path_is_a_noop() {
        let history = NetworkHistory::new(MONTH);
        history.push_at(now_ms(), 1, 1);
        history.save().unwrap();
    }

    #[test]
    fn ignores_history_from_another_version() {
        let path = temp_path("version");
        let invalid = Saved {
            version: STATE_VERSION + 1,
            lifetime: Transfer { rx: 9, tx: 9 },
            samples: vec![Sample {
                at: now_ms(),
                rx: 9,
                tx: 9,
            }],
        };
        write_atomic(&path, &invalid).unwrap();

        let history = NetworkHistory::with_state(MONTH, path.clone());
        let stats = history.snapshot();
        assert_eq!(stats.all_time, Transfer::default());
        assert_eq!(stats.now, Rate::default());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compaction_keeps_recent_detail_and_bounds_size() {
        let history = NetworkHistory::new(RETENTION);
        let now = now_ms();
        // Ten-second samples over 40 days: ~345,600 pushes would be far too
        // slow, so step by ten minutes and assert the bound instead.
        let mut rx = 0;
        for step in (0..=40 * 24 * 6).rev() {
            rx += 1_000;
            history.push_at(ago(now, step * 600), rx, rx);
        }

        let inner = history.inner.lock().unwrap();
        assert!(inner.samples.len() < 3_000, "{}", inner.samples.len());
        // Everything from the last two hours is still full resolution.
        let raw_from = ago(now, 2 * 60 * 60);
        let recent: Vec<_> = inner
            .samples
            .iter()
            .filter(|sample| sample.at >= raw_from)
            .collect();
        assert!(recent.len() >= 12, "{}", recent.len());
    }
}
