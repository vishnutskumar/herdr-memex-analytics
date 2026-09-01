use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::agents;
use crate::config::{self, Config, PluginPaths};
use crate::live::{self, FleetSnapshot};
use crate::notify;
use crate::report::{self, Filters, Report};
use crate::tips;

/// A retry-pattern hit recorded by the daemon's output scan; refresh_tips
/// turns repeated hits into an urgent retry-loop tip.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LoopAlert {
    #[serde(default)]
    pub count: u64,
    #[serde(default)]
    pub first_at_ms: u64,
    #[serde(default)]
    pub last_at_ms: u64,
    /// When the urgent tip for this streak last notified; None has never fired.
    #[serde(default)]
    pub last_notified_ms: Option<u64>,
}

/// All panes' output-match streaks, keyed by pane id.
pub type LoopAlerts = BTreeMap<String, LoopAlert>;

/// Output matches within this window are the same loop; older entries are
/// stale and pruned.
pub const LOOP_WINDOW_MS: u64 = 10 * 60 * 1000;
const LOOP_TIP_MIN_COUNT: u64 = 3;
/// Milliseconds between re-notifications of the same retry-loop tip; same
/// cadence as the blocked nag.
const RETRY_NAG_MS: u64 = 15 * 60 * 1000;
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
///
/// Returns `(tips, notified, kept)`: `tips` is published while the streak
/// stays fresh (the report pane must keep showing it), while `notified` lists
/// the panes whose re-notification is due this call (at most once per
/// RETRY_NAG_MS); their `last_notified_ms` is stamped in `kept` so the caller
/// can persist it.
pub fn merge_loop_alerts(
    alerts: &LoopAlerts,
    now_ms: u64,
) -> (Vec<agents::Tip>, Vec<String>, LoopAlerts) {
    let mut kept = BTreeMap::new();
    for (pane, alert) in alerts {
        if now_ms.saturating_sub(alert.last_at_ms) < LOOP_WINDOW_MS {
            kept.insert(pane.clone(), alert.clone());
        }
    }
    let mut tips = Vec::new();
    let mut notified = Vec::new();
    for (pane, alert) in kept.iter_mut() {
        if alert.count < LOOP_TIP_MIN_COUNT {
            continue;
        }
        tips.push(agents::Tip {
            pane_id: pane.clone(),
            message: format!(
                "retry loop suspected ({} output matches) — check the pane",
                alert.count
            ),
            urgent: true,
        });
        if due_for_loop_nag(alert, now_ms) {
            notified.push(pane.clone());
            alert.last_notified_ms = Some(now_ms);
        }
    }
    (tips, notified, kept)
}

