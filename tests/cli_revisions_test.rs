/// Integration tests for `sb page history|diff|restore` — git-backed page
/// revisions served over `/.revisions`.
///
/// Uses assert_cmd to run the real binary and assert on exit code,
/// stdout/stderr, and filesystem side effects. Uses wiremock for HTTP.
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{any, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a space with `sync.dir = "."` (content lives at the space root) and
/// a page file, pointed at `server_url`.
fn setup_space(server_url: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
    std::fs::write(
        sb_dir.join("config.toml"),
        format!("server_url = \"{server_url}\"\ntoken = \"test-token\"\n[sync]\ndir = \".\"\n"),
    )
    .expect("write config.toml");
    dir
}

fn sb_in(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("sb").expect("sb binary");
    cmd.current_dir(dir.path())
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg");
    cmd
}

const REV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

// ---------------------------------------------------------------------------
// sb page history
// ---------------------------------------------------------------------------

#[tokio::test]
async fn history_renders_a_revision_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"mode":"managed","uncommitted":false,"more":false,"revisions":[{{"rev":"{REV_A}","timestamp":1700000000000,"author":"alice","message":"edit note","added":1,"removed":0}}]}}"#
        )))
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "content").unwrap();

    sb_in(&space)
        .args(["page", "history", "Note", "--format", "human"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alice"))
        .stdout(predicate::str::contains("edit note"));
}

#[tokio::test]
async fn history_json_shape_is_stable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"mode":"managed","uncommitted":true,"more":false,"revisions":[{{"rev":"{REV_A}","timestamp":1700000000000,"author":"alice","message":"edit","added":1,"removed":0}}]}}"#
        )))
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "content").unwrap();

    let output = sb_in(&space)
        .args(["page", "history", "Note", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(json["mode"], "managed");
    assert_eq!(json["uncommitted"], true);
    assert_eq!(json["more"], false);
    assert_eq!(json["revisions"][0]["rev"], REV_A);
    assert_eq!(json["revisions"][0]["author"], "alice");
}

#[tokio::test]
async fn history_on_an_enabled_but_empty_space_renders_cleanly() {
    // Mirrors the real target server: mode "unmanaged", no commits yet.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"mode":"unmanaged","more":false,"revisions":[],"uncommitted":true}"#,
        ))
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "content").unwrap();

    sb_in(&space)
        .args(["page", "history", "Note", "--format", "human"])
        .assert()
        .success()
        .stderr(predicate::str::contains("error").not());
}

#[tokio::test]
async fn history_when_revisions_disabled_is_a_friendly_error_not_a_raw_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .respond_with(
            ResponseTemplate::new(404).set_body_string(r#"{"error": "revisions disabled"}"#),
        )
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "content").unwrap();

    sb_in(&space)
        .args(["page", "history", "Note", "--format", "human"])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains("not enabled"))
        .stderr(predicate::str::contains("404").not());
}

// ---------------------------------------------------------------------------
// sb page diff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn diff_with_rev_sends_rev_and_format_query_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .and(query_param("format", "diff"))
        .and(query_param("rev", REV_A))
        .respond_with(ResponseTemplate::new(200).set_body_string("@@ -1 +1 @@\n-old\n+new\n"))
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "content").unwrap();

    sb_in(&space)
        .args(["page", "diff", "Note", "--rev", REV_A, "--format", "human"])
        .assert()
        .success()
        .stdout(predicate::str::contains("@@"));
}

#[tokio::test]
async fn diff_without_rev_is_the_uncommitted_change_and_omits_rev_param() {
    let server = MockServer::start().await;
    // Constraining the mock to format=diff with no rev param IS the assertion
    // that the uncommitted-diff path omits `rev` entirely.
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .and(query_param("format", "diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string("+uncommitted change\n"))
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "content").unwrap();

    sb_in(&space)
        .args(["page", "diff", "Note", "--format", "human"])
        .assert()
        .success()
        .stdout(predicate::str::contains("uncommitted change"));
}

#[tokio::test]
async fn diff_short_rev_exits_2_with_no_http_request_at_all() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "content").unwrap();

    sb_in(&space)
        .args([
            "page",
            "diff",
            "Note",
            "--rev",
            "not-a-hash",
            "--format",
            "human",
        ])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("40-character"));
    // MockServer verifies expect(0) on drop -- no request was ever sent.
}

// ---------------------------------------------------------------------------
// sb page restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restore_without_yes_or_force_exits_6_and_leaves_file_untouched() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    let page_path = space.path().join("Note.md");
    std::fs::write(&page_path, "original content").unwrap();

    sb_in(&space)
        .args([
            "page", "restore", "Note", "--rev", REV_A, "--format", "human",
        ])
        .assert()
        .failure()
        .code(6);

    let content = std::fs::read_to_string(&page_path).unwrap();
    assert_eq!(
        content, "original content",
        "restore must not touch the file without confirmation"
    );
    // MockServer verifies expect(0) on drop -- confirmation is required
    // before any network call.
}

#[tokio::test]
async fn restore_with_force_writes_local_file_and_sends_no_put() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .and(query_param("rev", REV_A))
        .respond_with(ResponseTemplate::new(200).set_body_string("restored old content"))
        .mount(&server)
        .await;
    // The hard requirement: restore writes locally and stops. It must NEVER
    // PUT to the server -- that would bypass sync's conflict handling.
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    let page_path = space.path().join("Note.md");
    std::fs::write(&page_path, "current content").unwrap();

    sb_in(&space)
        .args([
            "page", "restore", "Note", "--rev", REV_A, "--force", "--format", "human",
        ])
        .assert()
        .success();

    let content = std::fs::read_to_string(&page_path).unwrap();
    assert_eq!(content, "restored old content");
    // MockServer verifies expect(0) PUTs on drop.
}

#[tokio::test]
async fn restore_short_rev_exits_2_with_no_http_request_at_all() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    let page_path = space.path().join("Note.md");
    std::fs::write(&page_path, "original content").unwrap();

    sb_in(&space)
        .args(["page", "restore", "Note", "--rev", "deadbeef", "--force"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("40-character"));

    let content = std::fs::read_to_string(&page_path).unwrap();
    assert_eq!(content, "original content");
}

#[tokio::test]
async fn restore_with_force_reports_stable_json_shape() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.revisions/Note.md"))
        .and(query_param("rev", REV_A))
        .respond_with(ResponseTemplate::new(200).set_body_string("old content"))
        .mount(&server)
        .await;

    let space = setup_space(&server.uri());
    std::fs::write(space.path().join("Note.md"), "current content").unwrap();

    let output = sb_in(&space)
        .args([
            "page", "restore", "Note", "--rev", REV_A, "--force", "--format", "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&output).expect("valid JSON");
    assert_eq!(json["restored"], true);
    assert_eq!(json["name"], "Note");
    assert_eq!(json["rev"], REV_A);
}
