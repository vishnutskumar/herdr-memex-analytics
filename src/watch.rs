use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;

use crate::agents;
use crate::config::{self, Config, PluginPaths};
use crate::live::{self, FleetSnapshot};
use crate::notify;
use crate::report::{self, Filters, Report};
use crate::tips;

/// A pane.output_matched hit recorded by the event hook; refresh_tips turns
/// repeated hits into an urgent retry-loop tip.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LoopAlert {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub first_at_ms: u64,
    #[serde(default)]
    pub last_at_ms: u64,
}

/// All panes' output-match streaks, keyed by pane id.
pub type LoopAlerts = BTreeMap<String, LoopAlert>;

/// Output matches within this window are the same loop; older entries are
/// stale and pruned.
pub const LOOP_WINDOW_MS: u64 = 10 * 60 * 1000;
const LOOP_TIP_MIN_COUNT: u64 = 3;
/// Revision advance per sample above which a long turn is considered actively
/// producing output rather than stuck.
pub const PRODUCING_CHURN_MIN: u64 = 50;

fn loop_alerts_path(paths: &PluginPaths) -> std::path::PathBuf {
    paths.state_dir.join("loop-alerts.json")
}

pub fn load_loop_alerts(paths: &PluginPaths) -> LoopAlerts {
    std::fs::read(loop_alerts_path(paths))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn store_loop_alerts(paths: &PluginPaths, alerts: &LoopAlerts) -> Result<()> {
    config::store_json(loop_alerts_path(paths), alerts)
}

/// Budget-alert bookkeeping, persisted in alerts.json so each alert fires at
/// most once per local day (daily budget) / hour (burn rate).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AlertState {
    #[serde(default)]
    pub last_daily_date: Option<String>,
    #[serde(default)]
    pub last_burn_at_ms: Option<u64>,
}

fn alert_state_path(paths: &PluginPaths) -> std::path::PathBuf {
    paths.state_dir.join("alerts.json")
}

