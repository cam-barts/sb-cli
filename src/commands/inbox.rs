//! `sb inbox` — open `@mention`s addressed to an identity (the Mention Inbox).
//!
//! `@name` mentions are relation objects with `kind == "at-mention"`, where
//! `to` holds the `@name` identifier *with* the leading `@`:
//!
//! ```json
//! {"kind":"at-mention","tag":"relation","to":"@cam", ...}
//! ```
//!
//! This is deliberately distinct from `sb links`' `kind == "mention"` (a
//! plain `[[wiki link]]`): a mention *addresses* someone and belongs in their
//! inbox, where a signature (`-- @name`, see `page::append_signature`)
//! merely credits authorship and never queues anything here.

use console::Style;

use crate::cli::OutputFormat;
use crate::commands::links::{lua_string_literal, render};
use crate::commands::server::{build_client, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

pub async fn execute(
    cli_token: Option<&str>,
    to: Option<&str>,
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

    let identity = resolve_identity(to, config.identity.value.as_deref())?;

    let client = build_client(cli_token)?;
    let lua_script = build_inbox_script(&identity, limit);
    let result = crate::runtime::eval(&client, "/.runtime/lua_script", &lua_script).await?;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    // `--fields` and JSON keep the generic relation rendering; the default
    // human view is a readable, page-grouped list.
    if matches!(format, OutputFormat::Human) && fields.is_empty() {
        render_human(&result, &identity, quiet, color, &mut handle).map_err(|e| {
            SbError::Internal {
                message: format!("failed to write output: {e}"),
            }
        })?;
        return Ok(());
    }
    render(&result, fields, format, quiet, "No mentions.", &mut handle)?;
    Ok(())
}

/// Human view: a header, then mentions grouped under their page, each with
/// its snippet cleaned of list markers and `[key: value]` attributes.
/// Styling is applied only when `color` is true (`--no-color`, `NO_COLOR`,
/// or a non-TTY stdout turn it off), so the plain output stays diffable.
fn render_human(
    result: &serde_json::Value,
    identity: &str,
    quiet: bool,
    color: bool,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let style = |s: Style| {
        if color {
            s.force_styling(true)
        } else {
            Style::new()
        }
    };
    let head = style(Style::new().bold());
    let page_style = style(Style::new().cyan().bold());
    let dim = style(Style::new().dim());
    let mention = style(Style::new().yellow().bold());

    let rows = result.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        if !quiet {
            writeln!(out, "No open mentions for {identity}.")?;
        }
        return Ok(());
    }

    // Group by page, preserving the order pages first appear in.
    let mut pages: Vec<(String, Vec<&serde_json::Value>)> = Vec::new();
    for row in &rows {
        let page = row
            .get("page")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        match pages.iter_mut().find(|(p, _)| *p == page) {
            Some((_, items)) => items.push(row),
            None => pages.push((page, vec![row])),
        }
    }

    let n = rows.len();
    let noun = if n == 1 { "mention" } else { "mentions" };
    writeln!(
        out,
        "{} {}",
        head.apply_to(identity),
        dim.apply_to(format!("· {n} open {noun}"))
    )?;
    for (page, items) in pages {
        writeln!(out)?;
        writeln!(out, "{}", page_style.apply_to(page))?;
        for row in items {
            let is_task = row.get("fromTag").and_then(|v| v.as_str()) == Some("task");
            let marker = if is_task { "☐" } else { "•" };
            let snippet = row.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            let words: Vec<String> = clean_snippet(snippet)
                .split(' ')
                .map(|w| {
                    if w.starts_with('@') && w.len() > 1 {
                        mention.apply_to(w).to_string()
                    } else if w.starts_with('#') && w.len() > 1 {
                        dim.apply_to(w).to_string()
                    } else {
                        w.to_string()
                    }
                })
                .collect();
            writeln!(out, "  {} {}", dim.apply_to(marker), words.join(" "))?;
        }
    }
    Ok(())
}

