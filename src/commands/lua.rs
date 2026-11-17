use crate::cli::OutputFormat;
use crate::commands::server::{build_client, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

pub async fn execute(
    cli_token: Option<&str>,
    expression: Option<&str>,
    script: Option<&str>,
    format: &OutputFormat,
    _quiet: bool,
    _color: bool,
) -> SbResult<()> {
    let space_root = crate::commands::page::find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;

    // Check runtime availability
    if !config.runtime_available.value {
        return Err(runtime_unavailable_error());
    }

    // Resolve which endpoint + body to send. `--script` reads the file (or
    // stdin for `-`) BEFORE any network call, so a missing file surfaces as a
    // filesystem error naming the path rather than a confusing HTTP failure.
    // clap's `conflicts_with` already rejects passing both, so only three
    // shapes reach here.
    let (endpoint, body) = match (expression, script) {
        (Some(expr), None) => ("/.runtime/lua", expr.to_string()),
        (None, Some(script_path)) => ("/.runtime/lua_script", read_script_source(script_path)?),
        (None, None) => {
            return Err(SbError::Usage(
                "pass a Lua expression, or --script <file> for a multi-statement script"
                    .to_string(),
            ))
        }
        (Some(_), Some(_)) => unreachable!("clap conflicts_with rejects expression + --script"),
    };

    let client = build_client(cli_token)?;
    let result = crate::runtime::eval(&client, endpoint, &body).await?;

    render_result(&result, format);
    Ok(())
}

/// Read a Lua script body from a file path, or from stdin when `script_path`
/// is `-`. A missing/unreadable file becomes `SbError::Filesystem` naming the
/// path, not a raw io error or an HTTP failure.
fn read_script_source(script_path: &str) -> SbResult<String> {
    if script_path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| SbError::Filesystem {
                message: "failed to read Lua script from stdin".to_string(),
                path: "-".to_string(),
                source: Some(e),
            })?;
        Ok(buf)
    } else {
        std::fs::read_to_string(script_path).map_err(|e| SbError::Filesystem {
            message: "failed to read Lua script file".to_string(),
            path: script_path.to_string(),
            source: Some(e),
        })
    }
}

/// Render a Runtime API result identically regardless of which endpoint
/// produced it, so expression mode and `--script` mode are indistinguishable
/// in their output.
fn render_result(result: &serde_json::Value, format: &OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(result).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            // Scalar values: print directly. Complex values: pretty-print JSON.
            match result {
                serde_json::Value::String(s) => println!("{s}"),
                serde_json::Value::Number(n) => println!("{n}"),
                serde_json::Value::Bool(b) => println!("{b}"),
                serde_json::Value::Null => println!("null"),
                _ => println!(
                    "{}",
                    serde_json::to_string_pretty(result).unwrap_or_default()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{make_space, SbSpaceGuard};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn enable_runtime(space_root: &std::path::Path) {
        crate::config::update_config_value(&space_root.join(".sb"), "runtime", "available", true)
            .unwrap();
    }

    #[tokio::test]
    async fn returns_error_when_server_returns_lua_error_in_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"error":"undefined variable"}"#),
            )
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(
            None,
            Some("return foo"),
            None,
            &OutputFormat::Json,
            true,
            false,
        )
        .await
        .unwrap_err();

        // A Lua error is the caller's fault, not the server's, so it surfaces as
        // a usage error (exit 2) carrying the message the runtime reported.
        match err {
            SbError::Usage(msg) => assert!(
                msg.contains("undefined variable"),
                "expected lua error in message, got: {msg}"
            ),
            other => panic!("expected Usage, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_error_when_body_is_invalid_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(
            None,
            Some("return 1"),
            None,
            &OutputFormat::Json,
            true,
            false,
        )
        .await
        .unwrap_err();

        match err {
            SbError::HttpStatus { body, .. } => {
                assert!(body.contains("invalid JSON"), "got body: {body}")
            }
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }

    // --- read_script_source (pure logic) ---

    #[test]
    fn read_script_source_missing_file_names_path() {
        let err = read_script_source("/nonexistent/path/to/script.lua").unwrap_err();
        match err {
            SbError::Filesystem { path, .. } => {
                assert_eq!(path, "/nonexistent/path/to/script.lua")
            }
            other => panic!("expected Filesystem, got: {other:?}"),
        }
    }

    #[test]
    fn read_script_source_reads_existing_file_contents() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "return 1 + 1").unwrap();
        let body = read_script_source(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(body, "return 1 + 1");
    }
}
