use crate::cli::OutputFormat;
use crate::commands::server::{build_client, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};
use console::Style;
use std::collections::BTreeMap;

/// Execute `sb describe <tag>` — introspect the observed schema of objects
/// indexed under `tag` (e.g. `task`, `page`, `template`).
///
/// SilverBullet's index does not expose a first-class schema endpoint, so
/// this command samples up to `limit` objects of that tag via the SLIQ
/// `query[[...]]` runtime path and reports the union of observed fields
/// with their inferred Lua types. The result is a best-effort introspection
/// rather than a contract, and the inferred types are biased toward what
/// the running space currently contains.
pub async fn execute(
    cli_token: Option<&str>,
    tag: &str,
    limit: usize,
    format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    let space_root = crate::commands::page::find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;
    if !config.runtime_available.value {
        return Err(runtime_unavailable_error());
    }

    let safe_tag = sanitize_tag(tag)?;
    let client = build_client(cli_token)?;
    let lua_script = build_describe_script(&safe_tag, limit);
    let resp = client
        .post_text("/.runtime/lua_script", &lua_script)
        .await?;
    let status = resp.status();
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
            body: format!("Lua error: {error}"),
        });
    }

    let result = parsed
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let summary = TagSummary::from_lua_result(&safe_tag, &result);

    render(&summary, format, color, quiet);
    Ok(())
}

