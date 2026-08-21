use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::PluginPaths;

/// How long an agent must sit in `blocked` before the daemon re-nags (the event
/// hook already notifies on the transition itself).
pub const BLOCKED_NAG_SECS: u64 = 15 * 60;
/// A single agent turn running longer than this is worth flagging.
pub const LONG_TURN_SECS: u64 = 45 * 60;
/// Blocked panes get their first daemon tip after this long.
pub const BLOCKED_TIP_SECS: u64 = 5 * 60;

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
    if let Some(prev) = states.get(&t.pane_id)
        && prev.status == "working"
        && matches!(t.status.as_str(), "idle" | "done" | "blocked")
    {
        result.completed_turn_ms = Some(t.at_ms.saturating_sub(prev.since_ms));
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
}
