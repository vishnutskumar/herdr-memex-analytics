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
}

fn default_scan_interval() -> u64 {
    900
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scan_interval_secs: default_scan_interval(),
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

/// Atomic write so a pane render never reads a half-written snapshot.
pub fn write_snapshot(paths: &PluginPaths, report: &Report) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("creating {}", paths.state_dir.display()))?;
    let tmp = snapshot_path(paths).with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(report)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, snapshot_path(paths))
        .with_context(|| format!("renaming into {}", snapshot_path(paths).display()))?;
    Ok(())
}

pub fn read_snapshot(paths: &PluginPaths) -> Option<Report> {
    let bytes = fs::read(snapshot_path(paths)).ok()?;
    serde_json::from_slice(&bytes).ok()
}
