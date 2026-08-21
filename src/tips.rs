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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Tip;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn tmp_paths(label: &str) -> PluginPaths {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "analytics-tips-{label}-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            nanos
        ));
        PluginPaths { state_dir: dir }
    }

    fn sample_tips() -> Tips {
        Tips {
            generated_at_ms: 42,
            items: vec![Tip {
                pane_id: "w1:p1".into(),
                message: "claude has been blocked 6m — it needs input".into(),
                urgent: true,
            }],
        }
    }

    #[test]
    fn tips_round_trip_through_store_and_load() {
        let paths = tmp_paths("roundtrip");
        let tips = sample_tips();
        store(&paths, &tips).expect("store");
        let loaded = load(&paths);
        assert_eq!(loaded.generated_at_ms, 42);
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.items[0].pane_id, "w1:p1");
        assert!(loaded.items[0].urgent);
        std::fs::remove_dir_all(&paths.state_dir).ok();
    }

    #[test]
    fn missing_tips_file_loads_as_default() {
        let paths = tmp_paths("missing");
        let loaded = load(&paths);
        assert_eq!(loaded.generated_at_ms, 0);
        assert!(loaded.items.is_empty());
    }

    #[test]
    fn corrupt_tips_file_loads_as_default_without_panicking() {
        let paths = tmp_paths("corrupt");
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(tips_path(&paths), b"{ truncated json").unwrap();
        let loaded = load(&paths);
        assert!(loaded.items.is_empty());
        std::fs::remove_dir_all(&paths.state_dir).ok();
    }
}
