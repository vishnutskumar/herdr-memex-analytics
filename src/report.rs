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
}

pub fn memex_paths(root: Option<&PathBuf>) -> Result<Paths> {
    Paths::new(root.cloned())
}

pub fn gather(filters: &Filters) -> Result<Report> {
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
    let projects = aggregate_projects(&rows);

    let (usage, project_usage, usage_note) = match usage_intel(&paths, filters.since_ms) {
        Ok((digest, per_project)) => (Some(digest), per_project, None),
        Err(err) => (
            None,
            Vec::new(),
            Some(format!("token usage unavailable: {err:#}")),
        ),
    };

    Ok(Report {
        generated_at_ms: now_ms(),
        since_ms: filters.since_ms,
        projects,
        usage,
        usage_note,
        project_usage,
    })
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

fn usage_intel(paths: &Paths, since_ms: Option<u64>) -> Result<(UsageDigest, Vec<ProjectUsage>)> {
    let config = UserConfig::load(paths)?;
    if !config.token_usage_enabled() {
        anyhow::bail!(
            "disabled; set token_usage = true in {}",
            paths.root.join("config.toml").display()
        );
    }
    let query = UsageQuery {
        source: None,
        project: None,
        project_grouping: memex::analytics::ProjectGrouping::Repository,
        session_keys: None,
        since_ms,
        until_ms: None,
        cost_mode: CostMode::Auto,
        include_events: true,
        cache_path: Some(paths.state.join("usage-cache.sqlite3")),
        memo_ttl_ms: 0,
    };
    let rep: UsageReport = scan_usage(&query)?;
    Ok((digest_from(&rep), project_usage_from(&rep.details)))
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
