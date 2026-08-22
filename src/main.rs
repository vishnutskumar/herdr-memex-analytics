mod agents;
mod config;
mod live;
mod notify;
mod render;
mod report;
mod tips;
mod tui;
mod watch;

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::PluginPaths;

/// herdr session analytics and realtime guidance, powered by memex.
#[derive(Parser)]
#[command(name = "analytics", version, about)]
pub struct Cli {
    /// memex data directory override (default ~/.memex)
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

    /// Plugin state directory override
    /// (default $HERDR_PLUGIN_STATE_DIR, else ~/.herdr-memex-analytics)
    #[arg(long, global = true)]
    pub state_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Print the efficiency report; prefers a fresh daemon snapshot over rescanning
    Report {
        /// Only sessions active on/after this point: YYYY-MM-DD or Nd / Nh
        #[arg(long)]
        since: Option<String>,
        /// Restrict to one display project
        #[arg(long)]
        project: Option<String>,
        /// Scan the full history instead of the 30-day default window
        #[arg(long)]
        all: bool,
        /// Emit the report as JSON
        #[arg(long)]
        json: bool,
        /// Re-render forever, for a herdr pane
        #[arg(long)]
        watch: bool,
        /// Seconds between re-renders in --watch mode
        #[arg(long, default_value_t = 30)]
        interval_secs: u64,
    },
    /// Interactive dashboard for a herdr pane (like `memex tui`)
    Ui,
    /// Compute once and write the snapshot JSON (one daemon scan cycle)
    Snapshot {
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Handle one pane.agent_status_changed event (invoked by the herdr
    /// [[events]] hook with HERDR_PLUGIN_EVENT_JSON set)
    EventHook {
        /// Event JSON override, for testing (default: $HERDR_PLUGIN_EVENT_JSON)
        #[arg(long)]
        event_json: Option<String>,
    },
    /// Background daemon: refresh the snapshot on a fixed cadence so panes and
    /// tips stay warm without manual refreshes
    Watch {
        /// Seconds between scans (default from config.toml, else 900)
        #[arg(long)]
        scan_interval_secs: Option<u64>,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("analytics: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = PluginPaths::new(cli.state_dir.clone())?;
    let cfg = config::Config::load(&paths);
    let filters = |since_ms, project: Option<String>| report::Filters {
        root: cli.root.clone(),
        since_ms,
        project,
        memo_ttl_ms: 0,
    };
    match cli.cmd {
        Cmd::Report {
            since,
            project,
            all,
            json,
            watch,
            interval_secs,
        } => {
            let since_ms = parse_since(since.as_deref(), all)?;
            let filters = filters(since_ms, project);
            if watch {
                loop {
                    render::print_cleared(&filters, &paths, json)
                        .context("report render failed")?;
                    std::thread::sleep(Duration::from_secs(interval_secs));
                }
            }
            let text = render::render(&filters, &paths, json)?;
            print!("{text}");
        }
        Cmd::Ui => {
            tui::run(&filters(None, None), &paths)?;
        }
        Cmd::Snapshot {
            since,
            project,
            all,
        } => {
            let since_ms = parse_since(since.as_deref(), all)?;
            let filters = filters(since_ms, project);
            let rep = report::gather(&filters, Some(&paths))?;
            config::write_snapshot(&paths, &rep)?;
            eprintln!(
                "snapshot written to {}",
                config::snapshot_path(&paths).display()
            );
        }
        Cmd::EventHook { event_json } => {
            let raw = match event_json {
                Some(json) => json,
                None => std::env::var("HERDR_PLUGIN_EVENT_JSON").context(
                    "HERDR_PLUGIN_EVENT_JSON not set (invoke via the herdr [[events]] hook)",
                )?,
            };
            event_hook(&paths, &raw)?;
        }
        Cmd::Watch { scan_interval_secs } => {
            let interval = scan_interval_secs.unwrap_or(cfg.scan_interval_secs);
            watch::run(
                // The daemon always maintains the full-window snapshot; `report`
                // narrows it client-side when it can, and rescans when it cannot.
                filters(None, None),
                &paths,
                Duration::from_secs(interval),
            )?;
        }
    }
    Ok(())
}

/// Record one agent status transition and act on it: notify immediately on
/// blocked, log completed turn durations and human-response gaps for later
/// efficiency analysis. Output matches feed the retry-loop detector instead.
fn event_hook(paths: &PluginPaths, raw: &str) -> Result<()> {
    let ev: serde_json::Value =
        serde_json::from_str(raw).with_context(|| format!("bad event JSON {raw:?}"))?;
    // herdr delivers events as an envelope ({type, data}); accept both shapes.
    let body = ev.get("data").unwrap_or(&ev);

    let is_output_match = ev["type"]
        .as_str()
        .is_some_and(|t| t.ends_with("output_matched"))
        || ev["event"]
            .as_str()
            .is_some_and(|t| t.ends_with("output_matched"))
        || body.get("matched_line").is_some();
    if is_output_match {
        let pane_id = body["pane_id"].as_str().context("event missing pane_id")?;
        record_loop_alert(paths, pane_id);
        return Ok(());
    }

    let status = body["agent_status"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let transition = agents::Transition {
        pane_id: body["pane_id"]
            .as_str()
            .context("event missing pane_id")?
            .to_string(),
        agent: body["agent"].as_str().map(str::to_string),
        status,
        at_ms: report::now_ms(),
    };

    let session = agents::session_key();
    let mut states = agents::load_states(paths, &session);
    let result = agents::apply_transition(&mut states, &transition);
    agents::store_states(paths, &session, &states)?;

    if result.entered_blocked {
        let who = transition.agent.as_deref().unwrap_or("agent");
        notify::show(
            paths,
            &format!("{who} blocked — needs input ({})", transition.pane_id),
        );
    }
    if let Some(duration_ms) = result.completed_turn_ms {
        append_turn(paths, &transition, duration_ms)?;
    }
    if let Some(duration_ms) = result.resumed_from_blocked_ms {
        append_gap(
            paths,
            &transition,
            transition.at_ms - duration_ms,
            duration_ms,
        )?;
    }
    Ok(())
}

/// Count a repeated-output match toward the pane's retry-loop streak. The hook
/// only maintains the ledger; the daemon decides when it becomes a tip.
fn record_loop_alert(paths: &PluginPaths, pane_id: &str) {
    let mut alerts = watch::load_loop_alerts(paths);
    agents::record_output_match(&mut alerts, pane_id, report::now_ms());
    watch::store_loop_alerts(paths, &alerts).ok();
}

/// Completed-turn log (JSONL): one line per closed working segment. `ended_by`
/// distinguishes real completions from blocked interruptions.
fn append_turn(paths: &PluginPaths, t: &agents::Transition, duration_ms: u64) -> Result<()> {
    append_jsonl(
        paths.state_dir.join("turns.jsonl"),
        &serde_json::json!({
            "pane_id": t.pane_id,
            "agent": t.agent,
            "finished_at_ms": t.at_ms,
            "duration_ms": duration_ms,
            "ended_by": t.status,
        }),
    )
}

/// Human-response gap (JSONL): how long a blocked pane waited before working
/// again.
fn append_gap(
    paths: &PluginPaths,
    t: &agents::Transition,
    started_at_ms: u64,
    duration_ms: u64,
) -> Result<()> {
    append_jsonl(
        paths.state_dir.join("gaps.jsonl"),
        &serde_json::json!({
            "pane_id": t.pane_id,
            "agent": t.agent,
            "started_at_ms": started_at_ms,
            "duration_ms": duration_ms,
        }),
    )
}

fn append_jsonl(path: PathBuf, value: &serde_json::Value) -> Result<()> {
    fs::create_dir_all(path.parent().context("log path has no parent")?)?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut f, value)?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Accepts `Nd`, `Nh`, or `YYYY-MM-DD`; `--all` wins over any value.
fn parse_since(spec: Option<&str>, all: bool) -> Result<Option<u64>> {
    if all {
        return Ok(None);
    }
    let Some(spec) = spec else {
        // Default window: 30 days, matching the report's focus on recent efficiency.
        return Ok(Some(ago_ms(30 * 24 * 3600)));
    };
    let spec = spec.trim();
    if let Some(days) = spec.strip_suffix('d') {
        let n: u64 = days.parse().context("bad --since day count")?;
        return Ok(Some(ago_ms(n * 24 * 3600)));
    }
    if let Some(hours) = spec.strip_suffix('h') {
        let n: u64 = hours.parse().context("bad --since hour count")?;
        return Ok(Some(ago_ms(n * 3600)));
    }
    let date = chrono::NaiveDate::parse_from_str(spec, "%Y-%m-%d")
        .with_context(|| format!("bad --since value {spec:?} (use YYYY-MM-DD, Nd, or Nh)"))?;
    let midnight = date.and_hms_opt(0, 0, 0).context("bad --since date")?;
    Ok(Some(
        u64::try_from(midnight.and_utc().timestamp_millis()).context("--since before epoch")?,
    ))
}

fn ago_ms(secs: u64) -> u64 {
    report::now_ms().saturating_sub(secs * 1000)
}