fn due_for_loop_nag(alert: &LoopAlert, now_ms: u64) -> bool {
    match alert.last_notified_ms {
        None => true,
        Some(at) => now_ms.saturating_sub(at) >= RETRY_NAG_MS,
    }
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

/// One full daemon cycle: rescan memex, refresh the snapshot, re-evaluate
/// tips. A failed cycle is logged and skipped; the daemon keeps running.
fn scan_cycle(filters: &mut Filters, paths: &PluginPaths) {
    match report::gather(filters, Some(paths)) {
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
}

/// Daemon loop: recompute the snapshot on a fixed cadence, like memex's
/// periodic reindex, then evaluate realtime tips from the agent states the
/// event hook maintains. When `retry_patterns` is configured, recognized
/// agent panes are additionally polled for retry-loop signals on a much
/// faster cadence (herdr's plugin event hooks do not expose pane output
/// events, so the daemon owns that detection). Never exits on a failed
/// cycle; a transient error just skips one refresh.
pub fn run(
    mut filters: Filters,
    paths: &PluginPaths,
    interval: Duration,
    cfg: &Config,
) -> Result<()> {
    // The daemon rescans every cycle anyway; the memo just bridges the two
    // gathers inside one interval window.
    filters.memo_ttl_ms = interval.as_millis() as u64 * 2;
    eprintln!(
        "analytics watch: scanning every {}s, snapshot at {}",
        interval.as_secs(),
        config::snapshot_path(paths).display()
    );
    scan_cycle(&mut filters, paths);

    // Retry-loop polling needs a floor: below 1 s the herdr CLI round trip is
    // the cost, not the signal.
    let retry_every = if cfg.retry_patterns.is_empty() {
        None
    } else {
        Some(Duration::from_millis(cfg.retry_scan_interval_ms.max(1_000)))
    };
    let mut next_scan = Instant::now() + interval;
    let mut next_retry = retry_every.map(|d| Instant::now() + d);
    loop {
        // Wake at the earlier of the two due times (scan always has one).
        let wake = next_retry.map_or(next_scan, |r| r.min(next_scan));
        if wake > Instant::now() {
            std::thread::sleep(wake - Instant::now());
        }
        let now = Instant::now();
        if next_retry.is_some_and(|r| r <= now)
            && let Some(retry) = retry_every
        {
            let hits = scan_live_output(paths, cfg);
            if let Err(err) = store_loop_alerts(paths, &hits) {
                eprintln!("analytics watch: loop-alerts write failed: {err:#}");
            }
            let snap = config::read_snapshot(paths);
            refresh_tips(paths, None, snap.as_ref());
            next_retry = Some(now + retry);
        }
        if next_scan <= now {
            scan_cycle(&mut filters, paths);
            next_scan = now + interval;
        }
    }
}

/// Scan recent output of every recognized agent pane for retry patterns and
/// return the updated ledger. Panes without a recognized agent are skipped —
/// ordinary shells would false-positive. A pane with no match this cycle
/// keeps its streak untouched; the 10-minute window prunes it later.
fn scan_live_output(paths: &PluginPaths, cfg: &Config) -> LoopAlerts {
    let panes = live::agent_panes();
    if panes.is_empty() {
        return load_loop_alerts(paths);
    }
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let mut alerts = load_loop_alerts(paths);
    let now = report::now_ms();
    for pane in panes.keys() {
        let Some(text) = read_recent_output(&bin, pane, cfg.retry_window_lines) else {
            continue;
        };
        let hits = count_hits(&text, &cfg.retry_patterns);
        for _ in 0..hits {
            agents::record_output_match(&mut alerts, pane, now);
        }
    }
    alerts
}

/// One `pane read` of recent output; any failure is a skipped pane, never an
/// aborted scan.
fn read_recent_output(bin: &str, pane: &str, lines: u64) -> Option<String> {
    let out = std::process::Command::new(bin)
        .args([
            "pane",
            "read",
            pane,
            "--source",
            "recent",
            "--lines",
            &lines.to_string(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// At most one hit per pattern per read, so a pattern repeated across the
/// whole window still counts once per scan cycle.
fn count_hits(text: &str, patterns: &[String]) -> u64 {
    patterns
        .iter()
        .filter(|p| text.contains(p.as_str()))
        .count() as u64
}

/// Evaluate tips across every herdr session's state, notify the urgent ones
/// (rate limited by last_notified_ms), and publish the full list for the
/// report pane. Also folds in retry-loop alerts from the daemon's output scan
/// and budget alerts from the latest report numbers.
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
    let (loop_tips, loop_notified, pruned) = merge_loop_alerts(&loaded, now);
    // Persist when entries were pruned or a nag fired: merge_loop_alerts
    // stamps last_notified_ms, which must survive the in-window period or
    // the urgent tip re-notifies every poll cycle.
    let ledger_changed = pruned.len() != loaded.len() || !loop_notified.is_empty();
    if ledger_changed && let Err(err) = store_loop_alerts(paths, &pruned) {
        eprintln!("analytics watch: loop-alerts write failed: {err:#}");
    }
    // The tip is published while the streak is fresh (the report pane keeps
    // showing it); the herdr notification is rate limited per pane.
    for tip in &loop_tips {
        if loop_notified.contains(&tip.pane_id) {
            notify::show(paths, &tip.message);
        }
        due.push(tip.clone());
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
            daily_cost_usd: daily,
            block_burn_rate_usd_hr: burn,
            ..Default::default()
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
                last_notified_ms: None,
            },
        );
        alerts.insert(
            "w1:p2".into(),
            LoopAlert {
                count: 2,
                first_at_ms: 0,
                last_at_ms: 1_000,
                last_notified_ms: None,
            },
        );
        alerts.insert(
            "old:p3".into(),
            LoopAlert {
                count: 9,
                first_at_ms: 0,
                last_at_ms: 0,
                last_notified_ms: None,
            },
        );
        let now = LOOP_WINDOW_MS + 500;
        let (tips, notified, kept) = merge_loop_alerts(&alerts, now);
        assert_eq!(tips.len(), 1);
        assert_eq!(tips[0].pane_id, "w1:p1");
        assert!(tips[0].urgent);
        assert!(tips[0].message.contains("retry loop"));
        // First sighting of a fresh streak notifies immediately.
        assert_eq!(notified, vec!["w1:p1".to_string()]);
        assert_eq!(kept.get("w1:p1").unwrap().last_notified_ms, Some(now));
        // Stale entry pruned; fresh but infrequent entry kept without a tip.
        assert!(!kept.contains_key("old:p3"));
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn loop_alerts_tip_stays_published_but_notification_is_rate_limited() {
        let now = RETRY_NAG_MS + 100_000;
        let mut alerts = BTreeMap::new();
        alerts.insert(
            "w1:p1".into(),
            LoopAlert {
                count: 5,
                first_at_ms: 0,
                last_at_ms: now,
                last_notified_ms: Some(now - 1), // nag window still open by 1 tick
            },
        );
        // Inside the window by one tick: re-notification is NOT due.
        let (tips, notified, kept) = merge_loop_alerts(&alerts, now);
        assert_eq!(
            tips.len(),
            1,
            "tip stays published while the streak is fresh"
        );
        assert!(
            notified.is_empty(),
            "no re-notification inside the nag window"
        );
        assert_eq!(kept.get("w1:p1").unwrap().last_notified_ms, Some(now - 1));

        // A fresh match keeps the streak alive; once the nag window elapses
        // the nag fires again and the stamp advances.
        let later = now + RETRY_NAG_MS;
        alerts.get_mut("w1:p1").unwrap().last_at_ms = later - 1_000;
        let (tips, notified, kept) = merge_loop_alerts(&alerts, later);
        assert_eq!(tips.len(), 1);
        assert_eq!(notified, vec!["w1:p1".to_string()]);
        assert_eq!(kept.get("w1:p1").unwrap().last_notified_ms, Some(later));
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
                last_notified_ms: None,
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
