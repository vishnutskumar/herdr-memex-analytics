use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use memex::analytics::{AnalyticsStore, SessionDetailRow};
use memex::config::{Paths, UserConfig};
use memex::usage::{CostMode, UsageQuery, UsageReport, scan_usage};

/// Query filters shared by every entrypoint.
#[derive(Clone)]
pub struct Filters {
    /// memex data directory override (None = ~/.memex)
    pub root: Option<PathBuf>,
    pub since_ms: Option<u64>,
    pub project: Option<String>,
    /// In-process scan memo TTL; the daemon passes twice its interval so
    /// back-to-back cycles share one corpus assembly. Zero disables the memo.
    pub memo_ttl_ms: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectStats {
    pub project: String,
    pub sessions: u64,
    pub messages: u64,
    /// Sum of per-session wall time from first to last message.
    pub active_ms: u64,
    pub last_at_ms: u64,
    pub sources: BTreeMap<String, u64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct UsageDigest {
    pub events: u64,
    pub total_tokens: u64,
    pub known_cost_usd: f64,
    pub missed_tokens: u64,
    pub missed_cost_usd: f64,
    pub miss_count: u64,
    pub idle_misses: u64,
    pub model_switch_misses: u64,
    pub by_source: Vec<SourceDigest>,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Prompt-token denominator of `cache_hit_rate`, mirroring memex's
    /// cache-chain definition: uncached_input + cache_read + cache_write.
    #[serde(default)]
    pub input_tokens: u64,
    /// cache_read_tokens / input_tokens; None when no prompt tokens were seen.
    #[serde(default)]
    pub cache_hit_rate: Option<f64>,
    #[serde(default)]
    pub by_model: Vec<ModelDigest>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ModelDigest {
    pub model: String,
    pub events: u64,
    pub total_tokens: u64,
    pub known_cost_usd: f64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ProjectUsage {
    pub project: String,
    pub events: u64,
    pub total_tokens: u64,
    pub known_cost_usd: f64,
    pub missed_cost_usd: f64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SourceDigest {
    pub source: String,
    pub events: u64,
    pub total_tokens: u64,
    pub known_cost_usd: f64,
    pub missed_tokens: u64,
}
#[derive(Serialize, Deserialize, Clone)]
pub struct DayPoint {
    /// Local calendar day, YYYY-MM-DD.
    pub date: String,
    pub tokens: u64,
    pub cost_usd: f64,
    pub events: u64,
    pub sessions: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BloatSession {
    pub session_id: String,
    pub project: String,
    pub last_uncached_input: u64,
}

/// Week-over-week comparison inputs: the figures of the preceding
/// equal-length window. Consumers diff them against the current digest to
/// render "vs prior" deltas.
#[derive(Serialize, Deserialize, Clone)]
pub struct Wow {
    pub cost_usd: f64,
    pub missed_cost_usd: f64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Report {
    pub generated_at_ms: u64,
    pub since_ms: Option<u64>,
    pub projects: Vec<ProjectStats>,
    /// Present only when memex has token usage tracking enabled.
    pub usage: Option<UsageDigest>,
    pub usage_note: Option<String>,
    /// Per-project cost attribution, top 10 by known cost; empty when usage
    /// tracking is disabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_usage: Vec<ProjectUsage>,
    /// Per-local-day activity for every day with usage events, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub daily: Vec<DayPoint>,
    /// 7 rows = the last 7 local days oldest->newest; token sums per hour of
    /// day. Empty when no usage data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_heatmap: Vec<[u64; 24]>,
    /// Source-reported USD spent in the trailing hour (i.e. USD/hr); None
    /// when usage is unavailable.
    #[serde(default)]
    pub burn_rate_usd_per_hr: Option<f64>,
    /// Source-reported USD spent today (local date); None when usage is
    /// unavailable.
    #[serde(default)]
    pub today_cost_usd: Option<f64>,
    #[serde(default)]
    pub reasoning_tokens: u64,
    /// reasoning / output; None when no output tokens were seen.
    #[serde(default)]
    pub reasoning_share: Option<f64>,
    /// Sessions whose uncached prompt grew monotonically past the bloat
    /// threshold; worst 5 by final size.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bloating_sessions: Vec<BloatSession>,
    /// Prior-window cost figures for week-over-week comparison; present only
    /// when the query window has a bounded start.
    #[serde(default)]
    pub wow: Option<Wow>,
    #[serde(default)]
    pub turns: Option<crate::agents::TurnStats>,
    /// Fleet status; only the daemon sets this, never `gather`.
    #[serde(default)]
    pub fleet: Option<crate::live::FleetSnapshot>,
}

pub fn memex_paths(root: Option<&PathBuf>) -> Result<Paths> {
    Paths::new(root.cloned())
}

pub fn gather(filters: &Filters, plugin: Option<&crate::config::PluginPaths>) -> Result<Report> {
    let paths = memex_paths(filters.root.as_ref())?;
    let store = AnalyticsStore::open_read_only(paths.state.join("analytics.sqlite"))
        .context("opening memex analytics store (run `memex index` first)")?;

    let rows = store.query_sessions_detailed(
        None,
        filters.project.as_deref(),
        None,
        filters.since_ms,
        None,
    )?;
    drop(store);
    let projects = aggregate_projects(&rows);

    let generated_at_ms = now_ms();
    let bloat_threshold = plugin
        .map(crate::config::Config::load)
        .unwrap_or_default()
        .context_bloat_tokens;

    let (intel, usage_note) = match usage_intel(&paths, filters) {
        Ok(intel) => (Some(intel), None),
        Err(err) => (None, Some(format!("token usage unavailable: {err:#}"))),
    };

    let sessions_by_day: BTreeMap<String, u64> =
        rows.iter().fold(BTreeMap::new(), |mut acc, row| {
            *acc.entry(local_day(row.started_at)).or_insert(0) += 1;
            acc
        });

    let empty: Vec<memex::usage::UsageEvent> = Vec::new();
    let details: &[memex::usage::UsageEvent] = match &intel {
        Some(i) => &i.rep.details,
        None => &empty,
    };
    let daily = daily_series(details, &sessions_by_day);
    let activity_heatmap = activity_heatmap(details, generated_at_ms);
    let burn_rate_usd_per_hr = (!details.is_empty()).then(|| {
        let cutoff = generated_at_ms.saturating_sub(3_600_000);
        details
            .iter()
            .filter(|event| event.timestamp_ms >= cutoff)
            .map(event_known_cost_usd)
            .sum()
    });
    let today = local_day(generated_at_ms);
    let today_cost_usd = (!details.is_empty()).then(|| {
        details
            .iter()
            .filter(|event| local_day(event.timestamp_ms) == today)
            .map(event_known_cost_usd)
            .sum::<f64>()
    });
    let reasoning_tokens: u64 = details.iter().map(|event| event.tokens.reasoning).sum();
    let output_tokens: u64 = details.iter().map(|event| event.tokens.output).sum();
    let reasoning_share =
        (output_tokens > 0).then(|| reasoning_tokens as f64 / output_tokens as f64);
    let bloating_sessions = bloating_sessions(details, bloat_threshold);
    let wow = match (&intel, filters.since_ms) {
        (Some(_), Some(since)) => wow_prior(&paths, filters, since, generated_at_ms).ok(),
        _ => None,
    };
    let (digest, per_project) = match intel {
        Some(i) => (Some(i.digest), i.per_project),
        None => (None, Vec::new()),
    };

    Ok(Report {
        generated_at_ms,
        since_ms: filters.since_ms,
        projects,
        usage: digest,
        usage_note,
        project_usage: per_project,
        daily,
        activity_heatmap,
        burn_rate_usd_per_hr,
        today_cost_usd,
        reasoning_tokens,
        reasoning_share,
        bloating_sessions,
        wow,
        turns: plugin.and_then(crate::agents::turn_stats),
        fleet: None,
    })
}

fn local_datetime(ms: u64) -> chrono::DateTime<chrono::Local> {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
}

fn local_day(ms: u64) -> String {
    local_datetime(ms).format("%Y-%m-%d").to_string()
}

/// Per-local-day usage series over the query window, oldest first; session
/// counts come from the sessions table bucketed by local start date.
fn daily_series(
    details: &[memex::usage::UsageEvent],
    sessions_by_day: &BTreeMap<String, u64>,
) -> Vec<DayPoint> {
    let mut by_day: BTreeMap<String, DayPoint> = BTreeMap::new();
    for event in details {
        let date = local_day(event.timestamp_ms);
        let point = by_day.entry(date.clone()).or_insert_with(|| DayPoint {
            date,
            tokens: 0,
            cost_usd: 0.0,
            events: 0,
            sessions: 0,
        });
        point.tokens = point.tokens.saturating_add(event.tokens.total());
        point.cost_usd += event_known_cost_usd(event);
        point.events += 1;
    }
    let mut daily: Vec<DayPoint> = by_day.into_values().collect();
    for point in &mut daily {
        point.sessions = sessions_by_day.get(&point.date).copied().unwrap_or(0);
    }
    daily
}

/// Token sums per hour of day for the last 7 local days (rows
/// oldest->newest); empty when there is no usage data at all.
fn activity_heatmap(details: &[memex::usage::UsageEvent], now_ms: u64) -> Vec<[u64; 24]> {
    use chrono::{Days, Timelike};

    if details.is_empty() {
        return Vec::new();
    }
    let today = local_datetime(now_ms).date_naive();
    let mut row_of: BTreeMap<String, usize> = BTreeMap::new();
    let mut grid: Vec<[u64; 24]> = Vec::with_capacity(7);
    for offset in (0..7).rev() {
        let date = today - Days::new(offset);
        row_of.insert(date.format("%Y-%m-%d").to_string(), grid.len());
        grid.push([0; 24]);
    }
    for event in details {
        let dt = local_datetime(event.timestamp_ms);
        if let Some(row) = row_of.get(&dt.format("%Y-%m-%d").to_string()) {
            grid[*row][dt.hour() as usize] =
                grid[*row][dt.hour() as usize].saturating_add(event.tokens.total());
        }
    }
    grid
}

fn display_project(project: Option<&str>) -> String {
    match project {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => "unknown".to_string(),
    }
}

/// Sessions whose time-ordered uncached prompt grew strictly across at least
/// 3 events and ended at or above `threshold`; worst 5 by final size.
fn bloating_sessions(details: &[memex::usage::UsageEvent], threshold: u64) -> Vec<BloatSession> {
    let mut by_session: BTreeMap<String, Vec<&memex::usage::UsageEvent>> = BTreeMap::new();
    for event in details {
        if let Some(id) = event.session_id.as_deref().filter(|id| !id.is_empty()) {
            by_session.entry(id.to_string()).or_default().push(event);
        }
    }
    let mut out: Vec<BloatSession> = by_session
        .into_iter()
        .filter_map(|(session_id, mut events)| {
            events.sort_by_key(|event| event.timestamp_ms);
            let monotonic = events.len() >= 3
                && events
                    .windows(2)
                    .all(|pair| pair[0].tokens.uncached_input < pair[1].tokens.uncached_input);
            let last = events.last()?;
            (monotonic && last.tokens.uncached_input >= threshold).then(|| BloatSession {
                session_id,
                project: display_project(last.project.as_deref()),
                last_uncached_input: last.tokens.uncached_input,
            })
        })
        .collect();
    out.sort_by_key(|session| std::cmp::Reverse(session.last_uncached_input));
    out.truncate(5);
    out
}

fn aggregate_projects(rows: &[SessionDetailRow]) -> Vec<ProjectStats> {
    let mut by_project: BTreeMap<String, ProjectStats> = BTreeMap::new();
    for row in rows {
        let display = match row.repo_project.as_deref() {
            Some(repo) if !repo.is_empty() => repo.to_string(),
            _ => row.project.clone(),
        };
        let entry = by_project
            .entry(display.clone())
            .or_insert_with(|| ProjectStats {
                project: display,
                sessions: 0,
                messages: 0,
                active_ms: 0,
                last_at_ms: 0,
                sources: BTreeMap::new(),
            });
        entry.sessions += 1;
        entry.messages += row.message_count;
        entry.active_ms += row.last_at.saturating_sub(row.started_at);
        entry.last_at_ms = entry.last_at_ms.max(row.last_at);
        *entry
            .sources
            .entry(format!("{:?}", row.source))
            .or_insert(0) += 1;
    }
    let mut projects: Vec<ProjectStats> = by_project.into_values().collect();
    projects.sort_by_key(|p| std::cmp::Reverse(p.last_at_ms));
    projects
}

struct Intel {
    rep: UsageReport,
    digest: UsageDigest,
    per_project: Vec<ProjectUsage>,
}

fn usage_query(
    paths: &Paths,
    filters: &Filters,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
) -> UsageQuery {
    UsageQuery {
        source: None,
        project: None,
        project_grouping: memex::analytics::ProjectGrouping::Repository,
        session_keys: None,
        since_ms,
        until_ms,
        cost_mode: CostMode::Auto,
        include_events: true,
        cache_path: Some(paths.state.join("usage-cache.sqlite3")),
        memo_ttl_ms: filters.memo_ttl_ms,
    }
}

fn usage_intel(paths: &Paths, filters: &Filters) -> Result<Intel> {
    let config = UserConfig::load(paths)?;
    if !config.token_usage_enabled() {
        anyhow::bail!(
            "disabled; set token_usage = true in {}",
            paths.root.join("config.toml").display()
        );
    }
    let rep: UsageReport = scan_usage(&usage_query(paths, filters, filters.since_ms, None))?;
    let digest = digest_from(&rep);
    let per_project = project_usage_from(&rep.details);
    Ok(Intel {
        rep,
        digest,
        per_project,
    })
}

/// Cost figures of the preceding equal-length window for week-over-week
/// comparison. The second scan reuses the memoized corpus assembly when the
/// memo TTL allows it.
fn wow_prior(paths: &Paths, filters: &Filters, since_ms: u64, now_ms: u64) -> Result<Wow> {
    let span = now_ms.saturating_sub(since_ms).max(1);
    let prior_since = since_ms.saturating_sub(span);
    let rep: UsageReport = scan_usage(&usage_query(
        paths,
        filters,
        Some(prior_since),
        Some(since_ms),
    ))?;
    Ok(Wow {
        cost_usd: rep.known_cost_usd,
        missed_cost_usd: rep.cache_waste.missed_cost_usd,
    })
}

/// Per-event cost memex actually charged for, from the provider-reported figure.
/// memex's catalog fallback pricing is not exposed per event, so events without
/// `source_cost_usd` contribute tokens but no cost here.
fn event_known_cost_usd(event: &memex::usage::UsageEvent) -> f64 {
    event
        .source_cost_usd
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .unwrap_or(0.0)
}

fn digest_from(rep: &UsageReport) -> UsageDigest {
    let mut cache_read_tokens = 0u64;
    let mut input_tokens = 0u64;
    let mut by_model: BTreeMap<String, ModelDigest> = BTreeMap::new();
    for event in &rep.details {
        cache_read_tokens = cache_read_tokens.saturating_add(event.tokens.cache_read);
        input_tokens = input_tokens
            .saturating_add(event.tokens.uncached_input)
            .saturating_add(event.tokens.cache_read)
            .saturating_add(event.tokens.cache_write);
        let model = match event.model.as_deref() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => "unknown".to_string(),
        };
        let entry = by_model
            .entry(model.clone())
            .or_insert_with(|| ModelDigest {
                model,
                events: 0,
                total_tokens: 0,
                known_cost_usd: 0.0,
            });
        entry.events += 1;
        entry.total_tokens = entry.total_tokens.saturating_add(event.tokens.total());
        entry.known_cost_usd += event_known_cost_usd(event);
    }
    let mut by_model: Vec<ModelDigest> = by_model.into_values().collect();
    by_model.sort_by_key(|m| std::cmp::Reverse(m.total_tokens));
    let cache_hit_rate = (input_tokens > 0).then(|| cache_read_tokens as f64 / input_tokens as f64);
    UsageDigest {
        events: rep.events,
        total_tokens: rep.total_tokens,
        known_cost_usd: rep.known_cost_usd,
        missed_tokens: rep.cache_waste.missed_tokens,
        missed_cost_usd: rep.cache_waste.missed_cost_usd,
        miss_count: rep.cache_waste.miss_count,
        idle_misses: rep.cache_waste.idle_misses,
        model_switch_misses: rep.cache_waste.model_switch_misses,
        by_source: rep
            .by_source
            .iter()
            .map(|s| SourceDigest {
                source: s.source.clone(),
                events: s.events,
                total_tokens: s.total_tokens,
                known_cost_usd: s.known_cost_usd,
                missed_tokens: s.cache_waste.missed_tokens,
            })
            .collect(),
        cache_read_tokens,
        input_tokens,
        cache_hit_rate,
        by_model,
    }
}

fn project_usage_from(details: &[memex::usage::UsageEvent]) -> Vec<ProjectUsage> {
    let mut by_project: BTreeMap<String, ProjectUsage> = BTreeMap::new();
    for event in details {
        let project = match event.project.as_deref() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => "unknown".to_string(),
        };
        let entry = by_project
            .entry(project.clone())
            .or_insert_with(|| ProjectUsage {
                project,
                events: 0,
                total_tokens: 0,
                known_cost_usd: 0.0,
                // Per-missed-token cost needs memex's private price catalog
                // (usage::cache_miss_cost_usd is not public), so attribution
                // reports 0 rather than a fabricated estimate; the digest-level
                // figure stays authoritative.
                missed_cost_usd: 0.0,
            });
        entry.events += 1;
        entry.total_tokens = entry.total_tokens.saturating_add(event.tokens.total());
        entry.known_cost_usd += event_known_cost_usd(event);
    }
    let mut usage: Vec<ProjectUsage> = by_project.into_values().collect();
    usage.sort_by(|a, b| b.known_cost_usd.total_cmp(&a.known_cost_usd));
    usage.truncate(10);
    usage
}

pub(crate) fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}