/// Build the Lua probe script. Samples up to `limit` objects of the given
/// tag, then walks each object's fields and tallies the observed Lua types.
/// Returns `{tag, sampled, fields = { name -> { type -> count } }}`.
pub(crate) fn build_describe_script(tag: &str, limit: usize) -> String {
    format!(
        r#"local rows = query[[from index.tag "{tag}" limit {limit}]]
local fields = {{}}
for _, obj in ipairs(rows) do
  for k, v in pairs(obj) do
    if not fields[k] then fields[k] = {{}} end
    local t = type(v)
    fields[k][t] = (fields[k][t] or 0) + 1
  end
end
return {{ tag = "{tag}", sampled = #rows, fields = fields }}"#,
    )
}

/// Reject tag names containing characters that could escape the embedded Lua
/// string literal. SilverBullet tags are tokens (letters, digits, `_`, `-`,
/// `/`); anything outside that set is a usage error.
pub(crate) fn sanitize_tag(tag: &str) -> SbResult<String> {
    if tag.is_empty() {
        return Err(SbError::Usage("tag name must not be empty".into()));
    }
    let ok = tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/');
    if !ok {
        return Err(SbError::Usage(format!(
            "tag '{tag}' contains characters not allowed in tag names (use letters, digits, '_', '-', '/')"
        )));
    }
    Ok(tag.to_string())
}

/// Parsed summary of a tag introspection result.
#[derive(Debug, Clone)]
pub(crate) struct TagSummary {
    pub tag: String,
    pub sampled: u64,
    /// field name -> sorted list of (lua_type, count)
    pub fields: BTreeMap<String, Vec<(String, u64)>>,
}

impl TagSummary {
    pub(crate) fn from_lua_result(tag: &str, value: &serde_json::Value) -> Self {
        let sampled = value.get("sampled").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut fields: BTreeMap<String, Vec<(String, u64)>> = BTreeMap::new();
        if let Some(obj) = value.get("fields").and_then(|f| f.as_object()) {
            for (name, types) in obj {
                let mut by_type: Vec<(String, u64)> = Vec::new();
                if let Some(t_obj) = types.as_object() {
                    for (t, count) in t_obj {
                        by_type.push((t.clone(), count.as_u64().unwrap_or(0)));
                    }
                }
                by_type.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                fields.insert(name.clone(), by_type);
            }
        }
        Self {
            tag: tag.to_string(),
            sampled,
            fields,
        }
    }
}

fn render(summary: &TagSummary, format: &OutputFormat, color: bool, quiet: bool) {
    match format {
        OutputFormat::Json => {
            let fields_json: serde_json::Map<String, serde_json::Value> = summary
                .fields
                .iter()
                .map(|(name, types)| {
                    let arr: Vec<serde_json::Value> = types
                        .iter()
                        .map(|(t, c)| serde_json::json!({ "type": t, "count": c }))
                        .collect();
                    (name.clone(), serde_json::Value::Array(arr))
                })
                .collect();
            let payload = serde_json::json!({
                "tag": summary.tag,
                "sampled": summary.sampled,
                "fields": fields_json,
            });
            println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        }
        OutputFormat::Human => {
            if summary.sampled == 0 {
                if !quiet {
                    eprintln!(
                        "No objects with tag '{}' found in the index. Nothing to describe.",
                        summary.tag
                    );
                }
                return;
            }

            let bold = if color {
                Style::new().bold()
            } else {
                Style::new()
            };
            let dim = if color {
                Style::new().dim()
            } else {
                Style::new()
            };

            if !quiet {
                println!(
                    "{} {} {}",
                    bold.apply_to(format!("Tag: {}", summary.tag)),
                    dim.apply_to("sampled"),
                    summary.sampled,
                );
            }

            // Determine column widths
            let name_w = summary
                .fields
                .keys()
                .map(|n| n.len())
                .max()
                .unwrap_or(0)
                .max("field".len());
            let type_w = summary
                .fields
                .values()
                .flat_map(|v| v.iter().map(|(t, _)| t.len()))
                .max()
                .unwrap_or(0)
                .max("type(s)".len());

            // Header
            println!(
                "{:<name_w$}  {:<type_w$}  coverage",
                "field",
                "type(s)",
                name_w = name_w,
                type_w = type_w,
            );
            println!("{}  {}  --------", "-".repeat(name_w), "-".repeat(type_w),);

            for (name, types) in &summary.fields {
                let total: u64 = types.iter().map(|(_, c)| *c).sum();
                let pct = if summary.sampled > 0 {
                    100.0 * (total as f64) / (summary.sampled as f64)
                } else {
                    0.0
                };
                let types_str = types
                    .iter()
                    .map(|(t, c)| format!("{t}({c})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{:<name_w$}  {:<type_w$}  {:>5.1}%",
                    name,
                    types_str,
                    pct,
                    name_w = name_w,
                    type_w = type_w,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_tag_accepts_letters_digits_dash_underscore_slash() {
        assert!(sanitize_tag("task").is_ok());
        assert!(sanitize_tag("foo_bar").is_ok());
        assert!(sanitize_tag("foo-bar").is_ok());
        assert!(sanitize_tag("ns/sub").is_ok());
        assert!(sanitize_tag("v2").is_ok());
    }

    #[test]
    fn sanitize_tag_rejects_quotes_and_brackets() {
        assert!(sanitize_tag("foo\"bar").is_err());
        assert!(sanitize_tag("foo]bar").is_err());
        assert!(sanitize_tag("foo bar").is_err());
        assert!(sanitize_tag("").is_err());
    }

    #[test]
    fn build_describe_script_contains_tag_and_limit() {
        let script = build_describe_script("task", 25);
        assert!(script.contains(r#"tag "task""#));
        assert!(script.contains("limit 25"));
        assert!(script.contains("return"));
    }

    #[test]
    fn from_lua_result_parses_fields_and_sampled() {
        let value = serde_json::json!({
            "tag": "task",
            "sampled": 3,
            "fields": {
                "name": { "string": 3 },
                "done": { "boolean": 2, "nil": 1 },
            }
        });
        let summary = TagSummary::from_lua_result("task", &value);
        assert_eq!(summary.sampled, 3);
        assert_eq!(summary.fields.len(), 2);
        let done_types = &summary.fields["done"];
        // Sorted by count desc, so "boolean" (2) before "nil" (1)
        assert_eq!(done_types[0].0, "boolean");
        assert_eq!(done_types[0].1, 2);
        assert_eq!(done_types[1].0, "nil");
        assert_eq!(done_types[1].1, 1);
    }

    #[test]
    fn from_lua_result_handles_empty_fields() {
        let value = serde_json::json!({ "tag": "task", "sampled": 0, "fields": {} });
        let summary = TagSummary::from_lua_result("task", &value);
        assert_eq!(summary.sampled, 0);
        assert!(summary.fields.is_empty());
    }

    #[test]
    fn from_lua_result_field_types_are_sorted_by_count_desc_then_name() {
        // Tie-breaking matters for stable output: same count → alphabetical type name.
        let value = serde_json::json!({
            "sampled": 4,
            "fields": {
                "x": { "string": 2, "number": 2, "boolean": 1 }
            }
        });
        let summary = TagSummary::from_lua_result("t", &value);
        let types = &summary.fields["x"];
        assert_eq!(types[0], ("number".into(), 2));
        assert_eq!(types[1], ("string".into(), 2));
        assert_eq!(types[2], ("boolean".into(), 1));
    }

    #[test]
    fn from_lua_result_missing_keys_yield_zero_sampled_no_fields() {
        let summary = TagSummary::from_lua_result("t", &serde_json::Value::Null);
        assert_eq!(summary.sampled, 0);
        assert!(summary.fields.is_empty());
    }

    mod execute_tests {
        use super::super::*;
        use crate::test_util::{make_space, SbSpaceGuard};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn enable_runtime(space_root: &std::path::Path) {
            crate::config::update_config_value(
                &space_root.join(".sb"),
                "runtime",
                "available",
                true,
            )
            .unwrap();
        }

        #[tokio::test]
        async fn execute_errors_on_invalid_tag() {
            let tmp = make_space(Some("http://127.0.0.1:1"));
            enable_runtime(tmp.path());
            let _g = SbSpaceGuard::set(tmp.path());
            let err = execute(None, "bad tag", 10, &OutputFormat::Json, true, false)
                .await
                .unwrap_err();
            assert!(matches!(err, SbError::Usage(_)));
        }

        #[tokio::test]
        async fn execute_errors_when_runtime_disabled() {
            let tmp = make_space(Some("http://127.0.0.1:1"));
            let _g = SbSpaceGuard::set(tmp.path());
            let err = execute(None, "task", 10, &OutputFormat::Json, true, false)
                .await
                .unwrap_err();
            assert!(format!("{err}").contains("Runtime API not available"));
        }

        #[tokio::test]
        async fn execute_succeeds_with_valid_lua_response() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/.runtime/lua_script"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    r#"{"result":{"tag":"task","sampled":2,"fields":{"name":{"string":2}}}}"#,
                ))
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            enable_runtime(tmp.path());
            let _g = SbSpaceGuard::set(tmp.path());
            execute(None, "task", 100, &OutputFormat::Json, true, false)
                .await
                .expect("succeed");
        }

        #[tokio::test]
        async fn execute_zero_sampled_human_format_short_circuits() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/.runtime/lua_script"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(r#"{"result":{"tag":"missing","sampled":0,"fields":{}}}"#),
                )
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            enable_runtime(tmp.path());
            let _g = SbSpaceGuard::set(tmp.path());
            // Should succeed (early-return path), not error.
            execute(None, "missing", 100, &OutputFormat::Human, false, false)
                .await
                .unwrap();
        }
    }
}
