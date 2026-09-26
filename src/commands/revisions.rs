//! `sb page history|diff|restore` — git-backed page history over `/.revisions`.
//!
//! These are plain GETs against the space's git-backed revision store, not
//! Runtime API calls, so they skip the `runtime_available` check that
//! `lua`/`query`/`describe` need.

use crate::cli::OutputFormat;
use crate::client::{FileRevisions, RevisionEntry};
use crate::commands::page::{find_content_dir, page_name_to_path, validate_page_path};
use crate::commands::server::build_client;
use crate::error::{SbError, SbResult};
use crate::output;
use std::path::Path;

/// Validate that `rev` is a full 40-character hex commit hash.
///
/// The server 404s anything else, which reads as a confusing "not found"
/// for what is actually a typo. Checked locally, before any request goes
/// out, so the failure is a clear usage error (exit 2) instead.
fn validate_rev(rev: &str) -> SbResult<()> {
    if rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(SbError::Usage(format!(
            "invalid revision '{rev}': expected a full 40-character commit hash"
        )))
    }
}

/// Interactively pick a page from the space. Prints a cancellation notice and
/// returns `None` when the user backs out. Mirrors `page::pick_page`, which is
/// private to that module.
async fn pick_page(content_dir: &Path, quiet: bool) -> SbResult<Option<String>> {
    let names = crate::commands::page::list_page_names(content_dir)?;
    let picked = crate::commands::picker::pick(&names, "page").await?;
    if picked.is_none() && !quiet {
        eprintln!("Cancelled.");
    }
    Ok(picked)
}

/// Resolve a page name (explicit or interactively picked) to its
/// space-relative `.md` path, validating against traversal.
async fn resolve_name_and_path(
    name: Option<&str>,
    quiet: bool,
) -> SbResult<Option<(String, String)>> {
    let content_dir = find_content_dir()?;
    let name = match name {
        Some(n) => n.to_string(),
        None => match pick_page(&content_dir, quiet).await? {
            Some(n) => n,
            None => return Ok(None),
        },
    };
    validate_page_path(&content_dir, &name)?;
    let rel_path = page_name_to_path(&name).to_string_lossy().into_owned();
    Ok(Some((name, rel_path)))
}

/// Format one revision entry as a single human-readable line.
fn render_history_line(entry: &RevisionEntry) -> String {
    let short_rev = &entry.rev[..entry.rev.len().min(12)];
    let when = format_ms_human(entry.timestamp);
    format!(
        "{short_rev}  {when}  {:<20}  {}",
        entry.author, entry.message
    )
}

/// Format a Unix-milliseconds timestamp as `"YYYY-MM-DD HH:MM"` in local time.
fn format_ms_human(ms: i64) -> String {
    match jiff::Timestamp::from_millisecond(ms) {
        Ok(ts) => ts
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Render a `FileRevisions` history to stdout/stderr per `format`.
fn render_history(history: &FileRevisions, name: &str, format: &OutputFormat, quiet: bool) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(history).unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            if history.revisions.is_empty() {
                if !quiet {
                    eprintln!("No revisions for '{name}' (mode: {}).", history.mode);
                }
            } else {
                for entry in &history.revisions {
                    println!("{}", render_history_line(entry));
                }
                if history.more && !quiet {
                    eprintln!("... more revisions available (--before <hash>)");
                }
            }
            if history.uncommitted && !quiet {
                eprintln!("note: '{name}' has uncommitted changes on disk.");
            }
        }
    }
}

/// List a page's revision history.
pub async fn execute_history(
    cli_token: Option<&str>,
    name: Option<&str>,
    limit: usize,
    before: Option<&str>,
    format: &OutputFormat,
    quiet: bool,
    _color: bool,
) -> SbResult<()> {
    if let Some(b) = before {
        validate_rev(b)?;
    }
    let Some((name, rel_path)) = resolve_name_and_path(name, quiet).await? else {
        return Ok(());
    };

    let client = build_client(cli_token)?;
    let history = client.get_file_revisions(&rel_path, before, limit).await?;
    render_history(&history, &name, format, quiet);
    Ok(())
}

