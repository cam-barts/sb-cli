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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a sandboxed `.sb/` directory; detect_runtime_api writes to config.toml under it.
    fn make_sb_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sb_dir = tmp.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).unwrap();
        std::fs::write(sb_dir.join("config.toml"), "").unwrap();
        tmp
    }

    #[tokio::test]
    async fn detect_runtime_api_returns_true_and_persists_when_runtime_present() {
        let server = MockServer::start().await;
        // probe_runtime_api posts to /.runtime/lua expecting a 200/400 (anything not 503)
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let client = SbClient::new(&server.uri(), "tok").unwrap();
        let tmp = make_sb_dir();
        let sb_dir = tmp.path().join(".sb");

        let available = detect_runtime_api(&client, &sb_dir).await;

        // Behavior: when server responds with non-503, available=true and config persists.
        assert!(available);
        let content = std::fs::read_to_string(sb_dir.join("config.toml")).unwrap();
        assert!(
            content.contains("available = true"),
            "expected config to record runtime.available=true, got: {content}"
        );
    }

    #[tokio::test]
    async fn detect_runtime_api_returns_false_and_persists_when_runtime_503() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = SbClient::new(&server.uri(), "tok").unwrap();
        let tmp = make_sb_dir();
        let sb_dir = tmp.path().join(".sb");

        let available = detect_runtime_api(&client, &sb_dir).await;

        assert!(!available);
        let content = std::fs::read_to_string(sb_dir.join("config.toml")).unwrap();
        assert!(
            content.contains("available = false"),
            "expected config to record runtime.available=false, got: {content}"
        );
    }

    #[tokio::test]
    async fn detect_runtime_api_returns_false_when_probe_errors() {
        // Use an unreachable URL to force a network error on probe.
        let client = SbClient::new("http://127.0.0.1:1", "tok").unwrap();
        let tmp = make_sb_dir();
        let sb_dir = tmp.path().join(".sb");

        let available = detect_runtime_api(&client, &sb_dir).await;

        // Network error → record false, return false, do not propagate the error.
        assert!(!available);
    }
}
