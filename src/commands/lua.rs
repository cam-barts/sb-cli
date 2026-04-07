use crate::cli::OutputFormat;
use crate::commands::server::{build_client, require_initialized, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

pub async fn execute(
    cli_token: Option<&str>,
    expression: &str,
    format: &OutputFormat,
    _quiet: bool,
    _color: bool,
) -> SbResult<()> {
    require_initialized()?;
    let config = ResolvedConfig::load()?;

    // Check runtime availability
    if !config.runtime_available.value {
        return Err(runtime_unavailable_error());
    }

    let client = build_client(cli_token)?;
    let resp = client.post_text("/.runtime/lua", expression).await?;
    let status = resp.status();

    // Handle 503 specifically: Runtime API went down between detection and use
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return Err(runtime_unavailable_error());
    }

    let body = resp.text().await.map_err(|e| SbError::HttpStatus {
        status: status.as_u16(),
        url: format!("{}/.runtime/lua", client.base_url()),
        body: format!("failed to read response: {e}"),
    })?;

    // Parse response JSON: {"result": ...} or {"error": "..."}
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url: format!("{}/.runtime/lua", client.base_url()),
            body: format!("invalid JSON response: {e}"),
        })?;

    if let Some(error) = parsed.get("error").and_then(|e| e.as_str()) {
        return Err(SbError::HttpStatus {
            status: status.as_u16(),
            url: format!("{}/.runtime/lua", client.base_url()),
            body: format!("Lua error: {error}"),
        });
    }

    let result = parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            // Scalar values: print directly. Complex values: pretty-print JSON.
            match &result {
                serde_json::Value::String(s) => println!("{s}"),
                serde_json::Value::Number(n) => println!("{n}"),
                serde_json::Value::Bool(b) => println!("{b}"),
                serde_json::Value::Null => println!("null"),
                _ => println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                ),
            }
        }
    }

    Ok(())
}
