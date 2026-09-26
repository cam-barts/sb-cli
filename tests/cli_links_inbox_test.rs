/// Integration tests for `sb links` and `sb inbox`.
///
/// Uses assert_cmd to run the real binary and assert on exit code, stdout,
/// stderr. Uses wiremock to stand in for the Runtime API.
use assert_cmd::Command;
use predicates::prelude::*;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Create a temp space with `.sb/config.toml` pointing at `server_url`, and
/// optionally recording `runtime.available = true`.
fn setup_space(server_url: &str, runtime_available: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    let mut config = format!("server_url = \"{server_url}\"\ntoken = \"testtoken\"\n");
    if runtime_available {
        config.push_str("\n[runtime]\navailable = true\n");
    }
    std::fs::write(sb_dir.join("config.toml"), config).unwrap();
    dir
}

fn sb_in(dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("sb").unwrap();
    cmd.current_dir(dir)
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg");
    cmd
}

fn relation_fixture_body() -> String {
    r#"{"result":[{
        "from":"Homelab/Blog Style Upgrade","fromTag":"page","kind":"mention",
        "page":"Homelab/Blog Style Upgrade","pageLastModified":"2026-06-23T12:53:57.311",
        "range":[0,32],"ref":"Homelab/Blog Style Upgrade@0",
        "snippet":"[[Library/Personal/Observatory]]",
        "tag":"relation","to":"Library/Personal/Observatory","toTag":"page"
    }]}"#
        .to_string()
}

// ---------------- sb links ----------------

#[tokio::test]
async fn links_backlinks_render_the_snippet_field_by_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .and(body_string_contains(r#"kind == "mention""#))
        .and(body_string_contains(
            r#"local target = "Library/Personal/Observatory""#,
        ))
        .and(body_string_contains("to == target"))
        .respond_with(ResponseTemplate::new(200).set_body_string(relation_fixture_body()))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args(["--format", "json", "links", "Library/Personal/Observatory"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[[Library/Personal/Observatory]]"));
}

#[tokio::test]
async fn links_from_flag_queries_the_from_field_instead_of_to() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .and(body_string_contains(
            r#"local target = "Homelab/Blog Style Upgrade""#,
        ))
        .and(body_string_contains("from == target"))
        .respond_with(ResponseTemplate::new(200).set_body_string(relation_fixture_body()))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args([
            "--format",
            "json",
            "links",
            "Homelab/Blog Style Upgrade",
            "--from",
        ])
        .assert()
        .success();

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "exactly one lua_script request");
}

#[tokio::test]
async fn links_empty_result_prints_note_on_stderr_and_empty_array_on_stdout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":[]}"#))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args(["--format", "json", "links", "Lonely Page"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[]"))
        .stderr(predicate::str::contains("No backlinks."));
}

/// Runs `sb links <page>` against a mock server and asserts that:
/// - exactly one request was sent (a broken/mis-escaped script would either
///   fail client-side before sending, or the fixed server response wouldn't
///   matter, but a real server would 500 on a malformed script)
/// - the request body's `local target = ...` line matches `expected_literal`
///   exactly
/// - the query line is the unchanged fixed constant -- i.e. `page` never
///   reached the query text itself, only the escaped Lua literal
async fn assert_backlink_query_is_injection_safe(page: &str, expected_literal: &str) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":[]}"#))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args(["--format", "json", "links", page])
        .assert()
        .success();

    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "exactly one request; the page name must not have broken the query"
    );
    let body = String::from_utf8_lossy(&received[0].body).to_string();
    assert_eq!(
        body.lines().next().unwrap(),
        format!("local target = {expected_literal}"),
        "escaped literal line, full body: {body}"
    );
    assert_eq!(
        body.lines().nth(1).unwrap(),
        r#"return query[[from index.tag "relation" where kind == "mention" and to == target limit 200]]"#,
        "query line must be a fixed constant regardless of page name, full body: {body}"
    );
}

