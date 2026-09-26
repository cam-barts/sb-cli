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
    _color: bool,
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
    render(&result, fields, format, quiet, "No mentions.", &mut handle)?;
    Ok(())
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
fn build_inbox_script(identity: &str, limit: usize) -> String {
    let literal = lua_string_literal(identity);
    format!(
        "local target = {literal}\nreturn query[[from index.tag \"relation\" where kind == \"at-mention\" and to == target limit {limit}]]"
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
        assert!(script.contains("limit 200"));
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
            r#"return query[[from index.tag "relation" where kind == "at-mention" and to == target limit 200]]"#
        );
    }

    #[test]
    fn build_inbox_script_uses_plain_double_bracket_not_a_leveled_one() {
        let script = build_inbox_script("@cam", 200);
        assert!(script.contains("query[["));
        assert!(!script.contains("[==["));
        assert!(!script.contains("]==]"));
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