/// A short, human-facing explanation for the two documented "nothing to
/// diff" 404s: a revision with no parent to diff against (root/merge
/// commit), and an uncommitted diff that turns out to match HEAD.
fn no_diff_message(name: &str, rev: Option<&str>) -> String {
    match rev {
        Some(r) => format!(
            "no diff available for '{name}' at revision {r} (root commit, or nothing to diff against a parent)"
        ),
        None => format!("no uncommitted changes to '{name}'"),
    }
}

/// Show what a revision changed in a page, as a unified diff.
pub async fn execute_diff(
    cli_token: Option<&str>,
    name: Option<&str>,
    rev: Option<&str>,
    format: &OutputFormat,
    quiet: bool,
    _color: bool,
) -> SbResult<()> {
    if let Some(r) = rev {
        validate_rev(r)?;
    }
    let Some((name, rel_path)) = resolve_name_and_path(name, quiet).await? else {
        return Ok(());
    };

    let client = build_client(cli_token)?;
    let diff = client.get_revision_diff(&rel_path, rev).await?;

    match (format, &diff) {
        (OutputFormat::Json, _) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "diff": diff }))
                    .unwrap_or_default()
            );
        }
        (OutputFormat::Human, Some(text)) => {
            print!("{text}");
        }
        (OutputFormat::Human, None) => {
            if !quiet {
                eprintln!("{}", no_diff_message(&name, rev));
            }
        }
    }
    Ok(())
}

/// Prompt user for restore confirmation on a TTY. Only called when
/// interactive input is available; the non-interactive fail-safe lives in
/// `execute_restore`. Mirrors `page::confirm_delete`.
async fn confirm_restore(name: &str, rev: &str) -> SbResult<bool> {
    let name = name.to_string();
    let rev = rev.to_string();
    let confirmed = tokio::task::spawn_blocking(move || -> bool {
        use std::io::Write;
        eprint!("Restore '{name}' to revision {rev}? [y/N] ");
        std::io::stderr().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("restore prompt task failed: {e}"),
    })?;
    Ok(confirmed)
}

