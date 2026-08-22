use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::report::Report;

/// Where the plugin keeps its state: the daemon snapshot and config.toml.
///
/// Inside herdr this is `$HERDR_PLUGIN_STATE_DIR`; standalone runs fall back to
/// `~/.herdr-memex-analytics` so the same commands work outside a pane.
pub struct PluginPaths {
    pub state_dir: PathBuf,
}

impl PluginPaths {
    pub fn new(override_dir: Option<PathBuf>) -> Result<Self> {
        let dir = override_dir
            .or_else(|| {
                std::env::var("HERDR_PLUGIN_STATE_DIR")
                    .ok()
                    .map(PathBuf::from)
            })
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|home| Path::new(&home).join(".herdr-memex-analytics"))
            })
            .context("no state directory (set HOME or --state-dir)")?;
        Ok(Self { state_dir: dir })
    }
}

#[derive(Deserialize)]
pub struct Config {
    /// Daemon cadence in seconds; mirrors memex's periodic reindex model.
    #[serde(default = "default_scan_interval")]
    pub scan_interval_secs: u64,
    /// Daily spend alert threshold in USD; None disables budget alerts.
    #[serde(default)]
    pub daily_cost_usd: Option<f64>,
    /// Urgent burn-rate alert threshold in USD per trailing hour.
    #[serde(default = "default_block_burn_rate_usd_hr")]
    pub block_burn_rate_usd_hr: f64,
    /// Uncached prompt size at which a session counts as context-bloated.
    #[serde(default = "default_context_bloat_tokens")]
    pub context_bloat_tokens: u64,
}

fn default_scan_interval() -> u64 {
    900
}

fn default_block_burn_rate_usd_hr() -> f64 {
    15.0
}

fn default_context_bloat_tokens() -> u64 {
    100_000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scan_interval_secs: default_scan_interval(),
            daily_cost_usd: None,
            block_burn_rate_usd_hr: default_block_burn_rate_usd_hr(),
            context_bloat_tokens: default_context_bloat_tokens(),
        }
    }
}

impl Config {
    /// Missing or unparsable config falls back to defaults; the daemon must never
    /// refuse to start over a bad config file.
    pub fn load(paths: &PluginPaths) -> Self {
        fs::read_to_string(paths.state_dir.join("config.toml"))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }
}

pub fn snapshot_path(paths: &PluginPaths) -> PathBuf {
    paths.state_dir.join("snapshot.json")
}

/// Atomic JSON write so a reader never sees a half-written file.
pub(crate) fn store_json<T: serde::Serialize>(path: PathBuf, value: &T) -> Result<()> {
    let dir = path.parent().context("state path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Atomic write so a pane render never reads a half-written snapshot.
pub fn write_snapshot(paths: &PluginPaths, report: &Report) -> Result<()> {
    store_json(snapshot_path(paths), report)
}

pub fn read_snapshot(paths: &PluginPaths) -> Option<Report> {
    let bytes = fs::read(snapshot_path(paths)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::ProjectStats;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "analytics-config-{label}-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_report() -> Report {
        Report {
            generated_at_ms: 1_234_567,
            since_ms: None,
            projects: vec![ProjectStats {
                project: "demo".into(),
                sessions: 3,
                messages: 30,
                active_ms: 600_000,
                last_at_ms: 1_000_000,
                sources: BTreeMap::from([("Claude".into(), 3)]),
            }],
            usage: None,
            usage_note: Some("disabled".into()),
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
    #[test]
    fn missing_config_file_falls_back_to_defaults() {
        let paths = PluginPaths {
            state_dir: tmp_dir("missing").join("does-not-exist"),
        };
        assert_eq!(Config::load(&paths).scan_interval_secs, 900);
    }

    #[test]
    fn broken_config_toml_falls_back_to_defaults() {
        let dir = tmp_dir("broken");
        fs::write(dir.join("config.toml"), "not [valid toml ===").unwrap();
        let paths = PluginPaths { state_dir: dir };
        assert_eq!(Config::load(&paths).scan_interval_secs, 900);
    }

    #[test]
    fn valid_config_override_is_honored() {
        let dir = tmp_dir("valid");
        fs::write(dir.join("config.toml"), "scan_interval_secs = 42\n").unwrap();
        let paths = PluginPaths { state_dir: dir };
        assert_eq!(Config::load(&paths).scan_interval_secs, 42);
    }

    #[test]
    fn snapshot_round_trips_through_store_and_read() {
        let dir = tmp_dir("roundtrip");
        let paths = PluginPaths { state_dir: dir };
        let rep = sample_report();
        write_snapshot(&paths, &rep).unwrap();
        let loaded = read_snapshot(&paths).expect("snapshot readable");
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].project, "demo");
        assert_eq!(loaded.projects[0].messages, 30);
    }

    #[test]
    fn missing_snapshot_reads_as_none() {
        let paths = PluginPaths {
            state_dir: tmp_dir("nosnap").join("absent"),
        };
        assert!(read_snapshot(&paths).is_none());
    }

    #[test]
    fn corrupt_snapshot_reads_as_none_without_panicking() {
        let dir = tmp_dir("corrupt");
        fs::write(dir.join("snapshot.json"), b"\xff\xfe not json {{{").unwrap();
        let paths = PluginPaths { state_dir: dir };
        assert!(read_snapshot(&paths).is_none());
    }

    #[test]
    fn state_dir_env_override_wins_over_home_fallback() {
        // PluginPaths::new with an explicit override must use it verbatim.
        let dir = tmp_dir("explicit");
        let paths = PluginPaths::new(Some(dir.clone())).unwrap();
        assert_eq!(paths.state_dir, dir);
    }
}
