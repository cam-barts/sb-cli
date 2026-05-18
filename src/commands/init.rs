use rusqlite::Connection;
use tracing::warn;

use crate::client::SbClient;
use crate::config;
use crate::error::{SbError, SbResult};
use crate::output;

/// Initialize a local SilverBullet space in the current directory.
///
/// Creates `.sb/config.toml` and `.sb/state.db`, verifies server connectivity
/// via `/.ping` when a token is available, and cleans up on failure.
pub async fn execute(
    server_url: String,
    token: Option<String>,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    // Normalize URL: strip trailing slash
    let server_url = server_url.trim_end_matches('/').to_string();

    let space_dir = std::env::current_dir().map_err(|e| SbError::Config {
        message: format!("cannot determine current directory: {e}"),
    })?;
    let sb_dir = space_dir.join(".sb");

    // Abort if already initialized
    if sb_dir.exists() {
        return Err(SbError::AlreadyInitialized {
            path: sb_dir.display().to_string(),
        });
    }

    // Create .sb/ directory
    tokio::fs::create_dir_all(&sb_dir)
        .await
        .map_err(|e| SbError::Filesystem {
            message: "failed to create .sb directory".into(),
            path: sb_dir.display().to_string(),
            source: Some(e),
        })?;

    // Wrap the remaining init logic — on any error, clean up .sb/
    match init_inner(&sb_dir, &space_dir, &server_url, token, quiet, color).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // Clean up partial .sb/ directory
            if let Err(cleanup_err) = tokio::fs::remove_dir_all(&sb_dir).await {
                warn!("failed to clean up .sb/ after init error: {cleanup_err}");
            }
            Err(e)
        }
    }
}

