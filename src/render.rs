use anyhow::Result;

use crate::config::{self, PluginPaths};
use crate::report::{self, Filters, Report};

/// staleness threshold for the daemon snapshot: twice the default scan cadence.
const SNAPSHOT_MAX_AGE_MS: u64 = 2 * 900 * 1000;

pub fn print_cleared(filters: &Filters, paths: &PluginPaths, json: bool) -> Result<()> {
    let text = render(filters, paths, json)?;
    print!("\x1b[2J\x1b[H{text}");
    Ok(())
}

pub fn render(filters: &Filters, paths: &PluginPaths, json: bool) -> Result<String> {
    let rep = load_report(filters, paths)?;
    if json {
        return Ok(serde_json::to_string_pretty(&rep)? + "\n");
    }
    let live = crate::tips::load(paths);
    Ok(render_text(&rep, &live))
}
/// Prefer the daemon's snapshot when fresh; otherwise scan directly. A narrowed
/// request (`--since`, `--project`) always scans: the daemon keeps the full window.
pub(crate) fn load_report_shared(filters: &Filters, paths: &PluginPaths) -> Report {
    load_report(filters, paths).unwrap_or_else(|err| Report {
        generated_at_ms: report::now_ms(),
        since_ms: filters.since_ms,
        projects: vec![],
        usage: None,
        usage_note: Some(format!("scan failed: {err:#}")),
    })
}

fn load_report(filters: &Filters, paths: &PluginPaths) -> Result<report::Report> {
    if filters.since_ms.is_none()
        && filters.project.is_none()
        && let Some(snap) = config::read_snapshot(paths)
        && report::now_ms().saturating_sub(snap.generated_at_ms) < SNAPSHOT_MAX_AGE_MS
    {
        return Ok(snap);
    }
    report::gather(filters)
}

fn render_text(rep: &Report, live: &crate::tips::Tips) -> String {
    let mut out = String::new();
    if !live.items.is_empty() {
        out.push_str("Live tips\n");
        for t in &live.items {
            let marker = if t.urgent { "!" } else { "-" };
            out.push_str(&format!("  {marker} {}\n", t.message));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "herdr analytics — {}{}\n\n",
        window_label(rep.since_ms),
        generated_label(rep.generated_at_ms)
    ));

    out.push_str("Sessions\n");
    out.push_str(&format!(
        "  {:<28.28} {:>8} {:>9} {:>7} {:>10}  {}\n",
        "project", "sessions", "messages", "hrs", "last", "sources"
    ));
    if rep.projects.is_empty() {
        out.push_str("  (no indexed sessions in window; run `memex index`)\n");
    }
    for p in &rep.projects {
        let sources = p
            .sources
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "  {:<28.28} {:>8} {:>9} {:>7.1} {:>10}  {}\n",
            p.project,
            p.sessions,
            p.messages,
            p.active_ms as f64 / 3_600_000.0,
            rel_time(p.last_at_ms, rep.generated_at_ms),
            sources,
        ));
    }

    match (&rep.usage, &rep.usage_note) {
        (Some(u), _) => {
            out.push_str("\nToken usage\n");
            out.push_str(&format!(
                "  {} events, {} tokens, ${:.2} known cost\n",
                u.events,
                human_tokens(u.total_tokens),
                u.known_cost_usd
            ));
            out.push_str(&format!(
                "  cache waste: {} tokens (${:.2}) across {} misses\n",
                human_tokens(u.missed_tokens),
                u.missed_cost_usd,
                u.miss_count
            ));
            out.push_str(&format!(
                "    idle-gap misses {}, model-switch misses {}\n",
                u.idle_misses, u.model_switch_misses
            ));
            for s in &u.by_source {
                out.push_str(&format!(
                    "  {:<10.10} {:>6} events {:>10} tokens ${:>8.2}  waste {}\n",
                    s.source,
                    s.events,
                    human_tokens(s.total_tokens),
                    s.known_cost_usd,
                    human_tokens(s.missed_tokens),
                ));
            }
            out.push_str("\n  tip: idle-gap misses mean returning after the cache went cold —\n");
            out.push_str("  batching related prompts into one session keeps it warm.\n");
        }
        (None, Some(note)) => {
            out.push_str("\nToken usage: unavailable — ");
            out.push_str(note);
            out.push('\n');
        }
        (None, None) => {}
    }
    out
}

fn window_label(since_ms: Option<u64>) -> String {
    match since_ms {
        Some(ms) => format!("since {}", rel_time(ms, report::now_ms())),
        None => "all history".to_string(),
    }
}

fn generated_label(at_ms: u64) -> String {
    format!("  (generated {})", rel_time(at_ms, report::now_ms()))
}

fn rel_time(then_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn human_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1_000.0),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1_000_000.0),
        _ => format!("{:.1}B", n as f64 / 1_000_000_000.0),
    }
}
