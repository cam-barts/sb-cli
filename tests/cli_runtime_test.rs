use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Create a temp dir with .sb/config.toml and .sb/state.db for testing.
/// `runtime_available`: if true, adds `[runtime]\navailable = true` to config.
fn setup_space(server_url: &str, runtime_available: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    fs::create_dir_all(&sb_dir).unwrap();
    let mut config = format!("server_url = \"{server_url}\"\ntoken = \"testtoken\"\n");
    if runtime_available {
        config.push_str("\n[runtime]\navailable = true\n");
    }
    fs::write(sb_dir.join("config.toml"), config).unwrap();
    let conn = Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    dir
}

#[tokio::test]
async fn lua_eval_returns_result_on_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result": 42}"#))
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .args(["lua", "2 + 2"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[tokio::test]
async fn lua_eval_reports_lua_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"error": "attempt to index nil"}"#),
        )
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .args(["lua", "nil.foo"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1);
}

#[tokio::test]
async fn lua_unavailable_prints_docs_link() {
    // No [runtime] section means runtime_available = false (default)
    let dir = setup_space("http://localhost:19999", false);

    Command::cargo_bin("sb")
        .unwrap()
        .args(["lua", "1 + 1"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "https://silverbullet.md/Runtime%20API",
        ));
}

#[tokio::test]
async fn query_returns_table_format() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"result": [{"name": "index", "size": 100}, {"name": "TODO", "size": 50}]}"#,
        ))
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .args(["query", "from tags.page limit 2"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("name")
                .and(predicate::str::contains("index"))
                .and(predicate::str::contains("-+-")),
        );
}

#[tokio::test]
async fn query_returns_json_format() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"result": [{"name": "index", "size": 100}, {"name": "TODO", "size": 50}]}"#,
        ))
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--format", "json", "query", "from tags.page limit 2"])
        .current_dir(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value =
        serde_json::from_slice(&output).expect("output should be valid JSON");
    assert!(parsed.is_array(), "output should be a JSON array");
}

#[tokio::test]
async fn query_unavailable_prints_docs_link() {
    // No [runtime] section means runtime_available = false
    let dir = setup_space("http://localhost:19999", false);

    Command::cargo_bin("sb")
        .unwrap()
        .args(["query", "from tags.page"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "https://silverbullet.md/Runtime%20API",
        ));
}

#[tokio::test]
async fn query_sends_correct_lua_script() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .and(body_string_contains(
            "return query[[from tags.page limit 2]]",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result": []}"#))
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .args(["query", "from tags.page limit 2"])
        .current_dir(dir.path())
        .assert()
        .success();

    // Verify the mock was called (body_string_contains matcher ensures correct body)
    let received = server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "expected exactly one request to the server"
    );
}

#[tokio::test]
async fn query_with_non_array_result_falls_back_to_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result": {"key": "value"}}"#))
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .args(["query", "some_scalar_expr"])
        .current_dir(dir.path())
        .assert()
        .success()
        // Should output JSON (not table) and notify on stderr
        .stdout(predicate::str::contains("key"))
        .stderr(predicate::str::contains("not an array"));
}
