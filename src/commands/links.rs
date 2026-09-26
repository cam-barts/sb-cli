//! `sb links` — wiki links between pages, read from the server's relation index.
//!
//! Relation objects look like:
//!
//! ```json
//! {"from":"Homelab/Blog Style Upgrade","fromTag":"page","kind":"mention",
//!  "page":"Homelab/Blog Style Upgrade","pageLastModified":"2026-06-23T12:53:57.311",
//!  "range":[0,32],"ref":"Homelab/Blog Style Upgrade@0",
//!  "snippet":"[[Library/Personal/Observatory]]",
//!  "tag":"relation","to":"Library/Personal/Observatory","toTag":"page"}
//! ```
//!
//! `index.relations` is a Lua *function*, not a query source — the only valid
//! source is `index.tag "relation"`, filtered with a `where` clause.

use crate::cli::OutputFormat;
use crate::commands::server::{build_client, runtime_unavailable_error};
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};
use console::Style;

#[allow(clippy::too_many_arguments)]
pub async fn execute(
    cli_token: Option<&str>,
    page: Option<&str>,
    outgoing: bool,
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

    let page = match page {
        Some(p) => p.to_string(),
        None => {
            let content_dir = space_root.join(&config.sync_dir.value);
            match pick_page(&content_dir, quiet).await? {
                Some(p) => p,
                None => return Ok(()),
            }
        }
    };

    let client = build_client(cli_token)?;
    let lua_script = build_links_script(&page, outgoing, limit);
    let result = crate::runtime::eval(&client, "/.runtime/lua_script", &lua_script).await?;

    let empty_note = if outgoing {
        "No outgoing links."
    } else {
        "No backlinks."
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    // `--fields` and JSON keep the generic relation table; the default
    // human view is grouped and readable.
    if matches!(format, OutputFormat::Human) && fields.is_empty() {
        render_human(&result, &page, outgoing, quiet, color, &mut handle).map_err(|e| {
            SbError::Internal {
                message: format!("failed to write output: {e}"),
            }
        })?;
        return Ok(());
    }
    render(&result, fields, format, quiet, empty_note, &mut handle)?;
    Ok(())
}

/// Human view. Backlinks: each linking page, with the line that links here
/// underneath and the link to `page` highlighted. Outgoing: the distinct
/// target pages, with a count when a page is linked more than once.
fn render_human(
    result: &serde_json::Value,
    page: &str,
    outgoing: bool,
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
    let hit = style(Style::new().yellow().bold());

    let rows = result.as_array().cloned().unwrap_or_default();
    let field = |row: &serde_json::Value, key: &str| -> String {
        row.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    if rows.is_empty() {
        if !quiet {
            if outgoing {
                writeln!(out, "{page} has no outgoing links.")?;
            } else {
                writeln!(out, "No backlinks to {page}.")?;
            }
        }
        return Ok(());
    }

    if outgoing {
        let mut targets: Vec<(String, usize)> = Vec::new();
        for row in &rows {
            let to = field(row, "to");
            match targets.iter_mut().find(|(t, _)| *t == to) {
                Some((_, n)) => *n += 1,
                None => targets.push((to, 1)),
            }
        }
        let n = targets.len();
        let noun = if n == 1 { "page" } else { "pages" };
        writeln!(
            out,
            "{} {}",
            head.apply_to(page),
            dim.apply_to(format!("→ links to {n} {noun}"))
        )?;
        writeln!(out)?;
        for (target, count) in targets {
            let times = if count > 1 {
                format!(" {}", dim.apply_to(format!("×{count}")))
            } else {
                String::new()
            };
            writeln!(out, "  {}{times}", page_style.apply_to(target))?;
        }
        return Ok(());
    }

    let mut sources: Vec<(String, Vec<String>)> = Vec::new();
    for row in &rows {
        let src = field(row, "page");
        let snippet = crate::commands::inbox::clean_snippet(&field(row, "snippet"));
        match sources.iter_mut().find(|(p, _)| *p == src) {
            Some((_, snips)) => snips.push(snippet),
            None => sources.push((src, vec![snippet])),
        }
    }
    let n = sources.len();
    let noun = if n == 1 { "page" } else { "pages" };
    writeln!(
        out,
        "{} {}",
        head.apply_to(page),
        dim.apply_to(format!("← linked from {n} {noun}"))
    )?;
    for (src, snippets) in sources {
        writeln!(out)?;
        writeln!(out, "{}", page_style.apply_to(src))?;
        for snippet in snippets {
            writeln!(out, "  {}", highlight_links_to(&snippet, page, &hit))?;
        }
    }
    Ok(())
}

/// Style every `[[...]]` wiki link in `text` that points at `page` (ignoring
/// an `|alias` or `#heading` suffix). Other text is left untouched.
fn highlight_links_to(text: &str, page: &str, hit: &Style) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let inner = &after[..end];
                let target = inner.split(['|', '#']).next().unwrap_or("").trim();
                let link = &rest[start..start + 2 + end + 2];
                if target == page {
                    out.push_str(&hit.apply_to(link).to_string());
                } else {
                    out.push_str(link);
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Interactively pick a page from the space when no page name is given.
/// Copied from `page::pick_page` (module-private there) rather than reused,
/// since the two callers live in different modules.
async fn pick_page(content_dir: &std::path::Path, quiet: bool) -> SbResult<Option<String>> {
    let names = crate::commands::page::list_page_names(content_dir)?;
    let picked = crate::commands::picker::pick(&names, "page").await?;
    if picked.is_none() && !quiet {
        eprintln!("Cancelled.");
    }
    Ok(picked)
}

/// Encode `s` as a double-quoted Lua string literal.
///
/// This is a plain Lua-literal escaper, *not* a SLIQ-injection guard: nothing
/// in `build_links_script`/`build_inbox_script` interpolates this value into
/// query syntax any more (see the comment on `build_links_script` for why).
/// The only things that can break a Lua double-quoted string literal are a
/// raw backslash or double quote, and a raw control character — an unescaped
/// literal newline inside a quoted Lua string is a syntax error, not just
/// cosmetic. Escape all of those; everything else (including `]]`, single
/// quotes, and non-ASCII text) passes through untouched, since none of it is
/// special inside a Lua string literal.
pub(crate) fn lua_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\{:03}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build the Runtime API script for `sb links`.
///
/// IMPORTANT — do not "fix" this back to a leveled long bracket
/// (`query[==[...]==]`). That looks like the right way to let a page name
/// contain `]]`, and it is valid Lua 5.4, but Space Lua does not implement
/// leveled long brackets. Verified against the live server:
///
///   sb lua '#query[[from index.tag "relation" limit 3]]'        =>  3
///   sb lua '#query[==[from index.tag "relation" limit 3]==]'     =>  server returned 500
///
/// So the query body has to stay a plain `[[...]]` bracket, which means it
/// can never contain raw user data — a page name containing `]]` would close
/// it early (and, before this fix, doubled as a Lua/SLIQ injection hole into
/// a script POSTed to an endpoint that can write pages and run shell
/// commands). Instead, the page name is declared as a Lua local *above* the
/// query and referenced by name *inside* it — SLIQ resolves variables from
/// the enclosing Lua scope, so the query text itself is a fixed constant
/// containing no user data at all. Verified against the live server,
/// including hostile names with embedded quotes, `]]`, and `]==]`: parses
/// cleanly, no 500, no injection.
pub(crate) fn build_links_script(page: &str, outgoing: bool, limit: usize) -> String {
    let field = if outgoing { "from" } else { "to" };
    let literal = lua_string_literal(page);
    format!(
        "local target = {literal}\nreturn query[[from index.tag \"relation\" where kind == \"mention\" and {field} == target limit {limit}]]"
    )
}

/// Shared renderer for `sb links` and `sb inbox`: both list relation objects
/// and follow the same output contract.
///
/// - stdout carries data only: the (optionally `--fields`-trimmed) JSON array
///   in `--format json`, or a table in `--format human`.
/// - stderr carries diagnostics only: an empty-result note (suppressed by
///   `--quiet`).
/// - An empty result is never a bare error: it prints the note on stderr and
///   an empty JSON array on stdout in either format, so scripts consuming
///   stdout never have to special-case "no results".
pub(crate) fn render(
    result: &serde_json::Value,
    fields: &[String],
    format: &OutputFormat,
    quiet: bool,
    empty_note: &str,
    out: &mut dyn std::io::Write,
) -> SbResult<()> {
    let write_err = |e: std::io::Error| SbError::Internal {
        message: format!("failed to write output: {e}"),
    };

    let rows = result.as_array().cloned().unwrap_or_default();
    let filtered =
        crate::output::filter_json_fields(&serde_json::Value::Array(rows.clone()), fields);

    if rows.is_empty() {
        if !quiet {
            eprintln!("{empty_note}");
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&filtered).unwrap_or_else(|_| "[]".to_string())
        )
        .map_err(write_err)?;
        return Ok(());
    }

    match format {
        OutputFormat::Json => {
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&filtered).unwrap_or_default()
            )
            .map_err(write_err)?;
        }
        OutputFormat::Human => {
            if let serde_json::Value::Array(items) = &filtered {
                render_table(items, out).map_err(write_err)?;
            }
        }
    }
    Ok(())
}

