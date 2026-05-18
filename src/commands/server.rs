use crate::client::SbClient;
use crate::config::{self, ResolvedConfig};
use crate::error::{SbError, SbResult};
use crate::output;

/// Builds an authenticated SbClient from CLI token override or resolved config.
/// Used as a shared helper across command modules.
pub(crate) fn build_client(cli_token: Option<&str>) -> SbResult<SbClient> {
    let space_root = crate::commands::page::find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;
    let server_url = config
        .server_url
        .value
        .as_deref()
        .ok_or_else(|| SbError::Config {
            message: "no server_url configured".into(),
        })?;
    let token = config::resolve_token(cli_token, &config)?;
    SbClient::new(server_url, &token)
}

pub(crate) fn runtime_unavailable_error() -> SbError {
    SbError::Config {
        message: "Runtime API not available on this server.\n\
                  These commands require SilverBullet to be running with a headless browser.\n\
                  See: https://silverbullet.md/Runtime%20API"
            .to_string(),
    }
}

/// Returns an error if no initialized sb space can be resolved.
/// Used as a shared helper across command modules.
pub(crate) fn require_initialized() -> SbResult<()> {
    crate::commands::page::find_space_root().map(|_| ())
}

/// Execute `sb server ping` — check connectivity and report response time.
pub async fn execute_ping(
    cli_token: Option<&str>,
    format: &crate::cli::OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    require_initialized()?;
    let client = build_client(cli_token)?;
    let elapsed = client.ping().await?;
    let ms = elapsed.as_millis();

    // Detect Runtime API availability during ping
    let rt_available = if let Ok(space_root) = crate::commands::page::find_space_root() {
        let sb_dir = space_root.join(".sb");
        crate::runtime::detect_runtime_api(&client, &sb_dir).await
    } else {
        false
    };

    match format {
        crate::cli::OutputFormat::Json => {
            let json = serde_json::json!({
                "reachable": true,
                "response_ms": ms,
                "runtime_api": rt_available,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        crate::cli::OutputFormat::Human => {
            output::print_success(&format!("Server reachable ({}ms)", ms), color, quiet);
            if rt_available {
                output::print_success("Runtime API: available", color, quiet);
            } else {
                output::print_success("Runtime API: not available", color, quiet);
            }
        }
    }

    Ok(())
}

/// Execute `sb server config` — display server configuration fields.
pub async fn execute_config(
    cli_token: Option<&str>,
    format: &crate::cli::OutputFormat,
    quiet: bool,
    _color: bool,
) -> SbResult<()> {
    require_initialized()?;
    let client = build_client(cli_token)?;
    let server_config = client.get_config().await?;

    match format {
        crate::cli::OutputFormat::Json => {
            let json_str =
                serde_json::to_string_pretty(&server_config).map_err(|e| SbError::Config {
                    message: format!("failed to serialize server config: {e}"),
                })?;
            println!("{}", json_str);
        }
        crate::cli::OutputFormat::Human => {
            if !quiet {
                println!("Server Configuration:");
                println!("  Read-only:    {}", server_config.read_only);
                println!("  Space folder: {}", server_config.space_folder_path);
                println!("  Index page:   {}", server_config.index_page);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{make_space, SbSpaceGuard};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn runtime_unavailable_error_carries_actionable_message() {
        let err = runtime_unavailable_error();
        let msg = format!("{err}");
        // Verify the user gets steered toward the docs, not just a vague error.
        assert!(msg.contains("Runtime API"), "got: {msg}");
        assert!(msg.contains("silverbullet.md"), "got: {msg}");
    }

    #[test]
    fn require_initialized_errors_when_no_space() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = SbSpaceGuard::set(tmp.path());
        // Tempdir has no .sb/ — find_space_root via SB_SPACE should fail.
        let res = require_initialized();
        assert!(res.is_err());
    }

    #[test]
    fn require_initialized_succeeds_when_space_exists() {
        let tmp = make_space(Some("http://127.0.0.1:1"));
        let _g = SbSpaceGuard::set(tmp.path());
        require_initialized().expect("space exists, should succeed");
    }

    #[test]
    fn build_client_errors_when_no_server_url_configured() {
        let tmp = tempfile::tempdir().unwrap();
        // Make a space with no server_url
        std::fs::create_dir_all(tmp.path().join(".sb")).unwrap();
        std::fs::write(tmp.path().join(".sb").join("config.toml"), "").unwrap();
        let _g = SbSpaceGuard::set(tmp.path());

        let res = build_client(None);
        let err = match res {
            Ok(_) => panic!("expected error, got Ok(SbClient)"),
            Err(e) => e,
        };
        match err {
            SbError::Config { message } => assert!(message.contains("no server_url")),
            other => panic!("expected Config error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_reports_response_time_on_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.ping"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // ping() also probes the runtime — answer 503 so detect returns false quickly.
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute_ping(None, &crate::cli::OutputFormat::Json, true, false).await;
        assert!(res.is_ok(), "{res:?}");
    }

    #[tokio::test]
    async fn ping_propagates_network_error_when_server_unreachable() {
        let tmp = make_space(Some("http://127.0.0.1:1"));
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute_ping(None, &crate::cli::OutputFormat::Human, true, false)
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::Network { .. }));
    }

    #[tokio::test]
    async fn config_returns_server_config_fields_in_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.config"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"readOnly":false,"spaceFolderPath":"/space","indexPage":"index"}"#,
            ))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute_config(None, &crate::cli::OutputFormat::Json, true, false).await;
        assert!(res.is_ok(), "{res:?}");
    }
}
