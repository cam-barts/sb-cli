use crate::client::SbClient;
use crate::config;
use std::path::Path;
use tracing::debug;

/// Detect Runtime API availability by probing POST /.runtime/lua.
/// Records result as runtime.available in config.toml.
/// Returns the detection result.
///
/// This is best-effort: network failures or missing tokens result in
/// recording false, never blocking the calling command.
pub async fn detect_runtime_api(client: &SbClient, sb_dir: &Path) -> bool {
    match client.probe_runtime_api().await {
        Ok(available) => {
            debug!(available, "Runtime API detection result");
            if let Err(e) = config::update_config_value(sb_dir, "runtime", "available", available) {
                debug!("failed to persist runtime.available: {e}");
            }
            available
        }
        Err(e) => {
            debug!("Runtime API probe failed: {e}");
            let _ = config::update_config_value(sb_dir, "runtime", "available", false);
            false
        }
    }
}
