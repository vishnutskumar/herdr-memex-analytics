use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::PluginPaths;

/// Per-pane output-activity delta since the previous fleet sample. A large
/// revision_delta means the agent is actively emitting output.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PaneChurn {
    pub pane_id: String,
    pub agent: Option<String>,
    pub status: String,
    pub revision_delta: u64,
}

/// Live herdr fleet state at one sample point.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct FleetSnapshot {
    pub working: u64,
    pub blocked: u64,
    pub idle: u64,
    pub churn: Vec<PaneChurn>,
    pub sampled_at_ms: u64,
}

#[derive(Deserialize)]
struct RawAgent {
    agent: Option<String>,
    agent_status: String,
    pane_id: String,
    revision: u64,
}

fn live_state_path(paths: &PluginPaths) -> PathBuf {
    paths.state_dir.join("live-state.json")
}

fn load_prev_revisions(paths: &PluginPaths) -> BTreeMap<String, u64> {
    fs::read(live_state_path(paths))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store_prev_revisions(paths: &PluginPaths, revisions: &BTreeMap<String, u64>) -> Result<()> {
    crate::config::store_json(live_state_path(paths), revisions)
}

/// One live fleet sample. Runs `$HERDR_BIN_PATH` (or `herdr`) `api snapshot`,
/// diffs pane revisions against the previous sample persisted in
/// live-state.json, and returns None on any failure so the daemon survives a
/// missing or wedged herdr server.
pub fn sample(paths: &PluginPaths) -> Option<FleetSnapshot> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let out = std::process::Command::new(bin)
        .args(["api", "snapshot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let agents = &value["result"]["snapshot"]["agents"];
    if !agents.is_array() {
        return None;
    }
    let prev = load_prev_revisions(paths);
    let (fleet, next) = compute_fleet(&agents.to_string(), &prev);
    // Panes that vanished from the snapshot are dropped by compute_fleet.
    if let Err(err) = store_prev_revisions(paths, &next) {
        eprintln!("analytics watch: live-state write failed: {err:#}");
    }
    Some(fleet)
}

/// Pure core of sample(): parse the `agents[]` JSON array, count statuses, and
/// diff revisions against `prev`. The returned map holds exactly the panes seen
/// now (unseen panes pruned); panes new to `prev` get delta 0.
pub fn compute_fleet(
    agents_json: &str,
    prev: &BTreeMap<String, u64>,
) -> (FleetSnapshot, BTreeMap<String, u64>) {
    let agents: Vec<RawAgent> = match serde_json::from_str(agents_json) {
        Ok(agents) => agents,
        Err(_) => return (FleetSnapshot::default(), prev.clone()),
    };
    let mut fleet = FleetSnapshot {
        sampled_at_ms: crate::report::now_ms(),
        ..FleetSnapshot::default()
    };
    let mut next = BTreeMap::new();
    for a in agents {
        match a.agent_status.as_str() {
            "working" => fleet.working += 1,
            "blocked" => fleet.blocked += 1,
            "idle" => fleet.idle += 1,
            _ => {}
        }
        let delta = prev
            .get(&a.pane_id)
            .map_or(0, |&last| a.revision.abs_diff(last));
        fleet.churn.push(PaneChurn {
            pane_id: a.pane_id.clone(),
            agent: a.agent,
            status: a.agent_status,
            revision_delta: delta,
        });
        next.insert(a.pane_id, a.revision);
    }
    fleet.churn.sort_by(|x, y| x.pane_id.cmp(&y.pane_id));
    (fleet, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENTS: &str = r#"[
        {"agent":"omp","agent_status":"working","pane_id":"w1:p1","revision":100},
        {"agent":"claude","agent_status":"blocked","pane_id":"w1:p2","revision":7},
        {"agent":null,"agent_status":"idle","pane_id":"w2:p1","revision":3},
        {"agent":"codex","agent_status":"done","pane_id":"w2:p2","revision":9}
    ]"#;

    #[test]
    fn counts_working_blocked_idle_and_ignores_other_statuses() {
        let (fleet, _) = compute_fleet(AGENTS, &BTreeMap::new());
        assert_eq!((fleet.working, fleet.blocked, fleet.idle), (1, 1, 1));
        assert_eq!(fleet.churn.len(), 4);
        assert!(fleet.sampled_at_ms > 0);
    }

    #[test]
    fn churn_delta_is_absolute_difference_against_previous_revisions() {
        let mut prev = BTreeMap::new();
        prev.insert("w1:p1".to_string(), 40);
        prev.insert("w1:p2".to_string(), 50);
        let (fleet, _) = compute_fleet(AGENTS, &prev);
        let delta = |pane: &str| {
            fleet
                .churn
                .iter()
                .find(|c| c.pane_id == pane)
                .unwrap()
                .revision_delta
        };
        assert_eq!(delta("w1:p1"), 60);
        // Revision went backward (pane restart): churn is still non-negative.
        assert_eq!(delta("w1:p2"), 43);
        // Pane unseen last sample: zero delta.
        assert_eq!(delta("w2:p1"), 0);
    }

    #[test]
    fn next_revisions_prune_vanished_panes() {
        let mut prev = BTreeMap::new();
        prev.insert("gone:p9".to_string(), 5);
        let (_, next) = compute_fleet(AGENTS, &prev);
        assert!(!next.contains_key("gone:p9"));
        assert_eq!(next.get("w1:p1"), Some(&100));
    }

    #[test]
    fn malformed_agents_json_degrades_to_empty_fleet() {
        let (fleet, next) = compute_fleet("not json", &BTreeMap::new());
        assert_eq!(fleet.working + fleet.blocked + fleet.idle, 0);
        assert!(fleet.churn.is_empty());
        assert!(next.is_empty());
    }

    #[test]
    fn extra_snapshot_fields_are_ignored() {
        let raw = r#"[{"agent":"omp","agent_status":"working","pane_id":"p",
            "revision":1,"workspace_id":"w","tokens":42,"focused":true}]"#;
        let (fleet, next) = compute_fleet(raw, &BTreeMap::new());
        assert_eq!(fleet.working, 1);
        assert_eq!(next.get("p"), Some(&1));
    }
}
