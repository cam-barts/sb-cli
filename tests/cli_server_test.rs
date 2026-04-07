use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::path::Path;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Create a temp dir with .sb/config.toml and .sb/state.db for testing
fn setup_initialized_space(temp_dir: &Path, server_url: &str, token: &str) {
    let sb_dir = temp_dir.join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    let config = format!("server_url = \"{server_url}\"\ntoken = \"{token}\"\n");
    std::fs::write(sb_dir.join("config.toml"), config).unwrap();
    // Create empty state.db with WAL mode
    let conn = Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
}

#[tokio::test]
async fn server_ping_reports_reachability() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
        .mount(&server)
        .await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    setup_initialized_space(temp_dir.path(), &server.uri(), "testtoken");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["server", "ping"])
        .current_dir(temp_dir.path())
        .assert()
        .success()
        .stderr(
            predicate::str::contains("reachable")
                .or(predicate::str::contains("Reachable"))
                .and(predicate::str::contains("ms")),
        );
}

#[tokio::test]
async fn server_ping_uninitialized_fails() {
    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["server", "ping"])
        .current_dir(temp_dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("not initialized"));
}

#[tokio::test]
async fn server_config_displays_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.config"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"readOnly":false,"spaceFolderPath":"/data/space","indexPage":"index","logPush":false,"enableClientEncryption":true}"#,
            ),
        )
        .mount(&server)
        .await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    setup_initialized_space(temp_dir.path(), &server.uri(), "testtoken");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["server", "config"])
        .current_dir(temp_dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Read-only")
                .and(predicate::str::contains("Space folder"))
                .and(predicate::str::contains("/data/space")),
        );
}

#[tokio::test]
async fn server_config_json_format() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.config"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                r#"{"readOnly":false,"spaceFolderPath":"/data/space","indexPage":"index","logPush":false,"enableClientEncryption":false}"#,
            ),
        )
        .mount(&server)
        .await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    setup_initialized_space(temp_dir.path(), &server.uri(), "testtoken");

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["server", "config", "--format", "json"])
        .current_dir(temp_dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("valid utf8");
    // Validate it's valid JSON with expected key
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("should be valid JSON");
    assert!(
        parsed.get("spaceFolderPath").is_some(),
        "JSON should contain spaceFolderPath; got:\n{stdout}"
    );
}

#[tokio::test]
async fn server_ping_json_format() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
        .mount(&server)
        .await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    setup_initialized_space(temp_dir.path(), &server.uri(), "testtoken");

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["server", "ping", "--format", "json"])
        .current_dir(temp_dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).expect("valid utf8");
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("should be valid JSON");
    assert_eq!(
        parsed.get("reachable").and_then(|v| v.as_bool()),
        Some(true),
        "JSON should have reachable:true; got:\n{stdout}"
    );
    assert!(
        parsed.get("response_ms").is_some(),
        "JSON should contain response_ms; got:\n{stdout}"
    );
}
