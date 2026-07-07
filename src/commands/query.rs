use crate::cli::OutputFormat;
use crate::commands::server::{build_client, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

pub async fn execute(
    cli_token: Option<&str>,
    query: &str,
    fields: &[String],
    format: &OutputFormat,
    quiet: bool,
    _color: bool,
) -> SbResult<()> {
    let space_root = crate::commands::page::find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;

    // Check runtime availability
    if !config.runtime_available.value {
        return Err(runtime_unavailable_error());
    }

    let client = build_client(cli_token)?;

    // Wrap query as Lua script using query[[...]] syntax
    let lua_script = format!("return query[[{}]]", query);
    let resp = client
        .post_text("/.runtime/lua_script", &lua_script)
        .await?;
    let status = resp.status();

    // Handle 503: Runtime API went down between detection and use
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        return Err(runtime_unavailable_error());
    }

    let body = resp.text().await.map_err(|e| SbError::HttpStatus {
        status: status.as_u16(),
        url: format!("{}/.runtime/lua_script", client.base_url()),
        body: format!("failed to read response: {e}"),
    })?;

    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url: format!("{}/.runtime/lua_script", client.base_url()),
            body: format!("invalid JSON response: {e}"),
        })?;

    if let Some(error) = parsed.get("error").and_then(|e| e.as_str()) {
        return Err(SbError::HttpStatus {
            status: status.as_u16(),
            url: format!("{}/.runtime/lua_script", client.base_url()),
            body: format!("Query error: {error}"),
        });
    }

    let result = parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match format {
        OutputFormat::Json => {
            // Raw JSON output, trimmed to --fields when requested.
            let out = crate::output::filter_json_fields(&result, fields);
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        }
        OutputFormat::Human => {
            // Table format for arrays of objects, JSON fallback otherwise
            if let Some(arr) = result.as_array() {
                if arr.is_empty() {
                    if !quiet {
                        eprintln!("No results.");
                    }
                } else {
                    render_table(arr);
                }
            } else {
                // Non-array: fall back to JSON
                if !quiet {
                    eprintln!("note: result is not an array; displaying as JSON");
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
        }
    }

    Ok(())
}

/// Render a JSON array of objects as an ASCII table.
/// Extracts column names from the keys of the first object.
/// Uses simple format!() padding -- no external table crate needed.
fn render_table(rows: &[serde_json::Value]) {
    // Collect column names from first row
    let columns: Vec<String> = if let Some(obj) = rows.first().and_then(|r| r.as_object()) {
        obj.keys().cloned().collect()
    } else {
        // Not objects -- just dump each value on a line
        for row in rows {
            println!("{}", value_to_string(row));
        }
        return;
    };

    // Calculate column widths
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        if let Some(obj) = row.as_object() {
            for (i, col) in columns.iter().enumerate() {
                let val = obj.get(col).map(value_to_string).unwrap_or_default();
                widths[i] = widths[i].max(val.len());
            }
        }
    }

    // Print header
    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
        .collect();
    println!("{}", header.join(" | "));

    // Print separator
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep.join("-+-"));

    // Print rows
    for row in rows {
        if let Some(obj) = row.as_object() {
            let cells: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let val = obj.get(col).map(value_to_string).unwrap_or_default();
                    format!("{:<width$}", val, width = widths[i])
                })
                .collect();
            println!("{}", cells.join(" | "));
        }
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "".to_string(),
        other => other.to_string(),
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

    #[test]
    fn value_to_string_unwraps_quoted_strings() {
        // String values must render without their JSON quotes — otherwise table cells
        // would print '"hello"' instead of 'hello'.
        assert_eq!(value_to_string(&serde_json::json!("hello")), "hello");
    }

    #[test]
    fn value_to_string_null_renders_blank() {
        assert_eq!(value_to_string(&serde_json::Value::Null), "");
    }

    #[test]
    fn value_to_string_preserves_number_form() {
        assert_eq!(value_to_string(&serde_json::json!(42)), "42");
        assert_eq!(value_to_string(&serde_json::json!(true)), "true");
    }

    #[tokio::test]
    async fn errors_when_runtime_disabled() {
        let tmp = make_space(Some("http://127.0.0.1:1"));
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(
            None,
            "from index.tag 'page'",
            &[],
            &OutputFormat::Json,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("Runtime API not available"));
    }

    #[tokio::test]
    async fn returns_query_error_when_server_reports_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"error":"bad query syntax"}"#),
            )
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, "garbage", &[], &OutputFormat::Json, true, false)
            .await
            .unwrap_err();
        match err {
            SbError::HttpStatus { body, .. } => assert!(body.contains("bad query syntax")),
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn succeeds_on_array_of_objects_result_human_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"result":[{"name":"Foo","count":3},{"name":"Bar","count":5}]}"#,
            ))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(None, "from a", &[], &OutputFormat::Human, true, false).await;
        assert!(res.is_ok(), "{res:?}");
    }

    #[tokio::test]
    async fn succeeds_on_empty_array_result_human_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":[]}"#))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(None, "from b", &[], &OutputFormat::Human, true, false).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn succeeds_on_non_array_result_falls_back_to_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":42}"#))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(None, "from c", &[], &OutputFormat::Human, true, false).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn returns_error_on_invalid_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, "from d", &[], &OutputFormat::Json, true, false)
            .await
            .unwrap_err();
        match err {
            SbError::HttpStatus { body, .. } => assert!(body.contains("invalid JSON")),
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }
}
