use crate::cli::OutputFormat;
use crate::commands::server::{build_client, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

pub async fn execute(
    cli_token: Option<&str>,
    expression: &str,
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
    async fn errors_when_runtime_disabled_in_config() {
        // No need for a server here — the runtime-availability check happens before any HTTP.
        let tmp = make_space(Some("http://127.0.0.1:1"));
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, "return 1", &OutputFormat::Json, true, false)
            .await
            .unwrap_err();

        let msg = format!("{err}");
        assert!(
            msg.contains("Runtime API not available"),
            "expected runtime-unavailable error, got: {msg}"
        );
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

        let err = execute(None, "return foo", &OutputFormat::Json, true, false)
            .await
            .unwrap_err();

        // The lua-error path packs the upstream error message into the HttpStatus body.
        match err {
            SbError::HttpStatus { body, .. } => assert!(
                body.contains("undefined variable"),
                "expected lua error in body, got: {body}"
            ),
            other => panic!("expected HttpStatus, got: {other:?}"),
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

        let err = execute(None, "return 1", &OutputFormat::Json, true, false)
            .await
            .unwrap_err();

        match err {
            SbError::HttpStatus { body, .. } => {
                assert!(body.contains("invalid JSON"), "got body: {body}")
            }
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn succeeds_on_scalar_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":42}"#))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(None, "return 42", &OutputFormat::Human, true, false).await;
        assert!(res.is_ok(), "expected success, got {res:?}");
    }

    #[tokio::test]
    async fn succeeds_on_complex_result_json_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"result":{"a":1,"b":[1,2,3]}}"#),
            )
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(None, "return {}", &OutputFormat::Json, true, false).await;
        assert!(res.is_ok());
    }
}
