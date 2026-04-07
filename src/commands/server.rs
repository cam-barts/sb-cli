use crate::client::SbClient;
use crate::config::{self, ResolvedConfig};
use crate::error::{SbError, SbResult};
use crate::output;

/// Builds an authenticated SbClient from CLI token override or resolved config.
/// Used as a shared helper across command modules.
pub(crate) fn build_client(cli_token: Option<&str>) -> SbResult<SbClient> {
    let config = ResolvedConfig::load()?;
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

/// Returns an error if the current directory is not inside an initialized sb space.
/// Used as a shared helper across command modules.
pub(crate) fn require_initialized() -> SbResult<()> {
    let cwd = std::env::current_dir().map_err(|e| SbError::Config {
        message: format!("cannot get current directory: {e}"),
    })?;
    if config::find_config_file(&cwd).is_none() {
        return Err(SbError::NotInitialized);
    }
    Ok(())
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
    let rt_available = if let Some(config_path) =
        config::find_config_file(&std::env::current_dir().unwrap_or_default())
    {
        if let Some(sb_dir) = config_path.parent() {
            crate::runtime::detect_runtime_api(&client, sb_dir).await
        } else {
            false
        }
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
