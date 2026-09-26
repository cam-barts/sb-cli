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
///
/// With no tag it lists every tag in the index instead. Knowing that
/// `index.tag "NAME"` is the only query source does not tell you which names
/// exist, so the two modes together are the discoverability path for `sb query`:
/// `sb describe` for the sources, `sb describe TAG` for the filterable fields.
pub async fn execute(
    cli_token: Option<&str>,
    tag: Option<&str>,
    limit: usize,
    fields: &[String],
    format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    let space_root = crate::commands::page::find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;
    if !config.runtime_available.value {
        return Err(runtime_unavailable_error());
    }

    // No tag: the caller does not yet know what is queryable, so list the tags
    // themselves. `sb describe` -> what exists; `sb describe TAG` -> what it has.
    let Some(tag) = tag else {
        let client = build_client(cli_token)?;
        let result =
            crate::runtime::eval(&client, "/.runtime/lua_script", LIST_TAGS_SCRIPT).await?;
        render_tag_list(
            &TagList::from_lua_result(&result),
            fields,
            format,
            color,
            quiet,
        );
        return Ok(());
    };

    let safe_tag = sanitize_tag(tag)?;
    let client = build_client(cli_token)?;
    let lua_script = build_describe_script(&safe_tag, limit);
    let result = crate::runtime::eval(&client, "/.runtime/lua_script", &lua_script).await?;

    let summary = TagSummary::from_lua_result(&safe_tag, &result);

    render(&summary, fields, format, color, quiet);
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

/// Probe for `sb describe` with no tag: tally every object in the `tag` index
/// by tag name, recording how many objects carry it and which object types it
/// is attached to. Returns `[{name, count, parents}]` sorted by count.
///
/// Two SilverBullet sharp edges are load-bearing here. Leaf values coming back
/// over the runtime bridge are wrapped, so they must be `tostring`-ed before
/// they can be used as table keys. And `select` de-duplicates its projection,
/// so the counts have to be tallied from full rows, not from `select name`.
pub(crate) const LIST_TAGS_SCRIPT: &str = r#"local tally = {}
for _, r in ipairs(query[[from index.tag "tag"]]) do
  local name = tostring(r.name)
  local t = tally[name]
  if not t then t = { count = 0, parents = {} }; tally[name] = t end
  t.count = t.count + 1
  t.parents[tostring(r.parent)] = true
end
local out = {}
for name, t in pairs(tally) do
  local parents = {}
  for p in pairs(t.parents) do parents[#parents + 1] = p end
  table.sort(parents)
  out[#out + 1] = { name = name, count = t.count, parents = table.concat(parents, ", ") }
end
table.sort(out, function(a, b)
  if a.count == b.count then return a.name < b.name end
  return a.count > b.count
end)
return out"#;

/// One row of `sb describe` with no tag.
#[derive(Debug, Clone)]
pub(crate) struct TagList(pub Vec<(String, u64, String)>);

impl TagList {
    pub(crate) fn from_lua_result(value: &serde_json::Value) -> Self {
        let rows = value
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        (
                            r.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            r.get("count").and_then(|v| v.as_u64()).unwrap_or(0),
                            r.get("parents")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self(rows)
    }
}

/// Widest the tag column gets in the human table. JSON output is never elided.
const TAG_NAME_COLUMN_MAX: usize = 48;

/// Shorten `s` to `width` characters (not bytes — tag names are arbitrary text),
/// marking the cut with a trailing `…`.
fn elide(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
}

fn render_tag_list(
    list: &TagList,
    fields: &[String],
    format: &OutputFormat,
    color: bool,
    quiet: bool,
) {
    match format {
        OutputFormat::Json => {
            let payload: Vec<serde_json::Value> = list
                .0
                .iter()
                .map(|(name, count, parents)| {
                    serde_json::json!({ "name": name, "count": count, "parents": parents })
                })
                .collect();
            let payload =
                crate::output::filter_json_fields(&serde_json::Value::Array(payload), fields);
            println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        }
        OutputFormat::Human => {
            if list.0.is_empty() {
                if !quiet {
                    eprintln!("No tags found in the index.");
                }
                return;
            }
            let dim = if color {
                Style::new().dim()
            } else {
                Style::new()
            };
            // Spaces accumulate junk tags (a stray `#` on a long line becomes a
            // 200-character "tag"), and one of those would push every other
            // column off the terminal. Cap the column and elide over-long names.
            let name_w = list
                .0
                .iter()
                .map(|(n, _, _)| n.chars().count())
                .max()
                .unwrap_or(0)
                .clamp("tag".len(), TAG_NAME_COLUMN_MAX);
            let count_w = list
                .0
                .iter()
                .map(|(_, c, _)| c.to_string().len())
                .max()
                .unwrap_or(0)
                .max("objects".len());
            println!("{:<name_w$}  {:>count_w$}  attached to", "tag", "objects");
            println!(
                "{}  {}  -----------",
                "-".repeat(name_w),
                "-".repeat(count_w)
            );
            for (name, count, parents) in &list.0 {
                let name = elide(name, name_w);
                println!("{name:<name_w$}  {count:>count_w$}  {parents}");
            }
            if !quiet {
                eprintln!(
                    "{}",
                    dim.apply_to(format!(
                        "{} tags. `sb describe TAG` shows the attributes objects of that tag carry.",
                        list.0.len()
                    ))
                );
            }
        }
    }
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

fn render(
    summary: &TagSummary,
    fields: &[String],
    format: &OutputFormat,
    color: bool,
    quiet: bool,
) {
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
            let payload = crate::output::filter_json_fields(&payload, fields);
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
    fn tag_list_parses_rows_and_keeps_probe_order() {
        let value = serde_json::json!([
            { "name": "zotero", "count": 606, "parents": "page" },
            { "name": "highlight", "count": 431, "parents": "data" },
            // A row the probe could not fully resolve must not panic or vanish.
            { "name": "odd" },
        ]);
        let list = TagList::from_lua_result(&value);
        assert_eq!(list.0.len(), 3);
        assert_eq!(list.0[0], ("zotero".into(), 606, "page".into()));
        assert_eq!(list.0[2], ("odd".into(), 0, String::new()));
    }

    #[test]
    fn elide_only_cuts_what_is_too_long_and_counts_chars() {
        assert_eq!(elide("task", 10), "task");
        assert_eq!(elide("abcdefghij", 10), "abcdefghij");
        assert_eq!(elide("abcdefghijk", 10), "abcdefghi…");
        // A multi-byte name must be cut on a char boundary, not mid-codepoint.
        assert_eq!(elide("émigré-café", 5), "émig…");
        assert_eq!(elide("émigré-café", 5).chars().count(), 5);
    }

    #[test]
    fn tag_list_tolerates_a_non_array_result() {
        assert!(TagList::from_lua_result(&serde_json::json!(null))
            .0
            .is_empty());
    }

    /// The probe has to tally from full rows: `select` de-duplicates, which
    /// would turn every count into 1.
    #[test]
    fn list_tags_script_does_not_project_with_select() {
        assert!(LIST_TAGS_SCRIPT.contains(r#"from index.tag "tag""#));
        assert!(!LIST_TAGS_SCRIPT.contains("select"));
        // Bridge leaf values are wrapped; unconverted they cannot be table keys.
        assert!(LIST_TAGS_SCRIPT.contains("tostring(r.name)"));
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
            let err = execute(
                None,
                Some("bad tag"),
                10,
                &[],
                &OutputFormat::Json,
                true,
                false,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, SbError::Usage(_)));
        }

        #[tokio::test]
        async fn execute_errors_when_runtime_disabled() {
            let tmp = make_space(Some("http://127.0.0.1:1"));
            let _g = SbSpaceGuard::set(tmp.path());
            let err = execute(
                None,
                Some("task"),
                10,
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
            execute(
                None,
                Some("task"),
                100,
                &[],
                &OutputFormat::Json,
                true,
                false,
            )
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
            execute(
                None,
                Some("missing"),
                100,
                &[],
                &OutputFormat::Human,
                false,
                false,
            )
            .await
            .unwrap();
        }
    }
}
