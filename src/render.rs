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
        project_usage: vec![],
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
            match u.cache_hit_rate {
                Some(rate) => out.push_str(&format!(
                    "  cache hit-rate: {:.1}% of {} prompt tokens\n",
                    rate * 100.0,
                    human_tokens(u.input_tokens)
                )),
                None => out.push_str("  cache hit-rate: n/a (no prompt tokens)\n"),
            }
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
            if !u.by_model.is_empty() {
                out.push_str(&format!(
                    "\n  {:<20.20} {:>6} {:>10} {:>9}\n",
                    "model", "events", "tokens", "cost"
                ));
                for m in &u.by_model {
                    out.push_str(&format!(
                        "  {:<20.20} {:>6} {:>10} ${:>8.2}\n",
                        m.model,
                        m.events,
                        human_tokens(m.total_tokens),
                        m.known_cost_usd
                    ));
                }
            }
            if !rep.project_usage.is_empty() {
                out.push_str(&format!(
                    "\n  {:<28.28} {:>6} {:>10} {:>9}\n",
                    "project", "events", "tokens", "cost"
                ));
                for p in rep.project_usage.iter().take(5) {
                    out.push_str(&format!(
                        "  {:<28.28} {:>6} {:>10} ${:>8.2}\n",
                        p.project,
                        p.events,
                        human_tokens(p.total_tokens),
                        p.known_cost_usd
                    ));
                }
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

pub(crate) fn rel_time(then_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

pub(crate) fn human_tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1_000.0),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1_000_000.0),
        _ => format!("{:.1}B", n as f64 / 1_000_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PluginPaths;
    use crate::report::{ProjectStats, SourceDigest, UsageDigest};
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_paths(label: &str) -> PluginPaths {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "analytics-render-{label}-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        PluginPaths { state_dir: dir }
    }

    /// A report with two projects and no usage digest, written as the daemon
    /// snapshot so `render` never has to scan a memex root.
    fn seed_snapshot(paths: &PluginPaths, mut rep: Report) -> Report {
        rep.generated_at_ms = report::now_ms();
        config::write_snapshot(paths, &rep).unwrap();
        rep
    }

    fn sample_report() -> Report {
        Report {
            generated_at_ms: 0,
            since_ms: None,
            projects: vec![
                ProjectStats {
                    project: "alpha".into(),
                    sessions: 3,
                    messages: 42,
                    active_ms: 7_200_000,
                    last_at_ms: report::now_ms() - 60_000,
                    sources: BTreeMap::from([("Claude".into(), 2), ("Codex".into(), 1)]),
                },
                ProjectStats {
                    project: "beta".into(),
                    sessions: 1,
                    messages: 5,
                    active_ms: 300_000,
                    last_at_ms: report::now_ms() - 3_600_000,
                    sources: BTreeMap::from([("Omp".into(), 1)]),
                },
            ],
            usage: None,
            usage_note: Some("disabled; set token_usage = true in config.toml".into()),
            project_usage: vec![],
        }
    }

    /// Filters whose root points nowhere: a fresh snapshot must make the scan
    /// unreachable, proving hermeticity of these tests.
    fn nowhere_filters() -> Filters {
        Filters {
            root: Some(std::path::PathBuf::from("/nonexistent-analytics-test-root")),
            since_ms: None,
            project: None,
        }
    }

    #[test]
    fn json_mode_emits_the_seeded_report_verbatim() {
        let paths = tmp_paths("json");
        let seeded = seed_snapshot(&paths, sample_report());
        let out = render(&nowhere_filters(), &paths, true).unwrap();
        let parsed: Report = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed.projects.len(), 2);
        assert_eq!(parsed.projects[0].project, seeded.projects[0].project);
        assert_eq!(parsed.projects[0].messages, 42);
        assert!(parsed.usage.is_none());
        fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn text_mode_renders_project_rows_with_sources() {
        let paths = tmp_paths("text");
        seed_snapshot(&paths, sample_report());
        let out = render(&nowhere_filters(), &paths, false).unwrap();
        assert!(
            out.contains("herdr analytics — all history"),
            "header: {out}"
        );
        assert!(out.contains("alpha"), "project row: {out}");
        assert!(out.contains("beta"));
        assert!(out.contains("Claude:2 Codex:1"), "sources map: {out}");
        assert!(out.contains("Omp:1"));
        // 7.2M ms active = exactly 2.0 hours.
        assert!(out.contains("2.0"), "hours column: {out}");
        fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn empty_project_list_shows_the_no_sessions_hint() {
        let paths = tmp_paths("empty");
        let mut rep = sample_report();
        rep.projects.clear();
        seed_snapshot(&paths, rep);
        let out = render(&nowhere_filters(), &paths, false).unwrap();
        assert!(out.contains("(no indexed sessions in window"), "{out}");
        fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn missing_usage_renders_the_unavailable_note() {
        let paths = tmp_paths("note");
        seed_snapshot(&paths, sample_report());
        let out = render(&nowhere_filters(), &paths, false).unwrap();
        assert!(
            out.contains("Token usage: unavailable — disabled; set token_usage"),
            "{out}"
        );
        fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn present_usage_renders_the_token_usage_block() {
        let paths = tmp_paths("usage");
        let mut rep = sample_report();
        rep.usage_note = None;
        rep.usage = Some(UsageDigest {
            events: 10,
            total_tokens: 25_000,
            known_cost_usd: 1.5,
            missed_tokens: 4_000,
            missed_cost_usd: 0.25,
            miss_count: 3,
            idle_misses: 2,
            model_switch_misses: 1,
            by_source: vec![SourceDigest {
                source: "claude".into(),
                events: 10,
                total_tokens: 25_000,
                known_cost_usd: 1.5,
                missed_tokens: 4_000,
            }],
            cache_read_tokens: 15_000,
            input_tokens: 20_000,
            cache_hit_rate: Some(0.75),
            by_model: vec![],
        });
        seed_snapshot(&paths, rep);
        let out = render(&nowhere_filters(), &paths, false).unwrap();
        assert!(out.contains("Token usage"), "{out}");
        assert!(out.contains("25.0K tokens"), "{out}");
        assert!(out.contains("$1.50 known cost"), "{out}");
        assert!(
            out.contains("cache hit-rate: 75.0% of 20.0K prompt tokens"),
            "{out}"
        );
        assert!(
            out.contains("idle-gap misses 2, model-switch misses 1"),
            "{out}"
        );
        fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn stale_or_missing_snapshot_falls_back_to_a_failed_scan_report() {
        // No snapshot at all and an unscannable root: load_report_shared (the
        // daemon's path) must degrade to an empty report, not panic.
        let paths = tmp_paths("fallback");
        let rep = load_report_shared(&nowhere_filters(), &paths);
        assert!(rep.projects.is_empty());
        assert!(rep.usage_note.unwrap().contains("scan failed"));
        fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn rel_time_buckets_seconds_minutes_hours_days() {
        let now = 10_000_000_000u64;
        assert_eq!(rel_time(now, now), "0s");
        assert_eq!(rel_time(now - 59_000, now), "59s");
        assert_eq!(rel_time(now - 60_000, now), "1m");
        assert_eq!(rel_time(now - 3_600_000, now), "1h");
        assert_eq!(rel_time(now - 86_400_000, now), "1d");
    }

    #[test]
    fn human_tokens_scales_through_k_m_b() {
        assert_eq!(human_tokens(0), "0");
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(25_000), "25.0K");
        assert_eq!(human_tokens(1_500_000), "1.5M");
        assert_eq!(human_tokens(2_000_000_000), "2.0B");
    }
}
