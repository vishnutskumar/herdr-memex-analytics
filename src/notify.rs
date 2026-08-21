use crate::config::PluginPaths;

/// herdr-native notification; never fails the caller (hook or daemon).
pub fn show(paths: &PluginPaths, body: &str) {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let _ = std::process::Command::new(bin)
        .args(["notification", "show", "analytics", "--body", body])
        .env("HERDR_PLUGIN_STATE_DIR", &paths.state_dir)
        .status();
}
