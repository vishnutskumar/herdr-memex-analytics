use std::time::Duration;

use anyhow::Result;

use crate::agents;
use crate::config::{self, PluginPaths};
use crate::notify;
use crate::report::{self, Filters};
use crate::tips;

/// Daemon loop: recompute the snapshot on a fixed cadence, like memex's periodic
/// reindex, then evaluate realtime tips from the agent states the event hook
/// maintains. Never exits on a failed cycle; a transient error just skips one
/// refresh.
pub fn run(filters: Filters, paths: &PluginPaths, interval: Duration) -> Result<()> {
    eprintln!(
        "analytics watch: scanning every {}s, snapshot at {}",
        interval.as_secs(),
        config::snapshot_path(paths).display()
    );
    loop {
        match report::gather(&filters) {
            Ok(rep) => {
                if let Err(err) = config::write_snapshot(paths, &rep) {
                    eprintln!("analytics watch: snapshot write failed: {err:#}");
                }
            }
            Err(err) => eprintln!("analytics watch: scan failed: {err:#}"),
        }
        refresh_tips(paths);
        std::thread::sleep(interval);
    }
}

/// Evaluate tips across every herdr session's state, notify the urgent ones
/// (rate limited by last_notified_ms), and publish the full list for the
/// report pane.
fn refresh_tips(paths: &PluginPaths) {
    let now = report::now_ms();
    let mut due: Vec<agents::Tip> = Vec::new();

    // Each herdr session has its own state file; pane ids are session-scoped.
    for (session, mut states) in agents::load_all_states(paths) {
        let session_tips = agents::evaluate_tips(&states, now);
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

    let published = tips::Tips {
        generated_at_ms: now,
        items: due,
    };
    if let Err(err) = tips::store(paths, &published) {
        eprintln!("analytics watch: tips write failed: {err:#}");
    }
}
