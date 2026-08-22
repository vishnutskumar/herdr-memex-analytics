use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;

use crate::config::PluginPaths;
/// How long an agent must sit in `blocked` before the daemon re-nags (the event
/// hook already notifies on the transition itself).
pub const BLOCKED_NAG_SECS: u64 = 15 * 60;
/// A single agent turn running longer than this is worth flagging.
pub const LONG_TURN_SECS: u64 = 45 * 60;
/// Blocked panes get their first daemon tip after this long.
pub const BLOCKED_TIP_SECS: u64 = 5 * 60;
/// How long completed-turn and gap logs are kept before rotation.
pub const TURN_RETENTION_MS: u64 = 90 * 24 * 3600 * 1000;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentState {
    pub pane_id: String,
    pub agent: Option<String>,
    pub status: String,
    pub since_ms: u64,
    #[serde(default)]
    pub last_notified_ms: Option<u64>,
}

pub type AgentStates = BTreeMap<String, AgentState>;

#[derive(Deserialize, Clone, Debug)]
pub struct Transition {
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub status: String,
    pub at_ms: u64,
}

/// What the event hook should do after recording a transition.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct TransitionResult {
    /// Agent just entered `blocked`: notify immediately.
    pub entered_blocked: bool,
    /// A working segment ended (idle/done = finished turn, blocked = paused to
    /// ask); wall time the segment took.
    pub completed_turn_ms: Option<u64>,
    /// The agent came back `working` after sitting `blocked`: wall time it sat
    /// there (the human-response gap).
    pub resumed_from_blocked_ms: Option<u64>,
}

/// Session identity: each herdr session has its own socket, so the socket path
/// namespaces state. Pane ids are only unique within a session; without this,
/// two sessions running `w1:p2` would clobber each other's status.
pub fn session_key() -> String {
    let raw = std::env::var("HERDR_SOCKET_PATH").unwrap_or_default();
    let sanitized: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = sanitized.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed
    }
}

pub fn states_path(paths: &PluginPaths, key: &str) -> std::path::PathBuf {
    paths.state_dir.join(format!("agent-states-{key}.json"))
}

pub fn load_states(paths: &PluginPaths, key: &str) -> AgentStates {
    fs::read(states_path(paths, key))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn store_states(paths: &PluginPaths, key: &str, states: &AgentStates) -> Result<()> {
    crate::config::store_json(states_path(paths, key), states)
}

/// Every session's states found on disk, for the daemon's cross-session view.
pub fn load_all_states(paths: &PluginPaths) -> Vec<(String, AgentStates)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&paths.state_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(key) = name
            .strip_prefix("agent-states-")
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if let Ok(bytes) = fs::read(entry.path())
            && let Ok(states) = serde_json::from_slice::<AgentStates>(&bytes)
        {
            out.push((key.to_string(), states));
        }
    }
    out
}

/// Record a status transition and report what should happen. A working segment
/// closes on idle/done (a finished turn) or on blocked (the agent paused to ask).
pub fn apply_transition(states: &mut AgentStates, t: &Transition) -> TransitionResult {
    let mut result = TransitionResult::default();
    if let Some(prev) = states.get(&t.pane_id) {
        if prev.status == "working" && matches!(t.status.as_str(), "idle" | "done" | "blocked") {
            result.completed_turn_ms = Some(t.at_ms.saturating_sub(prev.since_ms));
        }
        if prev.status == "blocked" && t.status == "working" {
            result.resumed_from_blocked_ms = Some(t.at_ms.saturating_sub(prev.since_ms));
        }
    }
    if t.status == "blocked" {
        result.entered_blocked = true;
    }
    states.insert(
        t.pane_id.clone(),
        AgentState {
            pane_id: t.pane_id.clone(),
            agent: t.agent.clone(),
            status: t.status.clone(),
            since_ms: t.at_ms,
            last_notified_ms: None,
        },
    );
    result
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AgentTurnStats {
    pub agent: String,
    pub completed: u64,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TurnStats {
    pub completed: u64,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    /// Per-agent breakdown, busiest agent first.
    pub by_agent: Vec<AgentTurnStats>,
    /// Share of turns that ended in `blocked` (the agent had to stop and ask).
    pub intervention_rate: Option<f64>,
    pub zero_intervention_rate: Option<f64>,
    /// Quick follow-up turns right after a blocked turn: signs of rework.
    pub rework_turns: u64,
    /// How long humans took to unblock agents (from gaps.jsonl).
    pub human_latency_p50_ms: Option<u64>,
    pub human_latency_total_ms: u64,
}

/// One line of turns.jsonl. Old lines without `ended_by` parse as plain
/// finished turns (`idle`).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TurnRecord {
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub finished_at_ms: u64,
    pub duration_ms: u64,
    #[serde(default = "default_ended_by")]
    pub ended_by: String,
}

fn default_ended_by() -> String {
    "idle".to_string()
}

/// One line of gaps.jsonl: how long a pane sat blocked before resuming work.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GapRecord {
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    pub started_at_ms: u64,
    pub duration_ms: u64,
}

