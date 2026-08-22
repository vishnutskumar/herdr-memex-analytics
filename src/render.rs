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
        daily: vec![],
        activity_heatmap: vec![],
        burn_rate_usd_per_hr: None,
        today_cost_usd: None,
        reasoning_tokens: 0,
        reasoning_share: None,
        bloating_sessions: vec![],
        wow: None,
        turns: None,
        fleet: None,
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
    report::gather(filters, Some(paths))
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
    let activity = activity_lines(rep);
    if !activity.is_empty() {
        out.push_str("\nActivity\n");
        for line in activity {
            out.push_str(&line);
            out.push('\n');
        }
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

/// Seven-level cost sparkline over a value series; flat baseline when every
/// value is zero.
pub(crate) fn sparkline(values: &[f64]) -> String {
    const RAMP: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    let max = values.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return "\u{2581}".repeat(values.len());
    }
    values
        .iter()
        .map(|v| {
            let idx = (((v / max) * 6.0).round() as usize).min(6);
            RAMP[idx]
        })
        .collect()
}

/// Compact duration: `45s`, `3m12s`, `2h05m`, `1d03h`.
pub(crate) fn human_ms(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{}s", secs / 60, secs % 60),
        3600..=86_399 => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
        _ => format!("{}d{:02}h", secs / 86_400, (secs % 86_400) / 3_600),
    }
}

/// Sign-aware dollar delta: `+$1.20` / `-$3.40`.
pub(crate) fn signed_usd(v: f64) -> String {
    if v >= 0.0 {
        format!("+${v:.2}")
    } else {
        format!("-${:.2}", -v)
    }
}