/// Strip a snippet down to its prose: leading `>` quote markers, the
/// list/checkbox marker, and inline `[key: value]` attributes go; wiki links
/// (`[[...]]`), `#tags`, and `@mentions` stay. Whitespace is collapsed to
/// single spaces. Shared with `sb links`.
pub(crate) fn clean_snippet(snippet: &str) -> String {
    let mut s = snippet.trim_start();
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start();
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(marker) {
            s = rest.trim_start();
            break;
        }
    }
    if s.len() >= 3 && s.starts_with('[') && s.as_bytes()[2] == b']' {
        s = s[3..].trim_start();
    }

    let chars: Vec<char> = s.chars().collect();
    let mut kept = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let is_single_open =
            chars[i] == '[' && chars.get(i + 1) != Some(&'[') && (i == 0 || chars[i - 1] != '[');
        if is_single_open {
            if let Some(len) = chars[i + 1..].iter().position(|&c| c == ']') {
                let inner: String = chars[i + 1..i + 1 + len].iter().collect();
                if is_attribute(&inner) {
                    i += len + 2;
                    continue;
                }
            }
        }
        kept.push(chars[i]);
        i += 1;
    }
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `key: value` / `key:value`, where key is an identifier-ish word.
fn is_attribute(inner: &str) -> bool {
    match inner.split_once(':') {
        Some((key, _)) => {
            let key = key.trim();
            !key.is_empty()
                && key
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }
        None => false,
    }
}

/// Resolve which `@name` to query: `--to` wins when given, otherwise the
/// space's configured `identity`. Never asks the server (`identity.own()`
/// returns `null` on servers without accounts, which is the target
/// deployment) — a missing identity on both sides is a usage error that
/// names exactly how to fix it.
fn resolve_identity(to: Option<&str>, configured: Option<&str>) -> SbResult<String> {
    match to.or(configured) {
        Some(name) => Ok(normalize_identity(name)),
        None => Err(SbError::Usage(
            "no identity to check: pass --to <name>, or set `identity = \"@name\"` in \
             .sb/config.toml (or the SB_IDENTITY env var)"
                .to_string(),
        )),
    }
}

/// Normalize an identity name so `cam` and `@cam` refer to the same person:
/// strip a leading `@` if present, then re-add exactly one. Used for both
/// `sb inbox --to` and `--sign` names, so a name is never double-prefixed
/// regardless of which flag supplied it.
pub(crate) fn normalize_identity(name: &str) -> String {
    let trimmed = name.trim();
    let bare = trimmed.strip_prefix('@').unwrap_or(trimmed);
    format!("@{bare}")
}