pub fn turns_path(paths: &PluginPaths) -> std::path::PathBuf {
    paths.state_dir.join("turns.jsonl")
}

pub fn gaps_path(paths: &PluginPaths) -> std::path::PathBuf {
    paths.state_dir.join("gaps.jsonl")
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Vec<T> {
    fs::read(path)
        .map(|bytes| {
            bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .filter_map(|line| serde_json::from_slice(line).ok())
                .collect()
        })
        .unwrap_or_default()
}
/// Nearest-rank percentile over an ascending-sorted slice.
fn nearest_rank(sorted: &[u64], pct: usize) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = sorted.len() * pct;
    let rank = rank.div_ceil(100).max(1);
    Some(sorted[rank - 1])
}

const REWORK_WINDOW_MS: u64 = 10 * 60_000;
const SHORT_TURN_MS: u64 = 5 * 60_000;

/// Aggregate turn efficiency from turns.jsonl + gaps.jsonl. None when neither
/// log has any records yet.
pub fn turn_stats(paths: &PluginPaths) -> Option<TurnStats> {
    let turns: Vec<TurnRecord> = read_jsonl(&turns_path(paths));
    let gaps: Vec<GapRecord> = read_jsonl(&gaps_path(paths));
    if turns.is_empty() && gaps.is_empty() {
        return None;
    }

    let mut durations: Vec<u64> = turns.iter().map(|t| t.duration_ms).collect();
    durations.sort_unstable();

    let mut by_agent: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for t in &turns {
        let label = t.agent.as_deref().unwrap_or("agent");
        by_agent.entry(label).or_default().push(t.duration_ms);
    }
    let mut by_agent: Vec<AgentTurnStats> = by_agent
        .into_iter()
        .map(|(agent, mut ds)| {
            ds.sort_unstable();
            AgentTurnStats {
                agent: agent.to_string(),
                completed: ds.len() as u64,
                p50_ms: nearest_rank(&ds, 50),
                p95_ms: nearest_rank(&ds, 95),
            }
        })
        .collect();
    by_agent.sort_by(|a, b| b.completed.cmp(&a.completed).then(a.agent.cmp(&b.agent)));

    let blocked_ended = turns.iter().filter(|t| t.ended_by == "blocked").count() as u64;
    let intervention_rate = if turns.is_empty() {
        None
    } else {
        Some(blocked_ended as f64 / turns.len() as f64)
    };

    // Rework: a short turn that starts shortly after a same-pane turn ended
    // blocked — the agent was interrupted mid-task and had to redo work.
    let rework_turns = turns
        .iter()
        .filter(|t| t.duration_ms < SHORT_TURN_MS)
        .filter(|t| {
            let start = t.finished_at_ms.saturating_sub(t.duration_ms);
            turns.iter().any(|prev| {
                prev.pane_id == t.pane_id
                    && prev.ended_by == "blocked"
                    && prev.finished_at_ms <= start
                    && start - prev.finished_at_ms < REWORK_WINDOW_MS
            })
        })
        .count() as u64;

    let mut gap_durations: Vec<u64> = gaps.iter().map(|g| g.duration_ms).collect();
    gap_durations.sort_unstable();

    Some(TurnStats {
        completed: turns.len() as u64,
        p50_ms: nearest_rank(&durations, 50),
        p95_ms: nearest_rank(&durations, 95),
        by_agent,
        intervention_rate,
        zero_intervention_rate: intervention_rate.map(|r| 1.0 - r),
        rework_turns,
        human_latency_p50_ms: nearest_rank(&gap_durations, 50),
        human_latency_total_ms: gap_durations.iter().sum(),
    })
}