/// The `Activity` block: daily-cost sparkline, spend pace, week-over-week
/// delta, turn quality, fleet status, and bloat warnings. Every line stays
/// within 100 columns.
fn activity_lines(rep: &Report) -> Vec<String> {
    let mut lines = Vec::new();
    if !rep.daily.is_empty() {
        let costs: Vec<f64> = rep.daily.iter().map(|d| d.cost_usd).collect();
        let total: f64 = costs.iter().sum();
        lines.push(format!(
            "  daily cost  {}  ${:.2}/{}d",
            sparkline(&costs),
            total,
            costs.len()
        ));
    }
    let mut spend = Vec::new();
    if let Some(burn) = rep.burn_rate_usd_per_hr {
        spend.push(format!("burn ${burn:.2}/hr"));
    }
    if let Some(today) = rep.today_cost_usd {
        spend.push(format!("today ${today:.2}"));
    }
    if !spend.is_empty() {
        lines.push(format!("  spend  {}", spend.join(" · ")));
    }
    if let (Some(usage), Some(wow)) = (&rep.usage, &rep.wow) {
        let delta = usage.known_cost_usd - wow.cost_usd;
        let pct = if wow.cost_usd > 0.0 {
            format!(" ({:+.1}%)", delta / wow.cost_usd * 100.0)
        } else {
            String::new()
        };
        lines.push(format!("  vs prior window  {}{pct}", signed_usd(delta)));
    }
    if let Some(t) = &rep.turns {
        let ir = t
            .intervention_rate
            .map(|r| format!(" · interventions {:.0}%", r * 100.0))
            .unwrap_or_default();
        lines.push(format!(
            "  turns {} completed · p50 {} · p95 {}{ir}",
            t.completed,
            t.p50_ms.map_or_else(|| "n/a".into(), human_ms),
            t.p95_ms.map_or_else(|| "n/a".into(), human_ms),
        ));
        if let Some(hl) = t.human_latency_p50_ms {
            lines.push(format!("  human unblock p50 {}", human_ms(hl)));
        }
    }
    if let Some(fleet) = &rep.fleet {
        lines.push(format!(
            "  fleet  {} working · {} blocked · {} idle",
            fleet.working, fleet.blocked, fleet.idle
        ));
    }
    if let Some(b) = rep.bloating_sessions.first() {
        lines.push(format!(
            "  context bloat: {:.12} ({}) at {} uncached input",
            b.session_id,
            b.project,
            human_tokens(b.last_uncached_input)
        ));
    }
    if rep.reasoning_tokens > 0 {
        let share = rep
            .reasoning_share
            .map(|s| format!(" ({:.1}% of output)", s * 100.0))
            .unwrap_or_default();
        lines.push(format!(
            "  reasoning {} tokens{share}",
            human_tokens(rep.reasoning_tokens)
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PluginPaths;
    use crate::report::{BloatSession, DayPoint, ProjectStats, SourceDigest, UsageDigest, Wow};
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
            daily: vec![],
            activity_heatmap: vec![],
            burn_rate_usd_per_hr: None,
            today_cost_usd: None,
            reasoning_tokens: 0,
            reasoning_share: None,
            bloating_sessions: vec![],
            wow: None,
            turns: None,
            fleet: None,
        }
    }

    /// Filters whose root points nowhere: a fresh snapshot must make the scan
    /// unreachable, proving hermeticity of these tests.
    fn nowhere_filters() -> Filters {
        Filters {
            root: Some(std::path::PathBuf::from("/nonexistent-analytics-test-root")),
            since_ms: None,
            project: None,
            memo_ttl_ms: 0,
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

    #[test]
    fn sparkline_scales_to_seven_levels_with_flat_zero_baseline() {
        assert_eq!(sparkline(&[]), "");
        assert_eq!(sparkline(&[0.0, 0.0]), "▁▁");
        assert_eq!(sparkline(&[0.0, 5.0]), "▁▇");
        assert_eq!(sparkline(&[2.5, 5.0, 0.0]), "▄▇▁");
        // Equal values all hit the top level.
        assert_eq!(sparkline(&[5.0, 5.0]), "▇▇");
    }

    #[test]
    fn human_ms_buckets_through_seconds_days() {
        assert_eq!(human_ms(45_000), "45s");
        assert_eq!(human_ms(192_000), "3m12s");
        assert_eq!(human_ms(7_500_000), "2h05m");
        assert_eq!(human_ms(97_200_000), "1d03h");
        assert_eq!(human_ms(900_000), "15m0s");
        assert_eq!(human_ms(65_000), "1m5s");
    }

    #[test]
    fn signed_usd_marks_direction() {
        assert_eq!(signed_usd(1.2), "+$1.20");
        assert_eq!(signed_usd(-3.4), "-$3.40");
        assert_eq!(signed_usd(0.0), "+$0.00");
    }

    #[test]
    fn activity_lines_render_wow_turns_fleet_bloat_and_reasoning() {
        let mut rep = sample_report();
        rep.usage_note = None;
        rep.usage = Some(UsageDigest {
            events: 10,
            total_tokens: 25_000,
            known_cost_usd: 11.0,
            missed_tokens: 0,
            missed_cost_usd: 0.0,
            miss_count: 0,
            idle_misses: 0,
            model_switch_misses: 0,
            by_source: vec![],
            cache_read_tokens: 0,
            input_tokens: 0,
            cache_hit_rate: None,
            by_model: vec![],
        });
        rep.daily = vec![DayPoint {
            date: "2026-08-20".into(),
            tokens: 10_000,
            cost_usd: 1.25,
            events: 4,
            sessions: 1,
        }];
        rep.burn_rate_usd_per_hr = Some(4.5);
        rep.today_cost_usd = Some(2.0);
        rep.wow = Some(Wow {
            cost_usd: 10.0,
            missed_cost_usd: 0.0,
        });
        rep.reasoning_tokens = 4_000;
        rep.reasoning_share = Some(0.4);
        rep.bloating_sessions = vec![BloatSession {
            session_id: "abc123-def456".into(),
            project: "/w/big".into(),
            last_uncached_input: 150_000,
        }];
        rep.turns = Some(crate::agents::TurnStats {
            completed: 8,
            p50_ms: Some(192_000),
            p95_ms: Some(900_000),
            by_agent: vec![],
            intervention_rate: Some(0.125),
            zero_intervention_rate: Some(0.875),
            rework_turns: 2,
            human_latency_p50_ms: Some(65_000),
            human_latency_total_ms: 600_000,
        });
        rep.fleet = Some(crate::live::FleetSnapshot {
            working: 2,
            blocked: 1,
            idle: 3,
            churn: vec![],
            sampled_at_ms: report::now_ms(),
        });

        let lines = activity_lines(&rep);
        let joined = lines.join("\n");
        for expected in [
            "daily cost",
            "spend  burn $4.50/hr · today $2.00",
            "vs prior window  +$1.00 (+10.0%)",
            "turns 8 completed · p50 3m12s · p95 15m0s · interventions 12%",
            "human unblock p50 1m5s",
            "fleet  2 working · 1 blocked · 3 idle",
            "context bloat: abc123-def45 (/w/big) at 150.0K uncached input",
            "reasoning 4.0K tokens (40.0% of output)",
        ] {
            assert!(
                joined.contains(expected),
                "missing `{expected}` in:\n{joined}"
            );
        }
        for line in &lines {
            assert!(
                line.chars().count() <= 100,
                "activity line exceeds 100 cols: {line}"
            );
        }

        rep.wow = Some(Wow {
            cost_usd: 12.0,
            missed_cost_usd: 0.0,
        });
        let joined = activity_lines(&rep).join("\n");
        assert!(
            joined.contains("vs prior window  -$1.00 (-8.3%)"),
            "negative WoW must be sign-aware:\n{joined}"
        );
    }

    #[test]
    fn text_report_includes_the_activity_block_when_data_exists() {
        let paths = tmp_paths("activity");
        let mut rep = sample_report();
        rep.usage_note = None;
        rep.usage = Some(UsageDigest {
            events: 2,
            total_tokens: 1_000,
            known_cost_usd: 0.5,
            missed_tokens: 0,
            missed_cost_usd: 0.0,
            miss_count: 0,
            idle_misses: 0,
            model_switch_misses: 0,
            by_source: vec![],
            cache_read_tokens: 0,
            input_tokens: 0,
            cache_hit_rate: None,
            by_model: vec![],
        });
        rep.wow = Some(Wow {
            cost_usd: 0.4,
            missed_cost_usd: 0.0,
        });
        seed_snapshot(&paths, rep);
        let out = render(&nowhere_filters(), &paths, false).unwrap();
        assert!(out.contains("Activity"), "{out}");
        assert!(out.contains("vs prior window  +$0.10 (+25.0%)"), "{out}");
        fs::remove_dir_all(&paths.state_dir).ok();
    }
}
