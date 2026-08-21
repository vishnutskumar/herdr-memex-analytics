use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::PluginPaths;

/// Tips the daemon currently holds; rendered by the report pane.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Tips {
    pub generated_at_ms: u64,
    pub items: Vec<crate::agents::Tip>,
}

pub fn tips_path(paths: &PluginPaths) -> PathBuf {
    paths.state_dir.join("tips.json")
}

pub fn load(paths: &PluginPaths) -> Tips {
    fs::read(tips_path(paths))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Atomic write so a pane render never reads a half-written file.
pub fn store(paths: &PluginPaths, tips: &Tips) -> Result<()> {
    crate::config::store_json(tips_path(paths), tips)
}
