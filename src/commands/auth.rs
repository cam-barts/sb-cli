use crate::config::{self, ConfigFile};
use crate::error::{SbError, SbResult};
use crate::output;

/// Execute `sb auth set` — store an auth token to the current space config.
pub async fn execute_set(token_flag: Option<String>, quiet: bool, color: bool) -> SbResult<()> {
    // Require initialized space
    let cwd = std::env::current_dir().map_err(|e| SbError::Config {
        message: format!("cannot determine current directory: {e}"),
    })?;
    let config_path = config::find_config_file(&cwd).ok_or(SbError::NotInitialized)?;
    let sb_dir = config_path
        .parent()
        .ok_or_else(|| SbError::Config {
            message: "config file has no parent directory".into(),
        })?
        .to_path_buf();

    // Determine token value
    let token = if let Some(t) = token_flag {
        t
    } else if output::is_tty() {
        // Interactive prompt (use eprint! to avoid echoing to stdout)
        tokio::task::spawn_blocking(|| {
            eprint!("Enter auth token: ");
            use std::io::Write as _;
            std::io::stderr().flush().ok();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();
            input.trim().to_string()
        })
        .await
        .map_err(|e| SbError::Config {
            message: format!("prompt failed: {e}"),
        })?
    } else {
        // Non-TTY: read one line from stdin
        tokio::task::spawn_blocking(|| {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).ok();
            s.trim().to_string()
        })
        .await
        .map_err(|e| SbError::Config {
            message: format!("stdin read failed: {e}"),
        })?
    };

    if token.is_empty() {
        return Err(SbError::Usage("no token provided".into()));
    }

    // Load existing config to preserve server_url (read before overwrite)
    let content = std::fs::read_to_string(&config_path).map_err(|e| SbError::Filesystem {
        message: "cannot read config file".into(),
        path: config_path.display().to_string(),
        source: Some(e),
    })?;
    let existing: ConfigFile = toml::from_str(&content).map_err(|e| SbError::Config {
        message: format!("invalid config: {e}"),
    })?;
    let server_url = existing.server_url.ok_or_else(|| SbError::Config {
        message: "no server_url found in existing config".into(),
    })?;

    // Check if keychain storage is enabled
    let resolved_config = config::ResolvedConfig::load_from(&cwd)?;
    if resolved_config.auth_keychain.value {
        // Write to OS keychain instead of config.toml
        crate::keychain::set_token(&server_url, &token)?;
        output::print_success("Auth token stored in OS keychain", color, quiet);
    } else {
        // Original behavior: write to config.toml
        config::write_config_file(&sb_dir, &server_url, Some(&token))?;
        output::print_success("Auth token updated", color, quiet);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::CwdGuard;

    #[tokio::test]
    async fn errors_when_no_space_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = CwdGuard::set(tmp.path());
        let err = execute_set(Some("tok".into()), true, false)
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::NotInitialized));
    }

    #[tokio::test]
    async fn errors_when_token_flag_is_empty_string() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".sb")).unwrap();
        std::fs::write(
            tmp.path().join(".sb").join("config.toml"),
            "server_url = \"https://example.com\"\n",
        )
        .unwrap();
        let _g = CwdGuard::set(tmp.path());

        let err = execute_set(Some(String::new()), true, false)
            .await
            .unwrap_err();
        match err {
            SbError::Usage(msg) => assert!(msg.contains("no token")),
            other => panic!("expected Usage, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_when_config_has_no_server_url() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".sb")).unwrap();
        // Initialized space but server_url is absent — auth set needs it to anchor the token.
        std::fs::write(tmp.path().join(".sb").join("config.toml"), "").unwrap();
        let _g = CwdGuard::set(tmp.path());

        let err = execute_set(Some("tok".into()), true, false)
            .await
            .unwrap_err();
        match err {
            SbError::Config { message } => assert!(message.contains("no server_url")),
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn writes_token_to_config_file_when_keychain_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".sb")).unwrap();
        std::fs::write(
            tmp.path().join(".sb").join("config.toml"),
            "server_url = \"https://example.com\"\n",
        )
        .unwrap();
        let _g = CwdGuard::set(tmp.path());

        execute_set(Some("new-secret-token".into()), true, false)
            .await
            .expect("auth set should succeed");

        let content = std::fs::read_to_string(tmp.path().join(".sb").join("config.toml")).unwrap();
        // Behavior: token is now present, server_url is preserved.
        assert!(
            content.contains("new-secret-token"),
            "token must be written"
        );
        assert!(
            content.contains("example.com"),
            "server_url must be preserved"
        );
    }

    #[tokio::test]
    async fn overwrites_existing_token_in_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".sb")).unwrap();
        std::fs::write(
            tmp.path().join(".sb").join("config.toml"),
            "server_url = \"https://example.com\"\ntoken = \"old-token\"\n",
        )
        .unwrap();
        let _g = CwdGuard::set(tmp.path());

        execute_set(Some("brand-new-token".into()), true, false)
            .await
            .expect("auth set");
        let content = std::fs::read_to_string(tmp.path().join(".sb").join("config.toml")).unwrap();
        assert!(content.contains("brand-new-token"));
        assert!(!content.contains("old-token"), "old token must be gone");
    }
}