/// Rewrite turns.jsonl + gaps.jsonl keeping only records newer than
/// `now - keep_ms`. Missing files are skipped silently.
pub fn rotate_logs(paths: &PluginPaths, keep_ms: u64) {
    let cutoff = crate::report::now_ms().saturating_sub(keep_ms);
    if let Err(err) = rotate_file(&turns_path(paths), |r: &TurnRecord| {
        r.finished_at_ms >= cutoff
    }) {
        eprintln!("analytics: turns rotation failed: {err:#}");
    }
    if let Err(err) = rotate_file(&gaps_path(paths), |g: &GapRecord| g.started_at_ms >= cutoff) {
        eprintln!("analytics: gaps rotation failed: {err:#}");
    }
}

fn rotate_file<T: serde::de::DeserializeOwned + Serialize>(
    path: &std::path::Path,
    keep: impl Fn(&T) -> bool,
) -> Result<()> {
    let records: Vec<T> = read_jsonl(path);
    if records.is_empty() || records.iter().all(&keep) {
        return Ok(());
    }
    let kept: Vec<&T> = records.iter().filter(|r| keep(r)).collect();
    let tmp = path.with_extension("jsonl.tmp");
    let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    for r in &kept {
        serde_json::to_writer(&mut f, r)?;
        f.write_all(b"\n")?;
    }
    drop(f);
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Pure increment over `crate::watch::LoopAlerts`: count the match; a pane
/// silent for over 10 minutes starts its streak over.
pub fn record_output_match(alerts: &mut crate::watch::LoopAlerts, pane_id: &str, now_ms: u64) {
    match alerts.get_mut(pane_id) {
        Some(a) if now_ms.saturating_sub(a.last_at_ms) <= crate::watch::LOOP_WINDOW_MS => {
            a.count += 1;
            a.last_at_ms = now_ms;
        }
        _ => {
            alerts.insert(
                pane_id.to_string(),
                crate::watch::LoopAlert {
                    count: 1,
                    first_at_ms: now_ms,
                    last_at_ms: now_ms,
                },
            );
        }
    }
}

/// A tip the daemon surfaced; rendered in the report pane, urgent ones also
/// become herdr notifications.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tip {
    pub pane_id: String,
    pub message: String,
    /// true = also send `herdr notification show`
    pub urgent: bool,
}

/// Pure evaluation: what should the daemon tell the user right now?
pub fn evaluate_tips(states: &AgentStates, now_ms: u64) -> Vec<Tip> {
    let mut tips = Vec::new();
    for s in states.values() {
        let held_ms = now_ms.saturating_sub(s.since_ms);
        let agent_label = s.agent.clone().unwrap_or_else(|| "agent".to_string());
        if s.status == "blocked" && held_ms >= BLOCKED_TIP_SECS * 1000 && due_for_nag(s, now_ms) {
            tips.push(Tip {
                pane_id: s.pane_id.clone(),
                message: format!(
                    "{agent_label} has been blocked {} — it needs input",
                    human_dur(held_ms)
                ),
                urgent: true,
            });
        } else if s.status == "working" && held_ms >= LONG_TURN_SECS * 1000 {
            tips.push(Tip {
                pane_id: s.pane_id.clone(),
                message: format!(
                    "{agent_label} has been working {} on one turn — consider checking in or splitting the task",
                    human_dur(held_ms)
                ),
                urgent: false,
            });
        }
    }
    tips
}

fn due_for_nag(s: &AgentState, now_ms: u64) -> bool {
    match s.last_notified_ms {
        None => true,
        Some(at) => now_ms.saturating_sub(at) >= BLOCKED_NAG_SECS * 1000,
    }
}

