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

    let (usage, usage_note) = match usage_digest(&paths, filters.since_ms) {
        Ok(digest) => (Some(digest), None),
        Err(err) => (None, Some(format!("token usage unavailable: {err:#}"))),
    };

    Ok(Report {
        generated_at_ms: now_ms(),
        since_ms: filters.since_ms,
        projects,
        usage,
        usage_note,
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

fn usage_digest(paths: &Paths, since_ms: Option<u64>) -> Result<UsageDigest> {
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
        include_events: false,
        cache_path: Some(paths.state.join("usage-cache.sqlite3")),
        memo_ttl_ms: 0,
    };
    let rep: UsageReport = scan_usage(&query)?;
    Ok(digest_from(&rep))
}

fn digest_from(rep: &UsageReport) -> UsageDigest {
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
    }
}

pub(crate) fn now_ms() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0)
}