async fn init_inner(
    sb_dir: &std::path::Path,
    space_dir: &std::path::Path,
    server_url: &str,
    token: Option<String>,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    // Write config.toml — only persist token if it came from --token CLI flag
    config::write_config_file(sb_dir, server_url, token.as_deref())?;

    // Register this space in the XDG user config so `sb` works from anywhere.
    // If the XDG config already points at a different path, leave it unchanged and notify.
    match config::load_user_config() {
        Ok(user_cfg) => {
            let existing_points_here = user_cfg.space.as_deref().is_some_and(|s| {
                config::expand_tilde(s)
                    .map(|p| p == space_dir)
                    .unwrap_or(false)
            });
            if user_cfg.space.is_none() || existing_points_here {
                if let Err(e) = config::write_user_config_space(space_dir) {
                    warn!("could not update XDG config: {e}");
                }
            } else if let Some(ref existing) = user_cfg.space {
                output::print_warning(
                    &format!(
                        "XDG config already points at {existing}; not changed. \
                         Run `sb config set-space {}` to switch.",
                        space_dir.display()
                    ),
                    color,
                    quiet,
                );
            }
        }
        Err(e) => {
            warn!("could not read XDG config: {e}");
        }
    }

    // Create state.db with WAL mode
    let db_path = sb_dir.join("state.db");
    let db_path_str = db_path.display().to_string();
    let db_path_clone = db_path.clone();
    tokio::task::spawn_blocking(move || -> SbResult<()> {
        let conn = Connection::open(&db_path_clone).map_err(|e| SbError::Filesystem {
            message: format!("failed to create state.db: {e}"),
            path: db_path_clone.display().to_string(),
            source: None,
        })?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| SbError::Filesystem {
                message: format!("failed to set WAL mode on state.db: {e}"),
                path: db_path_clone.display().to_string(),
                source: None,
            })?;
        Ok(())
    })
    .await
    .map_err(|e| SbError::Filesystem {
        message: format!("state.db initialization task panicked: {e}"),
        path: db_path_str,
        source: None,
    })??;

    // Warn on HTTP (insecure) URL
    if server_url.starts_with("http://") {
        output::print_warning(
            "Server URL uses HTTP (not HTTPS). Connection is not encrypted.",
            color,
            quiet,
        );
    }

    // Warn if filesystem is case-insensitive
    if let Ok(true) = output::detect_case_insensitive_fs(space_dir) {
        output::print_warning(
            "Local filesystem is case-insensitive. SilverBullet treats paths as case-sensitive \
             — be cautious with page names.",
            color,
            quiet,
        );
    }

    // Verify connectivity via /.ping when token is available
    match config::resolve_token(
        token.as_deref(),
        &config::ResolvedConfig::load_from(space_dir)?,
    ) {
        Ok(resolved_token) => {
            let client = SbClient::new(server_url, &resolved_token)?;
            let elapsed = client.ping().await?;
            output::print_success(
                &format!("Connected to {} ({}ms)", server_url, elapsed.as_millis()),
                color,
                quiet,
            );
            // Detect Runtime API availability after successful connectivity check
            let rt_available = crate::runtime::detect_runtime_api(&client, sb_dir).await;
            if rt_available {
                output::print_success("Runtime API available", color, quiet);
            }
        }
        Err(SbError::TokenNotFound { .. }) => {
            // No token available — skip ping; server may not require auth
        }
        Err(e) => return Err(e),
    }

    // Prompt for initial sync pull when TTY
    if output::is_tty() {
        let should_sync = tokio::task::spawn_blocking(|| -> bool {
            eprint!("Perform initial sync pull? [y/N] ");
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_ok() {
                matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
            } else {
                false
            }
        })
        .await
        .unwrap_or(false);

        if should_sync {
            output::print_success(
                "Run `sb sync pull` to download server content.",
                color,
                quiet,
            );
        }
    }

    // Final success message
    output::print_success(
        &format!("Initialized sb space at {}", sb_dir.display()),
        color,
        quiet,
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    // The "already initialized", "WAL mode", and "write_config_file" cases that
    // used to live here were testing things `init::execute` doesn't itself do
    // (constructing an error variant; using `Connection::open` directly; calling
    // `config::write_config_file` directly). They've been replaced by the
    // `execute_tests` module below, which calls `execute` against a tempdir and
    // verifies the observable result (.sb/state.db on disk, config contents,
    // error variants).

    mod execute_tests {
        use super::super::*;
        use crate::test_util::{CwdGuard, XdgGuard};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn execute_creates_sb_dir_with_config_and_state_db() {
            let space = tempfile::tempdir().unwrap();
            let xdg = tempfile::tempdir().unwrap();
            let _g = CwdGuard::set(space.path());
            let _xg = XdgGuard::set(xdg.path());

            // No token => skips ping. Pass URL with trailing slash to exercise normalization.
            execute("https://example.com/".to_string(), None, true, false)
                .await
                .expect("init should succeed without token");

            let sb_dir = space.path().join(".sb");
            assert!(sb_dir.is_dir(), ".sb/ should exist");
            assert!(sb_dir.join("state.db").is_file(), "state.db should exist");
            let cfg = std::fs::read_to_string(sb_dir.join("config.toml")).unwrap();
            assert!(
                cfg.contains("https://example.com"),
                "config.toml should contain server_url"
            );
            assert!(
                !cfg.contains("https://example.com/"),
                "trailing slash should be stripped before write"
            );
        }

        #[tokio::test]
        async fn execute_rejects_when_sb_dir_already_exists() {
            let space = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(space.path().join(".sb")).unwrap();
            let xdg = tempfile::tempdir().unwrap();
            let _g = CwdGuard::set(space.path());
            let _xg = XdgGuard::set(xdg.path());

            let err = execute("https://example.com".to_string(), None, true, false)
                .await
                .unwrap_err();
            assert!(matches!(err, SbError::AlreadyInitialized { .. }));
        }

        #[tokio::test]
        async fn execute_cleans_up_sb_dir_when_ping_fails() {
            let space = tempfile::tempdir().unwrap();
            let xdg = tempfile::tempdir().unwrap();
            let _g = CwdGuard::set(space.path());
            let _xg = XdgGuard::set(xdg.path());

            // Use unreachable URL; ping will fail with Network error, init_inner errors,
            // and execute should remove the partial .sb/.
            let err = execute(
                "http://127.0.0.1:1".to_string(),
                Some("tok".to_string()),
                true,
                false,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, SbError::Network { .. }));
            assert!(
                !space.path().join(".sb").exists(),
                ".sb/ should be cleaned up after init failure"
            );
        }

        #[tokio::test]
        async fn execute_with_token_pings_server_and_succeeds() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.ping"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
            // probe_runtime_api answers; 503 makes detect_runtime_api return false.
            Mock::given(method("POST"))
                .and(path("/.runtime/lua"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;

            let space = tempfile::tempdir().unwrap();
            let xdg = tempfile::tempdir().unwrap();
            let _g = CwdGuard::set(space.path());
            let _xg = XdgGuard::set(xdg.path());

            execute(server.uri(), Some("tok".to_string()), true, false)
                .await
                .expect("init with reachable server should succeed");

            let cfg =
                std::fs::read_to_string(space.path().join(".sb").join("config.toml")).unwrap();
            assert!(
                cfg.contains("tok"),
                "token should be persisted when passed as flag"
            );
        }
    }
}
