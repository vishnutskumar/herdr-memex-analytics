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

/// Evaluate tips from the event hook's state, notify the urgent ones (rate
/// limited by last_notified_ms), and publish the full list for the report pane.
fn refresh_tips(paths: &PluginPaths) {
    let mut states = agents::load_states(paths);
    let now = report::now_ms();
    let due = agents::evaluate_tips(&states, now);

    for tip in &due {
        if tip.urgent {
            notify::show(paths, &tip.message);
            if let Some(s) = states.get_mut(&tip.pane_id) {
                s.last_notified_ms = Some(now);
            }
        }
    }
    if !due.is_empty()
        && let Err(err) = agents::store_states(paths, &states)
    {
        eprintln!("analytics watch: state write failed: {err:#}");
    }

    let published = tips::Tips {
        generated_at_ms: now,
        items: due,
    };
    if let Err(err) = tips::store(paths, &published) {
        eprintln!("analytics watch: tips write failed: {err:#}");
    }
}
