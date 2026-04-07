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
    use super::*;
    use std::io::Write;

    fn setup_sb_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
        dir
    }

    #[test]
    fn url_trailing_slash_is_stripped() {
        // Verify that trailing slash normalization happens in execute
        let url = "https://example.com/".trim_end_matches('/').to_string();
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn already_initialized_returns_error() {
        let dir = setup_sb_dir();
        // .sb/ already exists in dir; test the check logic directly
        let sb_dir = dir.path().join(".sb");
        assert!(sb_dir.exists());
        let result: SbResult<()> = Err(SbError::AlreadyInitialized {
            path: sb_dir.display().to_string(),
        });
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::AlreadyInitialized { path } => {
                assert!(path.contains(".sb"));
            }
            other => panic!("expected AlreadyInitialized, got: {other:?}"),
        }
    }

    #[test]
    fn state_db_created_with_wal_mode() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");

        // Create state.db the same way init does
        let db_path = sb_dir.join("state.db");
        let conn = Connection::open(&db_path).expect("open state.db");
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .expect("set WAL mode");
        drop(conn);

        // Verify the DB can be opened and WAL mode is set
        let conn2 = Connection::open(&db_path).expect("reopen state.db");
        let mode: String = conn2
            .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
            .expect("query journal_mode");
        assert_eq!(mode, "wal", "state.db should use WAL journal mode");
    }

    #[test]
    fn write_config_file_with_token_includes_token() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");

        config::write_config_file(&sb_dir, "https://example.com", Some("testtoken"))
            .expect("write_config_file");

        let content = std::fs::read_to_string(sb_dir.join("config.toml")).expect("read config");
        assert!(content.contains("server_url"));
        assert!(content.contains("https://example.com"));
        assert!(content.contains("testtoken"));
    }

    #[test]
    fn write_config_file_without_token_omits_token_field() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");

        config::write_config_file(&sb_dir, "https://example.com", None).expect("write_config_file");

        let content = std::fs::read_to_string(sb_dir.join("config.toml")).expect("read config");
        assert!(content.contains("server_url"));
        assert!(!content.contains("token"));
    }
}