/// Restore a page to an earlier revision.
///
/// Writes the fetched content to the LOCAL file only and stops there --
/// deliberately does NOT `PUT` to the server. That leaves the change to go
/// out on the next `sb sync`, through the exact same conflict handling as any
/// other local edit, rather than a direct write bypassing the conditional-
/// write protection sync uses.
pub async fn execute_restore(
    cli_token: Option<&str>,
    name: Option<&str>,
    rev: &str,
    force: bool,
    format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    validate_rev(rev)?;
    let content_dir = find_content_dir()?;
    let name = match name {
        Some(n) => n.to_string(),
        None => match pick_page(&content_dir, quiet).await? {
            Some(n) => n,
            None => return Ok(()),
        },
    };
    let page_path = validate_page_path(&content_dir, &name)?;
    let rel_path = page_name_to_path(&name).to_string_lossy().into_owned();

    if !(force || output::assume_yes()) {
        if output::no_input() {
            return Err(SbError::ConfirmationRequired {
                action: format!("restore page '{name}' to revision {rev}"),
                rerun: format!("sb page restore {name} --rev {rev} --force"),
            });
        }
        let confirmed = confirm_restore(&name, rev).await?;
        if !confirmed {
            output::print_success("Cancelled", color, quiet);
            return Ok(());
        }
    }

    let client = build_client(cli_token)?;
    let content = client.get_revision_content(&rel_path, rev).await?;

    if let Some(parent) = page_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
            message: "failed to create parent directories".to_string(),
            path: parent.display().to_string(),
            source: Some(e),
        })?;
    }
    std::fs::write(&page_path, &content).map_err(|e| SbError::Filesystem {
        message: "failed to write restored page".to_string(),
        path: page_path.display().to_string(),
        source: Some(e),
    })?;

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "restored": true, "name": name, "rev": rev })
                )
                .unwrap_or_default()
            );
        }
        OutputFormat::Human => {
            output::print_success(
                &format!(
                    "restored '{name}' to revision {}. Run `sb sync` to push it.",
                    &rev[..12.min(rev.len())]
                ),
                color,
                quiet,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rev_accepts_full_hex40() {
        assert!(validate_rev(&"a".repeat(40)).is_ok());
        assert!(validate_rev("0123456789abcdef0123456789abcdef01234567").is_ok());
    }

    #[test]
    fn validate_rev_rejects_short_hash() {
        let err = validate_rev("abc123").expect_err("short hash should be rejected");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("40-character"));
    }

    #[test]
    fn validate_rev_rejects_non_hex_characters() {
        let bad = format!("{}z", "a".repeat(39));
        let err = validate_rev(&bad).expect_err("non-hex should be rejected");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn validate_rev_rejects_overlong_hash() {
        let err = validate_rev(&"a".repeat(41)).expect_err("overlong hash should be rejected");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn no_diff_message_mentions_root_or_merge_when_rev_given() {
        let msg = no_diff_message("index", Some(&"a".repeat(40)));
        assert!(msg.contains("root commit"));
        assert!(msg.contains("index"));
    }

    #[test]
    fn no_diff_message_mentions_uncommitted_when_rev_omitted() {
        let msg = no_diff_message("index", None);
        assert!(msg.contains("no uncommitted changes"));
    }

    #[test]
    fn render_history_line_includes_short_rev_author_and_message() {
        let entry = RevisionEntry {
            rev: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            timestamp: 1700000000000,
            author: "alice".to_string(),
            message: "edit note".to_string(),
            added: 1,
            removed: 0,
        };
        let line = render_history_line(&entry);
        assert!(line.starts_with("abcdef012345"));
        assert!(line.contains("alice"));
        assert!(line.contains("edit note"));
    }

    #[test]
    fn format_ms_human_renders_a_date() {
        // A known instant: 2023-11-14T22:13:20Z.
        let s = format_ms_human(1700000000000);
        assert!(s.contains("2023"), "{s}");
    }

    // --- execute_* end-to-end tests (space + wiremock, no interactive input) ---

    use crate::test_util::{make_space, SbSpaceGuard};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REV: &str = "abcdef0123456789abcdef0123456789abcdef01";

    fn write_page(space_root: &std::path::Path, name: &str, content: &str) {
        std::fs::write(space_root.join(format!("{name}.md")), content).unwrap();
    }

    #[tokio::test]
    async fn execute_history_prints_json_and_hits_expected_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/Note.md"))
            .and(query_param("limit", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r#"{{"mode":"managed","uncommitted":false,"more":false,"revisions":[{{"rev":"{REV}","timestamp":1700000000000,"author":"alice","message":"edit","added":1,"removed":0}}]}}"#
            )))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "content");
        let _g = SbSpaceGuard::set(tmp.path());

        let result = execute_history(
            None,
            Some("Note"),
            50,
            None,
            &OutputFormat::Json,
            true,
            false,
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn execute_history_rejects_bad_before_before_any_request() {
        // No mock mounted at all -- a request would 404 against an unmatched
        // route and this must never get that far.
        let server = MockServer::start().await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "content");
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute_history(
            None,
            Some("Note"),
            50,
            Some("not-a-hash"),
            &OutputFormat::Human,
            true,
            false,
        )
        .await
        .expect_err("bad --before should be rejected");
        assert_eq!(err.exit_code(), 2);
    }

    #[tokio::test]
    async fn execute_history_disabled_maps_to_revisions_disabled() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/Note.md"))
            .respond_with(
                ResponseTemplate::new(404).set_body_string(r#"{"error": "revisions disabled"}"#),
            )
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "content");
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute_history(
            None,
            Some("Note"),
            50,
            None,
            &OutputFormat::Human,
            true,
            false,
        )
        .await
        .expect_err("disabled space should error");
        assert!(matches!(err, SbError::RevisionsDisabled));
    }

    #[tokio::test]
    async fn execute_history_without_a_name_in_noninteractive_env_errors() {
        // No TTY in the test process, so the interactive picker refuses
        // rather than hanging.
        let tmp = make_space(Some("http://127.0.0.1:1"));
        write_page(tmp.path(), "Note", "content");
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute_history(None, None, 50, None, &OutputFormat::Human, true, false)
            .await
            .expect_err("no name + no TTY should error");
        assert_eq!(err.exit_code(), 2);
    }

    #[tokio::test]
    async fn execute_diff_with_rev_prints_the_diff() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/Note.md"))
            .and(query_param("format", "diff"))
            .and(query_param("rev", REV))
            .respond_with(ResponseTemplate::new(200).set_body_string("@@ -1 +1 @@\n-a\n+b\n"))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "content");
        let _g = SbSpaceGuard::set(tmp.path());

        let result = execute_diff(
            None,
            Some("Note"),
            Some(REV),
            &OutputFormat::Human,
            true,
            false,
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn execute_diff_without_rev_handles_a_benign_404_as_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/Note.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "content");
        let _g = SbSpaceGuard::set(tmp.path());

        let result = execute_diff(None, Some("Note"), None, &OutputFormat::Json, true, false).await;
        assert!(result.is_ok(), "a benign no-diff 404 must not be an Err");
    }

    #[tokio::test]
    async fn execute_diff_rejects_bad_rev_before_any_request() {
        let server = MockServer::start().await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "content");
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute_diff(
            None,
            Some("Note"),
            Some("short"),
            &OutputFormat::Human,
            true,
            false,
        )
        .await
        .expect_err("short rev should be rejected");
        assert_eq!(err.exit_code(), 2);
    }

    #[tokio::test]
    async fn execute_restore_without_force_or_yes_requires_confirmation() {
        let server = MockServer::start().await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "original");
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute_restore(
            None,
            Some("Note"),
            REV,
            false,
            &OutputFormat::Human,
            true,
            false,
        )
        .await
        .expect_err("restore without --force/--yes must require confirmation");
        assert_eq!(err.exit_code(), 6);
        // File must be untouched -- confirmation happens before any read/write.
        let content = std::fs::read_to_string(tmp.path().join("Note.md")).unwrap();
        assert_eq!(content, "original");
    }

    #[tokio::test]
    async fn execute_restore_with_force_writes_local_file_and_never_puts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.revisions/Note.md"))
            .and(query_param("rev", REV))
            .respond_with(ResponseTemplate::new(200).set_body_string("old content"))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "current content");
        let _g = SbSpaceGuard::set(tmp.path());

        let result = execute_restore(
            None,
            Some("Note"),
            REV,
            true,
            &OutputFormat::Json,
            true,
            false,
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        let content = std::fs::read_to_string(tmp.path().join("Note.md")).unwrap();
        assert_eq!(content, "old content");
    }

    #[tokio::test]
    async fn execute_restore_rejects_bad_rev_before_any_request() {
        let server = MockServer::start().await;
        let tmp = make_space(Some(&server.uri()));
        write_page(tmp.path(), "Note", "original");
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute_restore(
            None,
            Some("Note"),
            "not-a-real-hash",
            true,
            &OutputFormat::Human,
            true,
            false,
        )
        .await
        .expect_err("bad rev should be rejected");
        assert_eq!(err.exit_code(), 2);
        let content = std::fs::read_to_string(tmp.path().join("Note.md")).unwrap();
        assert_eq!(content, "original");
    }
}