#[tokio::test]
async fn links_page_name_with_an_apostrophe_does_not_break_the_query() {
    // An apostrophe never needed escaping, but this still proves the
    // apostrophe passes through untouched into a valid, unbroken query.
    assert_backlink_query_is_injection_safe("Cam's Notes", r#""Cam's Notes""#).await;
}

#[tokio::test]
async fn links_page_name_with_a_double_quote_stays_confined_to_the_literal() {
    assert_backlink_query_is_injection_safe(r#"Say "hi" Notes"#, r#""Say \"hi\" Notes""#).await;
}

#[tokio::test]
async fn links_page_name_with_a_backslash_stays_confined_to_the_literal() {
    assert_backlink_query_is_injection_safe(r"a\b\", r#""a\\b\\""#).await;
}

#[tokio::test]
async fn links_page_name_with_double_and_leveled_closing_brackets_does_not_break_the_query() {
    // This is exactly the payload that broke the old `[==[...]==]` wrapper:
    // a page name containing `]]` (would have closed a plain `[[...]]`
    // early) and `]==]` (would have closed the old leveled bracket early).
    // Neither can do anything here because the page name never touches the
    // query body at all.
    assert_backlink_query_is_injection_safe("Weird]==]Page]]end", r#""Weird]==]Page]]end""#).await;
}

#[tokio::test]
async fn links_page_name_with_a_newline_does_not_break_the_query() {
    // An unescaped literal newline inside a Lua double-quoted string is a
    // syntax error, so this must come through as the two-character escape
    // `\n`, keeping the whole script on its original two lines.
    assert_backlink_query_is_injection_safe("Line one\nLine two", r#""Line one\nLine two""#).await;
}

#[tokio::test]
async fn links_page_name_with_a_control_character_does_not_break_the_query() {
    assert_backlink_query_is_injection_safe("Control\u{7}Char", r#""Control\007Char""#).await;
}

#[tokio::test]
async fn links_runtime_unavailable_prints_docs_link() {
    let dir = setup_space("http://localhost:19999", false);
    sb_in(dir.path())
        .args(["links", "Some Page"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "https://silverbullet.md/Runtime%20API",
        ));
}

// ---------------- sb inbox ----------------

fn at_mention_fixture_body() -> String {
    r#"{"result":[{"kind":"at-mention","tag":"relation","to":"@cam","from":"Some Page","snippet":"@cam please review"}]}"#.to_string()
}

#[tokio::test]
async fn inbox_to_cam_and_to_at_cam_send_the_identical_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .and(body_string_contains(r#"kind == "at-mention""#))
        .and(body_string_contains(r#"local target = "@cam""#))
        .and(body_string_contains("to == target"))
        .respond_with(ResponseTemplate::new(200).set_body_string(at_mention_fixture_body()))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args(["--format", "json", "inbox", "--to", "cam"])
        .assert()
        .success();
    sb_in(dir.path())
        .args(["--format", "json", "inbox", "--to", "@cam"])
        .assert()
        .success();

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    let bodies: Vec<String> = received
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .collect();
    assert_eq!(
        bodies[0], bodies[1],
        "--to cam and --to @cam must produce the identical query body"
    );
}

#[tokio::test]
async fn inbox_falls_back_to_configured_identity_when_to_is_omitted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .and(body_string_contains(r#"local target = "@cam""#))
        .and(body_string_contains("to == target"))
        .respond_with(ResponseTemplate::new(200).set_body_string(at_mention_fixture_body()))
        .mount(&server)
        .await;
    // `identity` must come before the `[runtime]` table -- appending it
    // after would make TOML parse it as `runtime.identity` instead.
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        format!(
            "server_url = \"{}\"\ntoken = \"testtoken\"\nidentity = \"@cam\"\n\n[runtime]\navailable = true\n",
            server.uri()
        ),
    )
    .unwrap();

    sb_in(dir.path())
        .args(["--format", "json", "inbox"])
        .assert()
        .success();
}

#[tokio::test]
async fn inbox_missing_identity_and_no_to_flag_is_a_usage_error() {
    let dir = setup_space("http://localhost:19999", true);

    sb_in(dir.path())
        .args(["inbox"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--to"));
}

#[tokio::test]
async fn inbox_empty_result_prints_note_on_stderr_and_empty_array_on_stdout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":[]}"#))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args(["--format", "json", "inbox", "--to", "nobody"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[]"))
        .stderr(predicate::str::contains("No mentions."));
}

#[tokio::test]
async fn inbox_renders_the_at_mention_snippet_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(at_mention_fixture_body()))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args(["--format", "json", "inbox", "--to", "cam"])
        .assert()
        .success()
        .stdout(predicate::str::contains("@cam please review"));
}

/// Same injection-safety property as `assert_backlink_query_is_injection_safe`,
/// for `sb inbox --to`, which takes its value straight off argv rather than a
/// page name read from the space's own file listing.
async fn assert_inbox_query_is_injection_safe(to: &str, expected_literal: &str) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result":[]}"#))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    sb_in(dir.path())
        .args(["--format", "json", "inbox", "--to", to])
        .assert()
        .success();

    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "exactly one request; the identity must not have broken the query"
    );
    let body = String::from_utf8_lossy(&received[0].body).to_string();
    assert_eq!(
        body.lines().next().unwrap(),
        format!("local target = {expected_literal}"),
        "escaped literal line, full body: {body}"
    );
    assert_eq!(
        body.lines().nth(1).unwrap(),
        r#"local rows = query[[from index.tag "relation" where kind == "at-mention" and to == target]]"#,
        "query line must be a fixed constant regardless of identity, full body: {body}"
    );
}

#[tokio::test]
async fn inbox_to_with_a_double_quote_and_leveled_closing_bracket_stays_confined_to_the_literal() {
    // `--to` comes straight from argv (or config), unlike a page name that's
    // at least drawn from the space's own file listing when picked
    // interactively -- so this is the more directly attacker-reachable path.
    assert_inbox_query_is_injection_safe(
        r#"@cam]==]" .. (os.execute or print)("pwn")"#,
        r#""@cam]==]\" .. (os.execute or print)(\"pwn\")""#,
    )
    .await;
}
