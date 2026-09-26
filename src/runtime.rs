use crate::client::SbClient;
use crate::commands::server::runtime_unavailable_error;
use crate::config;
use crate::error::{SbError, SbResult};
use std::path::Path;
use tracing::debug;

/// POST a Lua body to a Runtime API endpoint and return the unwrapped `result`.
///
/// Every Runtime API caller needs the same six steps (post, check 503, read the
/// body, parse it, check the `error` key, take `result`), so they live here once
/// rather than being copied into each command.
///
/// The Runtime API attaches a stable `code` to execution failures, and this is
/// the only place that reads it. The distinction matters because the three
/// causes need different responses from the user:
///
/// * `script_error` — the Lua you sent threw. Your input is wrong, so this is a
///   usage error (exit 2), not a server fault. `sb lua 'return 1+1'` answering
///   HTTP 500 has confused every consumer of this CLI at least once.
/// * `bridge_unavailable` — the headless browser is not up. The server is at
///   fault and there is nothing to fix in the request.
/// * `timeout` — the server gave up waiting, which `--timeout` can raise.
pub async fn eval(client: &SbClient, endpoint: &str, body: &str) -> SbResult<serde_json::Value> {
    let url = format!("{}{}", client.base_url(), endpoint);
    // Deliberately not the retrying POST: see `post_text_once`. Retrying a Lua
    // error is pointless and destroys the message that explains it.
    let resp = client.post_text_once(endpoint, body).await?;
    let status = resp.status();

    let text = resp.text().await.map_err(|e| SbError::HttpStatus {
        status: status.as_u16(),
        url: url.clone(),
        body: format!("failed to read response: {e}"),
    })?;

    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        // A non-JSON body on a failing status is a bare error (an empty 503, a
        // proxy's HTML 502). Classify it by status rather than blaming the
        // JSON parser for a failure it did not cause.
        Err(_) if !status.is_success() => {
            let message = if text.trim().is_empty() {
                "no response body".to_string()
            } else {
                text.trim().to_string()
            };
            return Err(classify_runtime_error(
                None,
                status.as_u16(),
                &message,
                &url,
            ));
        }
        Err(e) => {
            return Err(SbError::HttpStatus {
                status: status.as_u16(),
                url,
                body: format!("invalid JSON response: {e}"),
            })
        }
    };

    if let Some(message) = parsed.get("error").and_then(|e| e.as_str()) {
        return Err(classify_runtime_error(
            parsed.get("code").and_then(|c| c.as_str()),
            status.as_u16(),
            message,
            &url,
        ));
    }

    Ok(parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

/// A query or script with a syntax error makes the server answer with a whole
/// HTML error page as its `error` message — tens of kilobytes of markup that
/// buries the one fact the caller needs. Collapse it to that fact.
fn collapse_html_error(message: &str) -> Option<String> {
    let trimmed = message.trim_start();
    if !(trimmed.starts_with("<!doctype") || trimmed.starts_with("<!DOCTYPE")) {
        return None;
    }
    let title = trimmed
        .split_once("<title>")
        .and_then(|(_, rest)| rest.split_once("</title>"))
        .map(|(t, _)| t.trim().to_string())
        .unwrap_or_else(|| "HTML error page".to_string());
    Some(format!(
        "the runtime returned an HTML error page ({title}) instead of a Lua message. \
         That is what a syntax error in the query or script looks like: check quoting, \
         and remember comparison is `==`, not `=`. `sb query --help` has worked examples."
    ))
}

/// Map a Runtime API error envelope onto an `SbError`.
///
/// Split out from `eval` so the mapping is testable without a server: it is
/// pure, and it is the part most likely to be wrong.
fn classify_runtime_error(code: Option<&str>, status: u16, message: &str, url: &str) -> SbError {
    let collapsed = collapse_html_error(message);
    let message: &str = collapsed.as_deref().unwrap_or(message);
    // The `code` field is authoritative when present. Status is the fallback for
    // servers that answer an error envelope without one.
    match code {
        Some("script_error") => return SbError::Usage(format!("Lua error: {message}")),
        Some("bridge_unavailable") => return runtime_unavailable_error(),
        Some("timeout") => {
            return SbError::RuntimeTimeout {
                seconds: crate::client::timeout_secs(),
            }
        }
        _ => {}
    }

    match status {
        503 => runtime_unavailable_error(),
        504 => SbError::RuntimeTimeout {
            seconds: crate::client::timeout_secs(),
        },
        // 500 is what the Runtime API returns when the Lua threw; 400 is an
        // empty request body. Both mean the request was wrong, not the server,
        // so they are usage errors the caller can act on. A 200 carrying an
        // error envelope is the same story from an older server.
        200 | 400 | 500 => SbError::Usage(format!("Lua error: {message}")),
        // Anything else really is a server/transport fault the user cannot fix.
        _ => SbError::HttpStatus {
            status,
            url: url.to_string(),
            body: format!("Lua error: {message}"),
        },
    }
}

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

    /// A syntax error makes the server answer with a full HTML error page as the
    /// error message. Printing 70KB of markup hides the one useful fact.
    #[test]
    fn html_error_page_collapses_to_its_title_and_a_hint() {
        let page = "<!doctype html><html lang=\"en\"><title>500 | Internal Server Error</title><body>...</body></html>";
        let msg = collapse_html_error(page).expect("HTML page should collapse");
        assert!(msg.contains("500 | Internal Server Error"), "{msg}");
        assert!(
            msg.contains("`==`"),
            "hint should name the usual cause: {msg}"
        );
        assert!(
            msg.len() < 400,
            "collapsed message should be short: {}",
            msg.len()
        );
    }

    #[test]
    fn a_real_lua_message_is_left_alone() {
        assert_eq!(collapse_html_error("attempt to index a nil value"), None);
    }

    #[test]
    fn classify_uses_the_collapsed_message() {
        let err = classify_runtime_error(
            Some("script_error"),
            500,
            "<!DOCTYPE html><title>500 | Internal Server Error</title>",
            "http://x/.runtime/lua_script",
        );
        let text = format!("{err}");
        assert!(!text.contains("<!DOCTYPE"), "raw HTML leaked: {text}");
        assert!(text.contains("HTML error page"), "{text}");
    }

    /// The `code` field wins over the status, so a server that answers a script
    /// error with an unexpected status still classifies correctly.
    #[test]
    fn script_error_code_is_a_usage_error_whatever_the_status() {
        for status in [200, 400, 500, 502] {
            let err = classify_runtime_error(Some("script_error"), status, "boom", "u");
            match err {
                SbError::Usage(msg) => assert!(msg.contains("boom"), "status {status}: {msg}"),
                other => panic!("status {status}: expected Usage, got {other:?}"),
            }
            assert_eq!(
                classify_runtime_error(Some("script_error"), status, "boom", "u").exit_code(),
                2
            );
        }
    }

    #[test]
    fn bridge_unavailable_code_reports_the_runtime_as_down() {
        let err = classify_runtime_error(Some("bridge_unavailable"), 503, "no browser", "u");
        assert!(
            format!("{err}").contains("Runtime API not available"),
            "got: {err}"
        );
    }

    #[test]
    fn timeout_code_names_the_timeout_flag_in_its_hint() {
        let err = classify_runtime_error(Some("timeout"), 504, "too slow", "u");
        assert!(matches!(err, SbError::RuntimeTimeout { .. }));
        assert!(
            err.hint().unwrap_or_default().contains("--timeout"),
            "hint should point at the flag that fixes it"
        );
    }

    /// Older servers answer an error envelope without a `code`, so status is the
    /// fallback. 500 means the Lua threw; it is not a server fault to report.
    #[test]
    fn status_classifies_when_no_code_is_present() {
        assert!(matches!(
            classify_runtime_error(None, 500, "threw", "u"),
            SbError::Usage(_)
        ));
        assert!(matches!(
            classify_runtime_error(None, 400, "empty body", "u"),
            SbError::Usage(_)
        ));
        assert!(matches!(
            classify_runtime_error(None, 200, "legacy", "u"),
            SbError::Usage(_)
        ));
        assert!(matches!(
            classify_runtime_error(None, 504, "slow", "u"),
            SbError::RuntimeTimeout { .. }
        ));
        assert!(
            format!("{}", classify_runtime_error(None, 503, "down", "u"))
                .contains("Runtime API not available")
        );
    }

    /// A status outside the runtime's own vocabulary really is a server or proxy
    /// fault, and must not be misreported as the caller's mistake.
    #[test]
    fn unknown_status_stays_a_server_fault() {
        let err = classify_runtime_error(None, 502, "bad gateway", "http://x/.runtime/lua");
        match err {
            SbError::HttpStatus { status, .. } => assert_eq!(status, 502),
            other => panic!("expected HttpStatus, got {other:?}"),
        }
        assert_eq!(
            classify_runtime_error(None, 502, "bad gateway", "u").exit_code(),
            1
        );
    }

    #[tokio::test]
    async fn eval_unwraps_the_result_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":{"a":1}}"#))
            .mount(&server)
            .await;
        let client = SbClient::new(&server.uri(), "t").unwrap();

        let got = eval(&client, "/.runtime/lua", "1").await.unwrap();
        assert_eq!(got, serde_json::json!({"a": 1}));
    }

    /// A response with neither key is not an error; it is a Lua call that
    /// returned nothing.
    #[tokio::test]
    async fn eval_treats_a_missing_result_as_null() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let client = SbClient::new(&server.uri(), "t").unwrap();

        assert_eq!(
            eval(&client, "/.runtime/lua", "1").await.unwrap(),
            serde_json::Value::Null
        );
    }

    /// The runtime can go down between the config check and the call, and a bare
    /// 503 carries no body to classify from.
    #[tokio::test]
    async fn eval_maps_a_bodyless_503_to_runtime_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = SbClient::new(&server.uri(), "t").unwrap();

        let err = eval(&client, "/.runtime/lua", "1").await.unwrap_err();
        assert!(format!("{err}").contains("Runtime API not available"));
    }

    /// The regression this refactor exists to fix. A Lua error arrives as a 500
    /// carrying `script_error`, and the generic retry path used to swallow it:
    /// four requests, seven seconds of backoff, and "server returned 500" in
    /// place of the message naming the actual mistake.
    #[tokio::test]
    async fn eval_does_not_retry_a_lua_error_and_keeps_its_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(500).set_body_string(
                r#"{"error":"attempt to call a nil value","code":"script_error"}"#,
            ))
            .expect(1) // exactly one request: no retry storm
            .mount(&server)
            .await;
        let client = SbClient::new(&server.uri(), "t").unwrap();

        let err = eval(&client, "/.runtime/lua", "nope()").await.unwrap_err();
        match err {
            SbError::Usage(msg) => assert!(
                msg.contains("attempt to call a nil value"),
                "the runtime's own message must survive, got: {msg}"
            ),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn eval_reports_unparseable_bodies_as_a_server_fault() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>nope</html>"))
            .mount(&server)
            .await;
        let client = SbClient::new(&server.uri(), "t").unwrap();

        match eval(&client, "/.runtime/lua", "1").await.unwrap_err() {
            SbError::HttpStatus { body, .. } => assert!(body.contains("invalid JSON")),
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

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