pub fn human_dur(ms: u64) -> String {
    let mins = ms / 60_000;
    if mins < 60 {
        format!("{mins}m")
    } else {
        format!("{}h{}m", mins / 60, mins % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(pane: &str, status: &str, at_ms: u64) -> Transition {
        Transition {
            pane_id: pane.to_string(),
            agent: Some("claude".to_string()),
            status: status.to_string(),
            at_ms,
        }
    }

    #[test]
    fn blocked_transition_notifies_immediately() {
        let mut states = AgentStates::default();
        let r = apply_transition(&mut states, &tr("w1:p1", "blocked", 1_000));
        assert!(r.entered_blocked);
        assert!(r.completed_turn_ms.is_none());
    }

    #[test]
    fn daemon_tip_waits_for_blocked_threshold() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "blocked", 0));
        // Under 5 minutes: silence — the hook already notified on the transition.
        assert!(evaluate_tips(&states, 4 * 60 * 1000).is_empty());
        let tips = evaluate_tips(&states, 6 * 60 * 1000);
        assert_eq!(tips.len(), 1);
        assert!(tips[0].urgent, "a long-blocked agent is urgent");
        assert!(tips[0].message.contains("blocked"));
    }

    #[test]
    fn blocked_nag_respects_rate_limit() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "blocked", 0));
        states.get_mut("w1:p1").unwrap().last_notified_ms = Some(6 * 60 * 1000);
        // Notified 4 minutes ago: no repeat.
        assert!(evaluate_tips(&states, 10 * 60 * 1000).is_empty());
        // Notified 16 minutes ago: nag again.
        assert_eq!(evaluate_tips(&states, 22 * 60 * 1000).len(), 1);
    }

    #[test]
    fn long_working_turn_flags_non_urgent_tip() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "working", 0));
        assert!(evaluate_tips(&states, 44 * 60 * 1000).is_empty());
        let tips = evaluate_tips(&states, 50 * 60 * 1000);
        assert_eq!(tips.len(), 1);
        assert!(!tips[0].urgent, "long turns advise, they do not alarm");
    }

    #[test]
    fn turn_completion_measures_working_span() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "working", 0));
        let r = apply_transition(&mut states, &tr("w1:p1", "idle", 600_000));
        assert_eq!(r.completed_turn_ms, Some(600_000));
    }

    #[test]
    fn idle_to_idle_transition_is_not_a_turn() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "idle", 0));
        let r = apply_transition(&mut states, &tr("w1:p1", "idle", 999_999));
        assert!(!r.entered_blocked);
        assert_eq!(r.completed_turn_ms, None);
    }

    #[test]
    fn done_after_working_also_closes_the_turn() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "working", 0));
        let r = apply_transition(&mut states, &tr("w1:p1", "done", 300_000));
        assert_eq!(r.completed_turn_ms, Some(300_000));
    }

    #[test]
    fn blocked_interrupts_working_and_closes_the_segment() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "working", 0));
        let r = apply_transition(&mut states, &tr("w1:p1", "blocked", 120_000));
        // Both matter: the pause cost a 2-minute segment AND the agent needs input.
        assert!(r.entered_blocked);
        assert_eq!(r.completed_turn_ms, Some(120_000));
    }

    #[test]
    fn states_survive_a_store_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("analytics-test-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w2:p9", "working", 42));
        store_states(&paths, "default", &states).expect("store");
        let loaded = load_states(&paths, "default");
        let s = loaded.get("w2:p9").expect("state persisted");
        assert_eq!(s.status, "working");
        assert_eq!(s.since_ms, 42);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn same_pane_id_in_two_sessions_stays_isolated() {
        let dir = std::env::temp_dir().join(format!("analytics-test-multi-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        // Both sessions run a pane literally named w1:p1.
        let mut session_a = AgentStates::default();
        apply_transition(&mut session_a, &tr("w1:p1", "working", 100));
        store_states(&paths, "session-a", &session_a).expect("store a");

        let mut session_b = AgentStates::default();
        apply_transition(&mut session_b, &tr("w1:p1", "blocked", 200));
        store_states(&paths, "session-b", &session_b).expect("store b");

        let a = load_states(&paths, "session-a");
        let b = load_states(&paths, "session-b");
        assert_eq!(a.get("w1:p1").expect("a kept its pane").status, "working");
        assert_eq!(b.get("w1:p1").expect("b kept its pane").status, "blocked");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn blocked_tip_fires_exactly_at_threshold_but_not_one_ms_before() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "blocked", 0));
        let threshold = BLOCKED_TIP_SECS * 1000;
        assert!(evaluate_tips(&states, threshold - 1).is_empty());
        assert_eq!(evaluate_tips(&states, threshold).len(), 1);
    }

    #[test]
    fn blocked_nag_fires_exactly_at_interval_but_not_one_ms_before() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "blocked", 0));
        let notified = 5 * 60 * 1000;
        states.get_mut("w1:p1").unwrap().last_notified_ms = Some(notified);
        let interval = BLOCKED_NAG_SECS * 1000;
        assert!(evaluate_tips(&states, notified + interval - 1).is_empty());
        assert_eq!(evaluate_tips(&states, notified + interval).len(), 1);
    }

    #[test]
    fn long_turn_tip_fires_exactly_at_threshold_and_stays_non_urgent() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "working", 0));
        let threshold = LONG_TURN_SECS * 1000;
        assert!(evaluate_tips(&states, threshold - 1).is_empty());
        let tips = evaluate_tips(&states, threshold);
        assert_eq!(tips.len(), 1);
        assert!(!tips[0].urgent);
    }

    #[test]
    fn unknown_start_status_is_recorded_without_side_effects() {
        let mut states = AgentStates::default();
        let r = apply_transition(&mut states, &tr("w1:p1", "mystery", 500));
        assert!(!r.entered_blocked);
        assert_eq!(r.completed_turn_ms, None);
        assert_eq!(states["w1:p1"].status, "mystery");
    }

    #[test]
    fn human_dur_formats_minutes_hours_and_zero() {
        assert_eq!(human_dur(0), "0m");
        assert_eq!(human_dur(59_999), "0m");
        assert_eq!(human_dur(60_000), "1m");
        assert_eq!(human_dur(45 * 60_000), "45m");
        assert_eq!(human_dur(60 * 60_000), "1h0m");
        assert_eq!(human_dur(90 * 60_000), "1h30m");
    }

    #[test]
    fn states_path_names_files_by_session_key() {
        let paths = PluginPaths {
            state_dir: std::path::PathBuf::from("/tmp/nowhere"),
        };
        assert_eq!(
            states_path(&paths, "sock-abc"),
            std::path::PathBuf::from("/tmp/nowhere/agent-states-sock-abc.json")
        );
    }
    #[test]
    fn resuming_work_after_blocked_measures_the_gap() {
        let mut states = AgentStates::default();
        apply_transition(&mut states, &tr("w1:p1", "working", 0));
        apply_transition(&mut states, &tr("w1:p1", "blocked", 120_000));
        let r = apply_transition(&mut states, &tr("w1:p1", "working", 420_000));
        assert_eq!(r.resumed_from_blocked_ms, Some(300_000));
        assert!(!r.entered_blocked);
        // Blocked -> idle is not a resume.
        apply_transition(&mut states, &tr("w1:p1", "blocked", 500_000));
        let r = apply_transition(&mut states, &tr("w1:p1", "idle", 600_000));
        assert_eq!(r.resumed_from_blocked_ms, None);
    }

    fn seed_turns(paths: &PluginPaths, lines: &[serde_json::Value]) {
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        use std::io::Write;
        let mut f = std::fs::File::create(turns_path(paths)).unwrap();
        for line in lines {
            serde_json::to_writer(&mut f, line).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    fn turn_line(
        pane: &str,
        agent: &str,
        finished: u64,
        duration: u64,
        ended_by: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "pane_id": pane, "agent": agent,
            "finished_at_ms": finished, "duration_ms": duration, "ended_by": ended_by
        })
    }

    #[test]
    fn turn_percentiles_use_nearest_rank() {
        let dir = std::env::temp_dir().join(format!("analytics-ts-pct-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        // Durations 10..=100 step 10: nearest-rank p50 of 10 samples is the 5th, p95 the 10th.
        let lines: Vec<_> = (1..=10)
            .map(|i| turn_line("p", "claude", i * 1000, i * 10_000, "idle"))
            .collect();
        seed_turns(&paths, &lines);
        let stats = turn_stats(&paths).unwrap();
        assert_eq!(stats.completed, 10);
        assert_eq!(stats.p50_ms, Some(50_000));
        assert_eq!(stats.p95_ms, Some(100_000));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn by_agent_sorted_busiest_first_with_per_agent_percentiles() {
        let dir = std::env::temp_dir().join(format!("analytics-ts-agents-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        let mut lines = vec![
            turn_line("p1", "claude", 1_000, 40_000, "idle"),
            turn_line("p1", "claude", 2_000, 60_000, "idle"),
            turn_line("p2", "codex", 3_000, 10_000, "done"),
        ];
        // claude gets a third turn so it outranks codex.
        lines.push(turn_line("p3", "claude", 4_000, 20_000, "idle"));
        seed_turns(&paths, &lines);
        let stats = turn_stats(&paths).unwrap();
        assert_eq!(stats.by_agent.len(), 2);
        assert_eq!(stats.by_agent[0].agent, "claude");
        assert_eq!(stats.by_agent[0].completed, 3);
        assert_eq!(stats.by_agent[0].p50_ms, Some(40_000));
        assert_eq!(stats.by_agent[1].agent, "codex");
        // Overall intervention rate: 0 blocked / 4 turns.
        assert_eq!(stats.intervention_rate, Some(0.0));
        assert_eq!(stats.zero_intervention_rate, Some(1.0));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn intervention_rate_counts_only_blocked_ended_turns() {
        let dir = std::env::temp_dir().join(format!("analytics-ts-ir-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        seed_turns(
            &paths,
            &[
                turn_line("p1", "claude", 1_000, 40_000, "blocked"),
                turn_line("p1", "claude", 2_000, 60_000, "idle"),
                turn_line("p1", "claude", 3_000, 20_000, "blocked"),
                turn_line("p1", "claude", 4_000, 20_000, "done"),
            ],
        );
        let stats = turn_stats(&paths).unwrap();
        assert_eq!(stats.intervention_rate, Some(0.5));
        assert_eq!(stats.zero_intervention_rate, Some(0.5));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rework_counts_short_turns_following_a_blocked_turn_in_same_pane() {
        let dir = std::env::temp_dir().join(format!("analytics-ts-rework-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        seed_turns(
            &paths,
            &[
                // Blocked turn ends at 100_000 on pane p1.
                turn_line("p1", "claude", 100_000, 80_000, "blocked"),
                // Starts 30s later, lasts 2min: rework.
                turn_line("p1", "claude", 220_000, 120_000, "idle"),
                // Starts 20min after the block: outside the window.
                turn_line("p1", "claude", 1_400_000, 120_000, "idle"),
                // Short but follows nothing blocked on its own pane.
                turn_line("p2", "claude", 150_000, 60_000, "idle"),
                // Same window as the rework turn but long: not rework.
                turn_line("p1", "claude", 400_000, 600_000, "idle"),
            ],
        );
        let stats = turn_stats(&paths).unwrap();
        assert_eq!(stats.rework_turns, 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn human_latency_comes_from_gap_records_only() {
        let dir = std::env::temp_dir().join(format!("analytics-ts-gaps-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        use std::io::Write;
        let mut f = std::fs::File::create(gaps_path(&paths)).unwrap();
        for duration in [200_000u64, 50_000, 800_000] {
            serde_json::to_writer(
                &mut f,
                &serde_json::json!({"pane_id":"p1","started_at_ms":0,"duration_ms":duration}),
            )
            .unwrap();
            f.write_all(b"\n").unwrap();
        }
        let stats = turn_stats(&paths).unwrap();
        assert_eq!(stats.human_latency_p50_ms, Some(200_000));
        assert_eq!(stats.human_latency_total_ms, 1_050_000);
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.intervention_rate, None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn old_turn_lines_without_ended_by_parse_as_plain_turns() {
        let dir = std::env::temp_dir().join(format!("analytics-ts-legacy-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        seed_turns(
            &paths,
            &[serde_json::json!({
                "pane_id":"p1","agent":"claude","finished_at_ms":5_000,"duration_ms":5_000
            })],
        );
        let stats = turn_stats(&paths).unwrap();
        assert_eq!(stats.completed, 1);
        assert_eq!(
            stats.intervention_rate,
            Some(0.0),
            "legacy lines are not interventions"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn no_log_records_yields_no_turn_stats() {
        let dir = std::env::temp_dir().join(format!("analytics-ts-empty-{}", std::process::id()));
        let paths = PluginPaths { state_dir: dir };
        assert!(turn_stats(&paths).is_none());
    }

    #[test]
    fn rotation_drops_old_records_and_keeps_recent() {
        let dir = std::env::temp_dir().join(format!("analytics-rotate-{}", std::process::id()));
        let paths = PluginPaths {
            state_dir: dir.clone(),
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let now = crate::report::now_ms();
        seed_turns(
            &paths,
            &[
                serde_json::json!({"pane_id":"p","finished_at_ms":now - TURN_RETENTION_MS - 1,"duration_ms":1}),
                serde_json::json!({"pane_id":"p","finished_at_ms":now,"duration_ms":2}),
            ],
        );
        use std::io::Write;
        let mut f = std::fs::File::create(gaps_path(&paths)).unwrap();
        serde_json::to_writer(
            &mut f,
            &serde_json::json!({"pane_id":"p","started_at_ms":now - TURN_RETENTION_MS - 1,"duration_ms":9}),
        )
        .unwrap();
        f.write_all(b"\n").unwrap();
        drop(f);

        rotate_logs(&paths, TURN_RETENTION_MS);

        let turns = read_jsonl::<TurnRecord>(&turns_path(&paths));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].duration_ms, 2);
        let gaps = read_jsonl::<GapRecord>(&gaps_path(&paths));
        assert!(gaps.is_empty(), "stale gap dropped");
        std::fs::remove_dir_all(dir).ok();
    }
}