/// Render a JSON array of objects as an ASCII table. Copied from the shape of
/// `query::render_table` -- simple `format!()` padding, no table crate.
fn render_table(rows: &[serde_json::Value], out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let columns: Vec<String> = if let Some(obj) = rows.first().and_then(|r| r.as_object()) {
        obj.keys().cloned().collect()
    } else {
        for row in rows {
            writeln!(out, "{}", value_to_string(row))?;
        }
        return Ok(());
    };

    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        if let Some(obj) = row.as_object() {
            for (i, col) in columns.iter().enumerate() {
                let val = obj.get(col).map(value_to_string).unwrap_or_default();
                widths[i] = widths[i].max(val.len());
            }
        }
    }

    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
        .collect();
    writeln!(out, "{}", header.join(" | "))?;

    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    writeln!(out, "{}", sep.join("-+-"))?;

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
            writeln!(out, "{}", cells.join(" | "))?;
        }
    }
    Ok(())
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
    use crate::error::SbError;
    use crate::test_util::{make_space, SbSpaceGuard};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // --- render_human ---

    fn backlink_rows() -> serde_json::Value {
        serde_json::json!([
            {"page": "Map", "to": "index", "snippet": "See also: [[index]] and [[Other]]"},
            {"page": "Dash", "to": "index", "snippet": "> [[index|home]] views"},
            {"page": "Map", "to": "index", "snippet": "- back to [[index#Top]] [pos:2]"}
        ])
    }

    #[test]
    fn human_backlinks_group_by_linking_page() {
        let mut out = Vec::new();
        render_human(&backlink_rows(), "index", false, false, false, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "index ← linked from 2 pages\n\nMap\n  See also: [[index]] and [[Other]]\n  back to [[index#Top]]\n\nDash\n  [[index|home]] views\n"
        );
    }

    #[test]
    fn human_outgoing_lists_distinct_targets_with_counts() {
        let rows = serde_json::json!([
            {"page": "index", "to": "A"}, {"page": "index", "to": "B"}, {"page": "index", "to": "A"}
        ]);
        let mut out = Vec::new();
        render_human(&rows, "index", true, false, false, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "index → links to 2 pages\n\n  A ×2\n  B\n"
        );
    }

    #[test]
    fn human_empty_results_print_one_line() {
        let mut back = Vec::new();
        render_human(&serde_json::json!([]), "X", false, false, false, &mut back).unwrap();
        assert_eq!(String::from_utf8(back).unwrap(), "No backlinks to X.\n");
        let mut fwd = Vec::new();
        render_human(&serde_json::json!([]), "X", true, false, false, &mut fwd).unwrap();
        assert_eq!(
            String::from_utf8(fwd).unwrap(),
            "X has no outgoing links.\n"
        );
    }

    #[test]
    fn human_color_only_when_asked() {
        let mut plain = Vec::new();
        render_human(&backlink_rows(), "index", false, false, false, &mut plain).unwrap();
        assert!(!String::from_utf8(plain).unwrap().contains('\x1b'));
        let mut colored = Vec::new();
        render_human(&backlink_rows(), "index", false, false, true, &mut colored).unwrap();
        assert!(String::from_utf8(colored).unwrap().contains('\x1b'));
    }

    #[test]
    fn highlight_links_to_only_styles_links_to_the_page() {
        let hit = Style::new().bold().force_styling(true);
        let s = highlight_links_to("[[index|x]] vs [[indexer]] vs [[index]]", "index", &hit);
        assert_eq!(s.matches('\x1b').count(), 4, "two links styled: {s:?}");
        assert!(s.contains(" vs [[indexer]] vs "));
    }

    fn enable_runtime(space_root: &std::path::Path) {
        crate::config::update_config_value(&space_root.join(".sb"), "runtime", "available", true)
            .unwrap();
    }

    fn relation_fixture() -> serde_json::Value {
        serde_json::json!([{
            "from": "Homelab/Blog Style Upgrade",
            "fromTag": "page",
            "kind": "mention",
            "page": "Homelab/Blog Style Upgrade",
            "pageLastModified": "2026-06-23T12:53:57.311",
            "range": [0, 32],
            "ref": "Homelab/Blog Style Upgrade@0",
            "snippet": "[[Library/Personal/Observatory]]",
            "tag": "relation",
            "to": "Library/Personal/Observatory",
            "toTag": "page",
        }])
    }

    // --- lua_string_literal ---

    #[test]
    fn lua_string_literal_passes_apostrophes_through_untouched() {
        // A single quote never needs escaping inside a double-quoted literal.
        assert_eq!(lua_string_literal("Cam's Notes"), "\"Cam's Notes\"");
    }

    #[test]
    fn lua_string_literal_escapes_double_quotes_and_backslashes() {
        assert_eq!(lua_string_literal(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(lua_string_literal(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn lua_string_literal_escapes_newline_carriage_return_and_tab() {
        // A raw newline inside a Lua double-quoted string is a syntax error,
        // so it must never survive into the literal unescaped.
        let out = lua_string_literal("line one\nline two\r\ttabbed");
        assert_eq!(out, r#""line one\nline two\r\ttabbed""#);
        assert!(!out.contains('\n'));
        assert!(!out.contains('\r'));
    }

    #[test]
    fn lua_string_literal_escapes_other_control_characters() {
        // Any other control byte (e.g. \x01, \x07 bell, \x7f DEL) is escaped
        // as a zero-padded three-digit Lua decimal escape so it can never be
        // ambiguous with a following literal digit.
        let out = lua_string_literal("a\u{1}b\u{7}c\u{7f}d");
        assert_eq!(out, r#""a\001b\007c\127d""#);
        assert!(out.chars().all(|c| (c as u32) >= 0x20 || c == '"'));
    }

    #[test]
    fn lua_string_literal_leaves_long_bracket_sequences_untouched() {
        // `]]` and `]==]` are only meaningful inside a Lua *long bracket*
        // string; inside a plain double-quoted literal they're just text and
        // need no escaping. This is exactly why binding the value to a local
        // and quoting it this way closes the injection hole: the sequence
        // that used to break out of `[==[...]==]` is inert here.
        let out = lua_string_literal("Weird]]Page ]==] more");
        assert_eq!(out, r#""Weird]]Page ]==] more""#);
    }

    // --- build_links_script ---

    #[test]
    fn build_links_script_declares_target_local_from_the_escaped_page_name() {
        let script = build_links_script("Cam's Notes", false, 50);
        assert_eq!(
            script.lines().next().unwrap(),
            r#"local target = "Cam's Notes""#
        );
    }

    #[test]
    fn build_links_script_filters_to_field_by_default() {
        let script = build_links_script("Cam's Notes", false, 50);
        assert!(script.contains("to == target"));
        assert!(script.contains(r#"kind == "mention""#));
        assert!(script.contains("limit 50"));
        assert!(!script.contains("from == target")); // backlinks: `to`, not `from`
    }

    #[test]
    fn build_links_script_filters_from_field_when_outgoing() {
        let script = build_links_script("Page A", true, 10);
        assert!(script.contains("from == target"));
    }

    #[test]
    fn build_links_script_uses_plain_double_bracket_not_a_leveled_one() {
        // Space Lua rejects `[==[...]==]` -- see the comment on
        // build_links_script for the live-server evidence. Guard against a
        // future "fix" that reintroduces it.
        let script = build_links_script("Some Page", false, 50);
        assert!(script.contains("query[["));
        assert!(!script.contains("[==["));
        assert!(!script.contains("]==]"));
    }

    #[test]
    fn build_links_script_query_line_is_a_fixed_constant_regardless_of_page_name() {
        // The money property: no matter what garbage is in the page name --
        // embedded quotes, backslashes, `]]`, `]==]` -- the query text itself
        // never changes. The page name only ever reaches the script via the
        // Lua local declared on line one, never inline in the query body, so
        // there is no way for it to escape its literal and become Lua code.
        let hostile = "Weird]==]Page\" .. (os.execute or print)(\"pwn\") --";
        let script = build_links_script(hostile, false, 50);
        let query_line = script.lines().nth(1).unwrap();
        assert_eq!(
            query_line,
            r#"return query[[from index.tag "relation" where kind == "mention" and to == target limit 50]]"#
        );
        // And the hostile text is confined to the escaped literal on line one.
        assert_eq!(script.lines().count(), 2);
        assert_eq!(
            script.lines().next().unwrap(),
            format!("local target = {}", lua_string_literal(hostile))
        );
    }

    // --- execute() end-to-end against a mock Runtime API ---

    #[tokio::test]
    async fn errors_when_runtime_disabled() {
        let tmp = make_space(Some("http://127.0.0.1:1"));
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(
            None,
            Some("Some Page"),
            false,
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
    async fn execute_succeeds_against_a_mocked_runtime() {
        // execute()'s only job beyond render() is plumbing: load config,
        // build the client, eval the script, hand the result to render().
        // That plumbing is what this test checks; the *content* of what gets
        // rendered (snippet field, empty-result shape, --fields trimming) is
        // covered directly against render() below, and by the assert_cmd
        // end-to-end tests in tests/cli_links_inbox_test.rs, which can
        // actually capture the real stdout this function writes to.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.runtime/lua_script"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(format!(r#"{{"result":{}}}"#, relation_fixture())),
            )
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_runtime(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(
            None,
            Some("Library/Personal/Observatory"),
            false,
            200,
            &[],
            &OutputFormat::Json,
            true,
            false,
        )
        .await;
        assert!(res.is_ok(), "{res:?}");
    }

    #[tokio::test]
    async fn returns_usage_error_when_server_reports_a_query_error() {
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

        let err = execute(
            None,
            Some("Some Page"),
            false,
            200,
            &[],
            &OutputFormat::Json,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SbError::Usage(_)));
    }

    // --- render() ---
    //
    // render() writes to an injected `dyn Write` rather than real stdout
    // (see the daily.rs::render_entries convention this follows), so these
    // tests capture the actual bytes it produces instead of only checking
    // that a call to it didn't error.

    #[test]
    fn render_json_keeps_snippet_field_when_fields_unset() {
        // No --fields means no trimming: snippet must survive into the output.
        let mut buf = Vec::new();
        render(
            &relation_fixture(),
            &[],
            &OutputFormat::Json,
            true,
            "unused",
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("[[Library/Personal/Observatory]]"));
    }

    #[test]
    fn render_json_drops_snippet_when_fields_excludes_it() {
        let mut buf = Vec::new();
        render(
            &relation_fixture(),
            &["to".to_string()],
            &OutputFormat::Json,
            true,
            "unused",
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("Library/Personal/Observatory"));
        assert!(!out.contains("snippet"));
    }

    #[test]
    fn render_human_prints_a_table_with_the_snippet_column() {
        let mut buf = Vec::new();
        render(
            &relation_fixture(),
            &[],
            &OutputFormat::Human,
            true,
            "unused",
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("snippet"), "header row: {out}");
        assert!(
            out.contains("[[Library/Personal/Observatory]]"),
            "data row: {out}"
        );
    }

    #[test]
    fn render_empty_result_writes_an_empty_json_array_regardless_of_format() {
        let mut buf = Vec::new();
        render(
            &serde_json::json!([]),
            &[],
            &OutputFormat::Human,
            true,
            "No backlinks.",
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out.trim(), "[]");
    }
}
