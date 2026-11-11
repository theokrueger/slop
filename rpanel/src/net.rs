use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sysinfo::Networks;

const MONTH: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    pub rx: u64,
    pub tx: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rate {
    pub rx: f64,
    pub tx: f64,
}

/// Bytes transferred over a window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
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
    start: Option<Sample>,
    max_age: Duration,
}

impl NetworkHistory {
    pub fn new(max_age: Duration) -> Self {
        Self {
            inner: Mutex::new(History {
                samples: Vec::new(),
                start: None,
                max_age,
            }),
        }
    }

    pub fn push(&self, at: Instant, rx: u64, tx: u64) {
        let mut history = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let sample = Sample { at, rx, tx };
        history.start.get_or_insert(sample);
        history.samples.push(sample);

        let cutoff = at.checked_sub(history.max_age).unwrap_or(at);
        let keep_from = history.samples.partition_point(|sample| sample.at < cutoff);
        if keep_from > 1 {
            history.samples.drain(..keep_from - 1);
        }
    }

    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let history = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let samples = &history.samples;
        let Some(latest) = samples.last() else {
            return Snapshot::default();
        };

        let now_rate = if samples.len() >= 2 {
            rate_between(&samples[samples.len() - 2], latest)
        } else {
            Rate::default()
        };

        let window = |duration: Duration| {
            let cutoff = now.checked_sub(duration).unwrap_or(now);
            let index = samples.partition_point(|sample| sample.at <= cutoff);
            let base = if index == 0 {
                &samples[0]
            } else {
                &samples[index - 1]
            };
            transfer_between(base, latest)
        };

        let all_time = history
            .start
            .map(|start| transfer_between(&start, latest))
            .unwrap_or_default();

        Snapshot {
            now: now_rate,
            five_min: window(Duration::from_secs(5 * 60)),
            hour: window(Duration::from_secs(60 * 60)),
            day: window(Duration::from_secs(24 * 60 * 60)),
            month: window(MONTH),
            all_time,
        }
    }
}

fn rate_between(a: &Sample, b: &Sample) -> Rate {
    let seconds = b.at.saturating_duration_since(a.at).as_secs_f64();
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

/// Counters can reset when an interface is recreated; treat a decrease as a
/// fresh counter rather than underflowing.
fn delta(old: u64, new: u64) -> u64 {
    if new >= old { new - old } else { new }
}

/// Sample network counters in the background. The first sample is taken
/// immediately so the panel has a baseline.
pub fn spawn_sampler(
    history: Arc<NetworkHistory>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut networks = Networks::new_with_refreshed_list();
        let (rx, tx) = totals(&networks);
        history.push(Instant::now(), rx, tx);

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;

        loop {
            ticker.tick().await;
            networks.refresh(true);
            let (rx, tx) = totals(&networks);
            history.push(Instant::now(), rx, tx);
        }
    })
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

    fn at(now: Instant, seconds: u64) -> Instant {
        now - Duration::from_secs(seconds)
    }

    #[test]
    fn computes_rates_over_windows() {
        let history = NetworkHistory::new(MONTH);
        let now = Instant::now();
        history.push(at(now, 20), 0, 0);
        history.push(at(now, 10), 1_000, 2_000);
        history.push(now, 3_000, 6_000);

        let stats = history.snapshot(now);
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
        let now = Instant::now();
        history.push(at(now, 10), 1_000, 1_000);
        history.push(now, 500, 500);

        let stats = history.snapshot(now);
        assert_eq!(stats.now, Rate { rx: 50.0, tx: 50.0 });
        assert_eq!(stats.all_time, Transfer { rx: 500, tx: 500 });
    }

    #[test]
    fn empty_history_is_zero() {
        let history = NetworkHistory::new(MONTH);
        let stats = history.snapshot(Instant::now());
        assert_eq!(stats.now, Rate::default());
        assert_eq!(stats.all_time, Transfer::default());
    }

    #[test]
    fn prunes_old_samples_but_keeps_one_baseline() {
        let history = NetworkHistory::new(Duration::from_secs(10));
        let now = Instant::now();
        history.push(at(now, 40), 0, 0);
        history.push(at(now, 20), 1, 1);
        history.push(now, 2, 2);

        let inner = history.inner.lock().unwrap();
        assert_eq!(inner.samples.len(), 2);
        assert_eq!(inner.samples[0].at, at(now, 20));
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
        let now = Instant::now();
        history.push(at(now, 100), 0, 0);
        history.push(at(now, 20), 1_000, 1_000);
        history.push(now, 2_000, 2_000);

        let stats = history.snapshot(now);
        assert_eq!(
            stats.all_time,
            Transfer {
                rx: 2_000,
                tx: 2_000
            }
        );
    }
}
