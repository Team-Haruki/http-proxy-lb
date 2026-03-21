use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::config::{BalanceMode, Config, UpstreamConfig};

// ---------------------------------------------------------------------------
// Upstream state
// ---------------------------------------------------------------------------

const STATE_ONLINE: u8 = 0;
const STATE_OFFLINE: u8 = 1;

// ---------------------------------------------------------------------------
// UpstreamEntry — one upstream proxy with live state & statistics
// ---------------------------------------------------------------------------

pub struct UpstreamEntry {
    pub config: UpstreamConfig,
    /// 0 = online, 1 = offline
    state: AtomicU8,
    /// Number of in-flight connections currently routed through this upstream
    pub active_conns: AtomicI64,
    /// Exponential moving average of successful response latency (stored as
    /// whole milliseconds; 0 means "no sample yet").
    latency_ema_ms: AtomicU64,
    /// Consecutive passive failures — for future tuning.
    pub consec_failures: AtomicU32,
    /// Wall-clock time of last state change (for logging only).
    last_state_change: Mutex<Instant>,
}

impl UpstreamEntry {
    fn new(config: UpstreamConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: AtomicU8::new(STATE_ONLINE),
            active_conns: AtomicI64::new(0),
            latency_ema_ms: AtomicU64::new(0),
            consec_failures: AtomicU32::new(0),
            last_state_change: Mutex::new(Instant::now()),
        })
    }

    fn from_existing(existing: &Self, config: UpstreamConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: AtomicU8::new(existing.state.load(Ordering::Acquire)),
            active_conns: AtomicI64::new(existing.active_conns.load(Ordering::Relaxed)),
            latency_ema_ms: AtomicU64::new(existing.latency_ema_ms.load(Ordering::Relaxed)),
            consec_failures: AtomicU32::new(existing.consec_failures.load(Ordering::Relaxed)),
            last_state_change: Mutex::new(*existing.last_state_change.lock()),
        })
    }

    // ---- state accessors ---------------------------------------------------

    pub fn is_online(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_ONLINE
    }

    pub fn mark_offline(&self) {
        if self
            .state
            .compare_exchange(
                STATE_ONLINE,
                STATE_OFFLINE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            *self.last_state_change.lock() = Instant::now();
            warn!(upstream = %self.config.url, "marked OFFLINE");
        }
    }

    pub fn mark_online(&self) {
        if self
            .state
            .compare_exchange(
                STATE_OFFLINE,
                STATE_ONLINE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.consec_failures.store(0, Ordering::Release);
            *self.last_state_change.lock() = Instant::now();
            info!(upstream = %self.config.url, "marked ONLINE");
        }
    }

    // ---- statistics --------------------------------------------------------

    /// Record a successful request with the measured round-trip latency.
    pub fn record_success(&self, latency_ms: u64) {
        self.consec_failures.store(0, Ordering::Release);
        // EMA(α=0.25): new = 0.25·sample + 0.75·old
        let old = self.latency_ema_ms.load(Ordering::Relaxed);
        let new_ema = if old == 0 {
            latency_ms
        } else {
            (latency_ms + old * 3) / 4
        };
        self.latency_ema_ms.store(new_ema, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        self.consec_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Lower score = better upstream for "best" mode.
    /// Combines latency EMA and active-connection penalty.
    pub fn score(&self) -> u64 {
        let latency = self.latency_ema_ms.load(Ordering::Relaxed);
        let base = if latency == 0 { 50 } else { latency }; // assume 50 ms if unknown
        let active = self.active_conns.load(Ordering::Relaxed).max(0) as u64;
        base + active * 50 // each in-flight connection adds 50 ms penalty
    }

    // ---- helpers -----------------------------------------------------------

    pub fn host_port(&self) -> Result<(String, u16)> {
        self.config.host_port()
    }
}

// ---------------------------------------------------------------------------
// UpstreamStatus — snapshot for admin API
// ---------------------------------------------------------------------------

/// Status snapshot of an upstream for metrics/admin API.
pub struct UpstreamStatus {
    pub url: String,
    pub online: bool,
    pub active_conns: i64,
    pub latency_ema_ms: u64,
    pub consec_failures: u32,
    pub weight: u32,
    pub priority: u32,
}

// ---------------------------------------------------------------------------
// UpstreamPool — thread-safe collection of upstream entries
// ---------------------------------------------------------------------------

pub struct UpstreamPool {
    entries: Mutex<Vec<Arc<UpstreamEntry>>>,
    /// Monotonically increasing counter used for round-robin cursor.
    rr_counter: AtomicU64,
}

impl UpstreamPool {
    /// Build an initial pool from a freshly loaded `Config`.
    pub fn from_config(cfg: &Config) -> Arc<Self> {
        let entries: Vec<Arc<UpstreamEntry>> = cfg
            .upstream
            .iter()
            .map(|c| UpstreamEntry::new(c.clone()))
            .collect();
        info!(count = entries.len(), "upstream pool initialised");
        Arc::new(Self {
            entries: Mutex::new(entries),
            rr_counter: AtomicU64::new(0),
        })
    }

    // ---- selection ---------------------------------------------------------

    /// Select an online upstream, skipping indices in `exclude`.
    /// Returns `(pool_index, entry)` or `None` when no online upstream remains.
    pub fn select(
        &self,
        mode: BalanceMode,
        exclude: &[usize],
    ) -> Option<(usize, Arc<UpstreamEntry>)> {
        match mode {
            BalanceMode::RoundRobin => self.select_rr(exclude),
            BalanceMode::Best => self.select_best(exclude),
            BalanceMode::Priority => self.select_priority(exclude),
        }
    }

    fn select_rr(&self, exclude: &[usize]) -> Option<(usize, Arc<UpstreamEntry>)> {
        let entries = self.entries.lock();
        // Build a weighted candidate list.
        let candidates: Vec<(usize, &Arc<UpstreamEntry>)> = entries
            .iter()
            .enumerate()
            .filter(|(i, e)| e.is_online() && !exclude.contains(i))
            .flat_map(|(i, e)| std::iter::repeat_n((i, e), e.config.weight.max(1) as usize))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) as usize % candidates.len();
        let (pool_idx, entry) = candidates[idx];
        Some((pool_idx, Arc::clone(entry)))
    }

    fn select_best(&self, exclude: &[usize]) -> Option<(usize, Arc<UpstreamEntry>)> {
        let entries = self.entries.lock();
        entries
            .iter()
            .enumerate()
            .filter(|(i, e)| e.is_online() && !exclude.contains(i))
            .min_by_key(|(_, e)| e.score())
            .map(|(i, e)| (i, Arc::clone(e)))
    }

    fn select_priority(&self, exclude: &[usize]) -> Option<(usize, Arc<UpstreamEntry>)> {
        let entries = self.entries.lock();
        entries
            .iter()
            .enumerate()
            .filter(|(i, e)| e.is_online() && !exclude.contains(i))
            .min_by_key(|(i, e)| (e.config.priority, *i))
            .map(|(i, e)| (i, Arc::clone(e)))
    }

    // ---- queries -----------------------------------------------------------

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Snapshot of all offline entries (for the health checker).
    pub fn offline_entries(&self) -> Vec<Arc<UpstreamEntry>> {
        self.entries
            .lock()
            .iter()
            .filter(|e| !e.is_online())
            .cloned()
            .collect()
    }

    /// Get status of all upstreams for metrics/admin API.
    pub fn all_status(&self) -> Vec<UpstreamStatus> {
        self.entries
            .lock()
            .iter()
            .map(|e| UpstreamStatus {
                url: e.config.url.clone(),
                online: e.is_online(),
                active_conns: e.active_conns.load(Ordering::Relaxed),
                latency_ema_ms: e.latency_ema_ms.load(Ordering::Relaxed),
                consec_failures: e.consec_failures.load(Ordering::Relaxed),
                weight: e.config.weight,
                priority: e.config.priority,
            })
            .collect()
    }

    // ---- hot reload --------------------------------------------------------

    /// Replace the upstream list from a new config while preserving state/stats
    /// for upstreams that already exist (matched by URL).
    pub fn reload(&self, new_cfg: &Config) {
        let mut entries = self.entries.lock();

        let new_entries: Vec<Arc<UpstreamEntry>> = new_cfg
            .upstream
            .iter()
            .map(|upstream_cfg| {
                // Reuse existing entry if URL matches (preserves state/stats).
                if let Some(existing) = entries.iter().find(|e| e.config.url == upstream_cfg.url) {
                    // State/stats are preserved while config (weight/auth/priority) is refreshed.
                    debug!(url = %upstream_cfg.url, "reusing existing upstream entry");
                    UpstreamEntry::from_existing(existing, upstream_cfg.clone())
                } else {
                    info!(url = %upstream_cfg.url, "adding new upstream");
                    UpstreamEntry::new(upstream_cfg.clone())
                }
            })
            .collect();

        // Log removals.
        for old in entries.iter() {
            if !new_cfg.upstream.iter().any(|n| n.url == old.config.url) {
                info!(url = %old.config.url, "removing upstream");
            }
        }

        *entries = new_entries;
        info!(count = entries.len(), "upstream pool reloaded");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BalanceMode, Config, DomainPolicyConfig, HealthCheckConfig, LimitsConfig, UpstreamConfig,
    };

    fn make_pool(urls: &[&str]) -> Arc<UpstreamPool> {
        let cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: None,
            mode: BalanceMode::RoundRobin,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            domain_policy: DomainPolicyConfig::default(),
            limits: LimitsConfig::default(),
            access_log: false,
            upstream: urls
                .iter()
                .map(|u| UpstreamConfig {
                    url: u.to_string(),
                    weight: 1,
                    priority: 100,
                    username: None,
                    password: None,
                })
                .collect(),
        };
        UpstreamPool::from_config(&cfg)
    }

    #[test]
    fn round_robin_cycles_through_online() {
        let pool = make_pool(&["http://a:1", "http://b:2", "http://c:3"]);
        // Collect 6 selections — should visit all 3 upstreams
        let mut seen = std::collections::HashSet::new();
        for _ in 0..6 {
            let (_, e) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
            seen.insert(e.config.url.clone());
        }
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn offline_upstream_is_skipped() {
        let pool = make_pool(&["http://a:1", "http://b:2"]);
        // Mark first upstream offline
        let (_, first) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
        first.mark_offline();

        // All subsequent selections should be the other one
        for _ in 0..10 {
            let (_, e) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
            assert_ne!(
                e.config.url, first.config.url,
                "offline upstream was selected"
            );
        }
    }

    #[test]
    fn no_online_upstream_returns_none() {
        let pool = make_pool(&["http://a:1"]);
        let (_, e) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
        e.mark_offline();
        assert!(pool.select(BalanceMode::RoundRobin, &[]).is_none());
    }

    #[test]
    fn exclude_skips_specified_indices() {
        let pool = make_pool(&["http://a:1", "http://b:2", "http://c:3"]);
        let result = pool.select(BalanceMode::RoundRobin, &[0, 1]);
        let (idx, _) = result.unwrap();
        assert_eq!(idx, 2);
    }

    #[test]
    fn mark_online_restores_offline_upstream() {
        let pool = make_pool(&["http://a:1"]);
        let (_, e) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
        e.mark_offline();
        assert!(pool.select(BalanceMode::RoundRobin, &[]).is_none());
        e.mark_online();
        assert!(pool.select(BalanceMode::RoundRobin, &[]).is_some());
    }

    #[test]
    fn best_mode_picks_lowest_score() {
        let pool = make_pool(&["http://a:1", "http://b:2"]);
        let entries = pool.entries.lock();
        // Give "b" a very high latency to force "a" to win
        entries[1]
            .latency_ema_ms
            .store(9999, std::sync::atomic::Ordering::Relaxed);
        drop(entries);
        let (idx, _) = pool.select(BalanceMode::Best, &[]).unwrap();
        assert_eq!(idx, 0, "should pick upstream with lower score");
    }

    #[test]
    fn reload_preserves_state_for_existing_upstream() {
        let pool = make_pool(&["http://a:1", "http://b:2"]);
        // Mark "a" offline
        let (_, a) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
        a.mark_offline();

        // Reload with same upstreams
        let new_cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: None,
            mode: BalanceMode::RoundRobin,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            domain_policy: DomainPolicyConfig::default(),
            limits: LimitsConfig::default(),
            access_log: false,
            upstream: vec![
                UpstreamConfig {
                    url: "http://a:1".to_string(),
                    weight: 1,
                    priority: 100,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://b:2".to_string(),
                    weight: 1,
                    priority: 100,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://c:3".to_string(),
                    weight: 1,
                    priority: 100,
                    username: None,
                    password: None,
                },
            ],
        };
        pool.reload(&new_cfg);

        // "a" should still be offline (state preserved)
        let entries = pool.entries.lock();
        let a_entry = entries
            .iter()
            .find(|e| e.config.url == "http://a:1")
            .unwrap();
        assert!(
            !a_entry.is_online(),
            "state should be preserved after reload"
        );
        // "c" should be a new online entry
        let c_entry = entries
            .iter()
            .find(|e| e.config.url == "http://c:3")
            .unwrap();
        assert!(c_entry.is_online());
    }

    #[test]
    fn weighted_round_robin_respects_weights() {
        let cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: None,
            mode: BalanceMode::RoundRobin,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            domain_policy: DomainPolicyConfig::default(),
            limits: LimitsConfig::default(),
            access_log: false,
            upstream: vec![
                UpstreamConfig {
                    url: "http://a:1".to_string(),
                    weight: 1,
                    priority: 100,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://b:2".to_string(),
                    weight: 3,
                    priority: 100,
                    username: None,
                    password: None,
                },
            ],
        };
        let pool = UpstreamPool::from_config(&cfg);

        let mut counts = std::collections::HashMap::new();
        for _ in 0..400 {
            let (_, e) = pool.select(BalanceMode::RoundRobin, &[]).unwrap();
            *counts.entry(e.config.url.clone()).or_insert(0u32) += 1;
        }
        // "b" has 3x weight → should get ~3× more traffic
        let a_count = counts["http://a:1"];
        let b_count = counts["http://b:2"];
        let ratio = b_count as f64 / a_count as f64;
        assert!(
            ratio > 2.5 && ratio < 3.5,
            "expected ~3:1 ratio, got a={a_count} b={b_count} (ratio={ratio:.2})"
        );
    }

    #[test]
    fn priority_mode_prefers_lowest_priority_value() {
        let cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: None,
            mode: BalanceMode::Priority,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            domain_policy: DomainPolicyConfig::default(),
            limits: LimitsConfig::default(),
            access_log: false,
            upstream: vec![
                UpstreamConfig {
                    url: "http://a:1".to_string(),
                    weight: 1,
                    priority: 50,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://b:2".to_string(),
                    weight: 1,
                    priority: 10,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://c:3".to_string(),
                    weight: 1,
                    priority: 30,
                    username: None,
                    password: None,
                },
            ],
        };
        let pool = UpstreamPool::from_config(&cfg);
        let (idx, entry) = pool.select(BalanceMode::Priority, &[]).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(entry.config.url, "http://b:2");
    }

    #[test]
    fn priority_mode_falls_back_to_next_online_priority() {
        let cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: None,
            mode: BalanceMode::Priority,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            domain_policy: DomainPolicyConfig::default(),
            limits: LimitsConfig::default(),
            access_log: false,
            upstream: vec![
                UpstreamConfig {
                    url: "http://a:1".to_string(),
                    weight: 1,
                    priority: 1,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://b:2".to_string(),
                    weight: 1,
                    priority: 2,
                    username: None,
                    password: None,
                },
            ],
        };
        let pool = UpstreamPool::from_config(&cfg);
        let (_, first) = pool.select(BalanceMode::Priority, &[]).unwrap();
        assert_eq!(first.config.url, "http://a:1");
        first.mark_offline();

        let (_, second) = pool.select(BalanceMode::Priority, &[]).unwrap();
        assert_eq!(second.config.url, "http://b:2");
    }

    #[test]
    fn reload_updates_priority_for_existing_upstream() {
        let cfg = Config {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: None,
            mode: BalanceMode::Priority,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            domain_policy: DomainPolicyConfig::default(),
            limits: LimitsConfig::default(),
            access_log: false,
            upstream: vec![
                UpstreamConfig {
                    url: "http://a:1".to_string(),
                    weight: 1,
                    priority: 100,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://b:2".to_string(),
                    weight: 1,
                    priority: 200,
                    username: None,
                    password: None,
                },
            ],
        };
        let pool = UpstreamPool::from_config(&cfg);
        let (_, first_before) = pool.select(BalanceMode::Priority, &[]).unwrap();
        assert_eq!(first_before.config.url, "http://a:1");

        let reloaded = Config {
            listen: "127.0.0.1:8080".to_string(),
            admin_listen: None,
            mode: BalanceMode::Priority,
            reload_interval_secs: 0,
            health_check: HealthCheckConfig::default(),
            domain_policy: DomainPolicyConfig::default(),
            limits: LimitsConfig::default(),
            access_log: false,
            upstream: vec![
                UpstreamConfig {
                    url: "http://a:1".to_string(),
                    weight: 1,
                    priority: 300,
                    username: None,
                    password: None,
                },
                UpstreamConfig {
                    url: "http://b:2".to_string(),
                    weight: 1,
                    priority: 50,
                    username: None,
                    password: None,
                },
            ],
        };
        pool.reload(&reloaded);
        let (_, first_after) = pool.select(BalanceMode::Priority, &[]).unwrap();
        assert_eq!(first_after.config.url, "http://b:2");
    }
}