/// Build the Runtime API script for `sb inbox`. See
/// `links::build_links_script` for why the query body has to be a plain
/// `[[...]]` bracket referencing a Lua local, rather than interpolating the
/// identity string into the query text directly.
///
/// "Open" matches SilverBullet's own Mention Inbox (`Library/Std/Editor/
/// Mention Inbox`): a mention that sits in a *done* task is hidden. That
/// filter needs the task object, so it runs in Lua after the query, and
/// `limit` caps the filtered rows rather than the raw relations.
fn build_inbox_script(identity: &str, limit: usize) -> String {
    let literal = lua_string_literal(identity);
    format!(
        "local target = {literal}
local rows = query[[from index.tag \"relation\" where kind == \"at-mention\" and to == target]]
local out = {{}}
for _, m in ipairs(rows) do
  local done = false
  if m.fromTag == \"task\" then
    local task = index.getObjectByRef(m.page, \"task\", m.from)
    done = task ~= nil and task.done == true
  end
  if not done then
    table.insert(out, m)
    if #out >= {limit} then break end
  end
end
return out"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SbError;
    use crate::test_util::{make_space, SbSpaceGuard};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn enable_runtime(space_root: &std::path::Path) {
        crate::config::update_config_value(&space_root.join(".sb"), "runtime", "available", true)
            .unwrap();
    }

    // --- normalize_identity ---

    #[test]
    fn normalize_identity_adds_leading_at_when_missing() {
        assert_eq!(normalize_identity("cam"), "@cam");
    }

    #[test]
    fn normalize_identity_does_not_double_prefix() {
        assert_eq!(normalize_identity("@cam"), "@cam");
    }

    #[test]
    fn normalize_identity_trims_whitespace() {
        assert_eq!(normalize_identity("  cam  "), "@cam");
    }

    // --- resolve_identity ---

    #[test]
    fn resolve_identity_to_flag_and_configured_produce_the_same_query_identity() {
        // `--to cam` and `--to @cam` (and falling back to a configured
        // `@cam`) must all resolve to the identical query target.
        assert_eq!(resolve_identity(Some("cam"), None).unwrap(), "@cam");
        assert_eq!(resolve_identity(Some("@cam"), None).unwrap(), "@cam");
        assert_eq!(resolve_identity(None, Some("@cam")).unwrap(), "@cam");
        assert_eq!(resolve_identity(None, Some("cam")).unwrap(), "@cam");
    }

    #[test]
    fn resolve_identity_flag_wins_over_configured() {
        assert_eq!(resolve_identity(Some("ada"), Some("cam")).unwrap(), "@ada");
    }

    #[test]
    fn resolve_identity_errors_when_neither_is_set() {
        let err = resolve_identity(None, None).unwrap_err();
        match err {
            SbError::Usage(msg) => {
                assert!(msg.contains("--to"));
                assert!(msg.contains("identity"));
            }
            other => panic!("expected Usage, got: {other:?}"),
        }
    }

    // --- build_inbox_script ---

    #[test]
    fn build_inbox_script_queries_at_mention_kind_and_to_field() {
        let script = build_inbox_script("@cam", 200);
        assert!(script.contains(r#"kind == "at-mention""#));
        assert!(script.contains("to == target"));
        assert!(script.contains("if #out >= 200 then break end"));
        assert_eq!(script.lines().next().unwrap(), r#"local target = "@cam""#);
    }

    #[test]
    fn build_inbox_script_query_line_is_a_fixed_constant_regardless_of_identity() {
        // Same injection-proof property as build_links_script: a hostile
        // identity can never change the query text, only the escaped Lua
        // literal that binds `target`.
        let hostile = "@cam]==]\" .. (os.execute or print)(\"pwn\") --";
        let script = build_inbox_script(hostile, 200);
        let query_line = script.lines().nth(1).unwrap();
        assert_eq!(
            query_line,
            r#"local rows = query[[from index.tag "relation" where kind == "at-mention" and to == target]]"#
        );
    }

    #[test]
    fn build_inbox_script_uses_plain_double_bracket_not_a_leveled_one() {
        let script = build_inbox_script("@cam", 200);
        assert!(script.contains("query[["));
        assert!(!script.contains("[==["));
        assert!(!script.contains("]==]"));
    }

    #[test]
    fn build_inbox_script_hides_mentions_in_done_tasks_and_limits_after_filtering() {
        let script = build_inbox_script("@cam", 5);
        assert!(script.contains(r#"index.getObjectByRef(m.page, "task", m.from)"#));
        assert!(script.contains("task.done == true"));
        assert!(script.contains("if #out >= 5 then break end"));
        assert!(
            !script.contains("limit 5"),
            "limit must not cap the raw query"
        );
        assert!(script.trim_end().ends_with("return out"));
    }

    // --- clean_snippet ---

    #[test]
    fn clean_snippet_strips_task_marker_and_attributes() {
        let s =
            "- [ ] Approve: do the thing @cam #needscam [assignee:cam] [dream_prop:2026-09-13-1]";
        assert_eq!(clean_snippet(s), "Approve: do the thing @cam #needscam");
    }

    #[test]
    fn clean_snippet_strips_spaced_attributes_and_star_bullets() {
        let s = "* [x] Rotate the token @cam #agent [assignee: cam] [created: 2026-09-26]";
        assert_eq!(clean_snippet(s), "Rotate the token @cam #agent");
    }

    #[test]
    fn clean_snippet_keeps_wiki_links_and_non_attribute_brackets() {
        let s = "- see [[Projects/X]] and [draft] notes @cam";
        assert_eq!(
            clean_snippet(s),
            "see [[Projects/X]] and [draft] notes @cam"
        );
    }

    #[test]
    fn clean_snippet_plain_paragraph_is_just_whitespace_collapsed() {
        assert_eq!(clean_snippet("  hey   @cam, look  "), "hey @cam, look");
    }

    // --- render_human ---

    fn sample_rows() -> serde_json::Value {
        serde_json::json!([
            {"page": "Pending/A", "fromTag": "task", "snippet": "- [ ] First @cam [assignee:cam]"},
            {"page": "Log/B", "fromTag": "paragraph", "snippet": "ping @cam about this"},
            {"page": "Pending/A", "fromTag": "task", "snippet": "- [ ] Second @cam #needscam"}
        ])
    }

    #[test]
    fn render_human_groups_by_page_in_first_seen_order() {
        let mut out = Vec::new();
        render_human(&sample_rows(), "@cam", false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "@cam · 3 open mentions\n\nPending/A\n  ☐ First @cam\n  ☐ Second @cam #needscam\n\nLog/B\n  • ping @cam about this\n"
        );
    }

    #[test]
    fn render_human_without_color_has_no_ansi_escapes() {
        let mut out = Vec::new();
        render_human(&sample_rows(), "@cam", false, false, &mut out).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains('\x1b'));
    }

    #[test]
    fn render_human_with_color_styles_the_output() {
        let mut out = Vec::new();
        render_human(&sample_rows(), "@cam", false, true, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains('\x1b'));
    }

    #[test]
    fn render_human_empty_prints_a_note_and_no_json() {
        let mut out = Vec::new();
        render_human(&serde_json::json!([]), "@cam", false, false, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "No open mentions for @cam.\n"
        );

        let mut quiet = Vec::new();
        render_human(&serde_json::json!([]), "@cam", true, false, &mut quiet).unwrap();
        assert!(quiet.is_empty());
    }

    // --- execute() end-to-end against a mock Runtime API ---

    #[tokio::test]
    async fn errors_when_runtime_disabled() {
        let tmp = make_space(Some("http://127.0.0.1:1"));
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(
            None,
            Some("cam"),
            200,
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
    async fn missing_identity_with_no_to_flag_is_a_usage_error() {
        let server = MockServer::start().await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, None, 200, &[], &OutputFormat::Json, true, false)
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::Usage(_)));
    }

    #[tokio::test]
    async fn execute_succeeds_on_an_empty_result() {
        // execute()'s plumbing is what this checks; the actual rendered
        // shape of an empty result ("[]" on stdout) is asserted directly
        // against render() in links.rs's own tests (render() is shared), and
        // end to end in
        // tests/cli_links_inbox_test.rs::inbox_empty_result_prints_note_on_stderr_and_empty_array_on_stdout,
        // which can capture the real stdout this function writes to.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":[]}"#))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(
            None,
            Some("cam"),
            200,
            &[],
            &OutputFormat::Json,
            true,
            false,
        )
        .await;
        assert!(res.is_ok());
    }

    #[test]
    fn render_includes_the_at_mention_snippet_field() {
        // Direct content check for the inbox fixture shape (kind ==
        // "at-mention"), complementing links.rs's render() coverage of the
        // "mention" shape.
        let fixture = serde_json::json!([{
            "kind": "at-mention",
            "tag": "relation",
            "to": "@cam",
            "from": "Some Page",
            "snippet": "@cam please review",
        }]);
        let mut buf = Vec::new();
        crate::commands::links::render(
            &fixture,
            &[],
            &OutputFormat::Json,
            true,
            "No mentions.",
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("@cam please review"));
    }

    #[tokio::test]
    async fn to_with_and_without_at_hit_the_server_with_the_identical_query() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":[]}"#))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        // Both must succeed identically; the query-string equivalence itself
        // is asserted directly in build_inbox_script/normalize_identity tests.
        let a = execute(
            None,
            Some("cam"),
            200,
            &[],
            &OutputFormat::Json,
            true,
            false,
        )
        .await;
        let b = execute(
            None,
            Some("@cam"),
            200,
            &[],
            &OutputFormat::Json,
            true,
            false,
        )
        .await;
        assert!(a.is_ok() && b.is_ok());
    }
}