fn load_alert_state(paths: &PluginPaths) -> AlertState {
    std::fs::read(alert_state_path(paths))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store_alert_state(paths: &PluginPaths, state: &AlertState) -> Result<()> {
    config::store_json(alert_state_path(paths), state)
}

/// Pure: which budget tips fire now, and the resulting alert state. The daily
/// tip fires once per local date; the burn-rate tip at most once per hour.
pub fn evaluate_budget_alerts(
    today_cost_usd: Option<f64>,
    burn_rate_usd_per_hr: Option<f64>,
    cfg: &Config,
    state: &AlertState,
    now_ms: u64,
    today: &str,
) -> (Vec<agents::Tip>, AlertState) {
    let mut next = state.clone();
    let mut tips = Vec::new();
    if let Some(limit) = cfg.daily_cost_usd
        && let Some(cost) = today_cost_usd
        && cost > limit
        && next.last_daily_date.as_deref() != Some(today)
    {
        tips.push(agents::Tip {
            pane_id: "budget".into(),
            message: format!("today's cost ${cost:.2} passed the ${limit:.2} daily budget"),
            urgent: true,
        });
        next.last_daily_date = Some(today.to_string());
    }
    if let Some(rate) = burn_rate_usd_per_hr
        && rate > cfg.block_burn_rate_usd_hr
        && state
            .last_burn_at_ms
            .is_none_or(|at| now_ms.saturating_sub(at) >= 3_600_000)
    {
        tips.push(agents::Tip {
            pane_id: "budget".into(),
            message: format!(
                "burn rate ${rate:.2}/hr exceeds ${:.2}/hr",
                cfg.block_burn_rate_usd_hr
            ),
            urgent: true,
        });
        next.last_burn_at_ms = Some(now_ms);
    }
    (tips, next)
}

/// Pure: urgent retry-loop tips for panes with enough fresh matches; stale
/// entries (no match within LOOP_WINDOW_MS) are dropped from the returned map.
pub fn merge_loop_alerts(alerts: &LoopAlerts, now_ms: u64) -> (Vec<agents::Tip>, LoopAlerts) {
    let mut kept = BTreeMap::new();
    for (pane, alert) in alerts {
        if now_ms.saturating_sub(alert.last_at_ms) < LOOP_WINDOW_MS {
            kept.insert(pane.clone(), alert.clone());
        }
    }
    let mut tips = Vec::new();
    for (pane, alert) in &kept {
        if alert.count >= LOOP_TIP_MIN_COUNT {
            tips.push(agents::Tip {
                pane_id: pane.clone(),
                message: format!(
                    "retry loop suspected ({} output matches) — check the pane",
                    alert.count
                ),
                urgent: true,
            });
        }
    }
    (tips, kept)
}

/// Pure: a long-turn tip on a pane whose revision keeps advancing means the
/// agent is producing, not stuck — swap the advice for a calmer progress note.
pub fn suppress_churning_tips(
    tips: Vec<agents::Tip>,
    churn: &BTreeMap<String, u64>,
) -> Vec<agents::Tip> {
    tips.into_iter()
        .map(|mut tip| {
            if !tip.urgent
                && churn
                    .get(&tip.pane_id)
                    .is_some_and(|&d| d >= PRODUCING_CHURN_MIN)
            {
                tip.message = "still producing output — long turn in progress".into();
            }
            tip
        })
        .collect()
}

/// Daemon loop: recompute the snapshot on a fixed cadence, like memex's periodic
/// reindex, then evaluate realtime tips from the agent states the event hook
/// maintains. Never exits on a failed cycle; a transient error just skips one
/// refresh.
pub fn run(mut filters: Filters, paths: &PluginPaths, interval: Duration) -> Result<()> {
    // The daemon rescans every cycle anyway; the memo just bridges the two
    // gathers inside one interval window.
    filters.memo_ttl_ms = interval.as_millis() as u64 * 2;
    eprintln!(
        "analytics watch: scanning every {}s, snapshot at {}",
        interval.as_secs(),
        config::snapshot_path(paths).display()
    );
    loop {
        match report::gather(&filters, Some(paths)) {
            Ok(mut rep) => {
                rep.fleet = live::sample(paths);
                if let Err(err) = config::write_snapshot(paths, &rep) {
                    eprintln!("analytics watch: snapshot write failed: {err:#}");
                }
                refresh_tips(paths, rep.fleet.as_ref(), Some(&rep));
            }
            Err(err) => {
                eprintln!("analytics watch: scan failed: {err:#}");
                let snap = config::read_snapshot(paths);
                refresh_tips(paths, None, snap.as_ref());
            }
        }
        agents::rotate_logs(paths, agents::TURN_RETENTION_MS);
        std::thread::sleep(interval);
    }
}

/// Evaluate tips across every herdr session's state, notify the urgent ones
/// (rate limited by last_notified_ms), and publish the full list for the
/// report pane. Also folds in retry-loop alerts from the event hook and
/// budget alerts from the latest report numbers.
fn refresh_tips(paths: &PluginPaths, fleet: Option<&FleetSnapshot>, rep: Option<&Report>) {
    let now = report::now_ms();
    let mut due: Vec<agents::Tip> = Vec::new();
    let churn: BTreeMap<String, u64> = fleet
        .map(|f| {
            f.churn
                .iter()
                .map(|c| (c.pane_id.clone(), c.revision_delta))
                .collect()
        })
        .unwrap_or_default();

    // Each herdr session has its own state file; pane ids are session-scoped.
    for (session, mut states) in agents::load_all_states(paths) {
        let session_tips = suppress_churning_tips(agents::evaluate_tips(&states, now), &churn);
        for tip in &session_tips {
            if tip.urgent {
                notify::show(paths, &tip.message);
                if let Some(s) = states.get_mut(&tip.pane_id) {
                    s.last_notified_ms = Some(now);
                }
            }
        }
        if !session_tips.is_empty() {
            for mut tip in session_tips {
                tip.message = format!("[{session}] {}", tip.message);
                due.push(tip);
            }
            if let Err(err) = agents::store_states(paths, &session, &states) {
                eprintln!("analytics watch: state write failed: {err:#}");
            }
        }
    }

    let loaded = load_loop_alerts(paths);
    let (loop_tips, pruned) = merge_loop_alerts(&loaded, now);
    if pruned.len() != loaded.len()
        && let Err(err) = store_loop_alerts(paths, &pruned)
    {
        eprintln!("analytics watch: loop-alerts write failed: {err:#}");
    }
    for tip in loop_tips {
        notify::show(paths, &tip.message);
        due.push(tip);
    }

    let cfg = Config::load(paths);
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let (budget_tips, alert_state) = evaluate_budget_alerts(
        rep.and_then(|r| r.today_cost_usd),
        rep.and_then(|r| r.burn_rate_usd_per_hr),
        &cfg,
        &load_alert_state(paths),
        now,
        &today,
    );
    if let Err(err) = store_alert_state(paths, &alert_state) {
        eprintln!("analytics watch: alerts write failed: {err:#}");
    }
    for tip in budget_tips {
        notify::show(paths, &tip.message);
        due.push(tip);
    }

    let published = tips::Tips {
        generated_at_ms: now,
        items: due,
    };
    if let Err(err) = tips::store(paths, &published) {
        eprintln!("analytics watch: tips write failed: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn cfg(daily: Option<f64>, burn: f64) -> Config {
        Config {
            scan_interval_secs: 900,
            daily_cost_usd: daily,
            block_burn_rate_usd_hr: burn,
            context_bloat_tokens: 100_000,
        }
    }

    fn tip(pane: &str, message: &str, urgent: bool) -> agents::Tip {
        agents::Tip {
            pane_id: pane.into(),
            message: message.into(),
            urgent,
        }
    }

    fn tmp_paths(label: &str) -> PluginPaths {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "analytics-watch-{label}-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            nanos
        ));
        PluginPaths { state_dir: dir }
    }

    #[test]
    fn daily_budget_alert_fires_once_per_local_day() {
        let c = cfg(Some(10.0), 15.0);
        let (tips, s1) = evaluate_budget_alerts(
            Some(12.0),
            None,
            &c,
            &AlertState::default(),
            1_000,
            "2026-08-21",
        );
        assert_eq!(tips.len(), 1);
        assert!(tips[0].urgent);
        assert_eq!(s1.last_daily_date.as_deref(), Some("2026-08-21"));

        // Same local day, cost climbs further: suppressed.
        let (tips, s2) = evaluate_budget_alerts(Some(20.0), None, &c, &s1, 2_000, "2026-08-21");
        assert!(tips.is_empty());

        // Next local day: fires again.
        let (tips, _) = evaluate_budget_alerts(Some(20.0), None, &c, &s2, 3_000, "2026-08-22");
        assert_eq!(tips.len(), 1);
    }

    #[test]
    fn daily_budget_alert_silent_under_limit_or_without_limit_or_cost() {
        let c = cfg(Some(10.0), 15.0);
        let (tips, _) =
            evaluate_budget_alerts(Some(9.99), None, &c, &AlertState::default(), 0, "d");
        assert!(tips.is_empty());
        let (tips, _) = evaluate_budget_alerts(
            Some(99.0),
            None,
            &cfg(None, 15.0),
            &AlertState::default(),
            0,
            "d",
        );
        assert!(tips.is_empty());
        let (tips, _) = evaluate_budget_alerts(None, None, &c, &AlertState::default(), 0, "d");
        assert!(tips.is_empty());
    }

    #[test]
    fn burn_rate_alert_fires_at_most_once_per_hour() {
        let c = cfg(None, 15.0);
        let (tips, s1) =
            evaluate_budget_alerts(None, Some(20.0), &c, &AlertState::default(), 0, "d");
        assert_eq!(tips.len(), 1);
        assert!(tips[0].urgent);

        // 59 minutes later: still suppressed.
        let (tips, _) = evaluate_budget_alerts(None, Some(25.0), &c, &s1, 59 * 60 * 1000, "d");
        assert!(tips.is_empty());

        // One hour after the last alert: fires again.
        let (tips, _) = evaluate_budget_alerts(None, Some(25.0), &c, &s1, 3_600_000, "d");
        assert_eq!(tips.len(), 1);
    }

    #[test]
    fn burn_rate_alert_silent_at_or_under_threshold() {
        let c = cfg(None, 15.0);
        for rate in [0.0, 14.99, 15.0] {
            let (tips, _) =
                evaluate_budget_alerts(None, Some(rate), &c, &AlertState::default(), 0, "d");
            assert!(tips.is_empty(), "rate {rate} must not alert");
        }
    }

    #[test]
    fn loop_alerts_tip_when_frequent_and_fresh_and_prune_stale_entries() {
        let mut alerts = BTreeMap::new();
        alerts.insert(
            "w1:p1".into(),
            LoopAlert {
                count: 3,
                first_at_ms: 0,
                last_at_ms: 1_000,
            },
        );
        alerts.insert(
            "w1:p2".into(),
            LoopAlert {
                count: 2,
                first_at_ms: 0,
                last_at_ms: 1_000,
            },
        );
        alerts.insert(
            "old:p3".into(),
            LoopAlert {
                count: 9,
                first_at_ms: 0,
                last_at_ms: 0,
            },
        );
        let now = LOOP_WINDOW_MS + 500;
        let (tips, kept) = merge_loop_alerts(&alerts, now);
        assert_eq!(tips.len(), 1);
        assert_eq!(tips[0].pane_id, "w1:p1");
        assert!(tips[0].urgent);
        assert!(tips[0].message.contains("retry loop"));
        // Stale entry pruned; fresh but infrequent entry kept without a tip.
        assert!(!kept.contains_key("old:p3"));
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn loop_alerts_round_trip_and_corrupt_file_falls_back_to_default() {
        let paths = tmp_paths("loopalerts");
        let mut alerts = BTreeMap::new();
        alerts.insert(
            "w1:p1".into(),
            LoopAlert {
                count: 4,
                first_at_ms: 10,
                last_at_ms: 20,
            },
        );
        store_loop_alerts(&paths, &alerts).unwrap();
        let loaded = load_loop_alerts(&paths);
        assert_eq!(loaded, alerts);

        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(loop_alerts_path(&paths), b"{ truncated").unwrap();
        assert!(load_loop_alerts(&paths).is_empty());
        std::fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn long_turn_tip_on_churning_pane_becomes_progress_note() {
        let tips = vec![
            tip("w1:p1", "omp has been working 12m on one turn", false),
            tip("w1:p2", "claude has been working 11m on one turn", false),
            tip("w1:p3", "codex has been blocked 6m", true),
        ];
        let churn = BTreeMap::from([("w1:p1".to_string(), 50), ("w1:p3".to_string(), 99)]);
        let out = suppress_churning_tips(tips, &churn);
        assert!(out[0].message.contains("still producing output"));
        assert!(!out[0].urgent);
        // Below the churn threshold: original advice kept.
        assert_eq!(out[1].message, "claude has been working 11m on one turn");
        // Urgent tips are never rewritten, even at high churn.
        assert_eq!(out[2].message, "codex has been blocked 6m");
    }
}
