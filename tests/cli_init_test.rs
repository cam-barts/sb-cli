use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Start a MockServer and mount a GET /.ping -> 200 "OK" handler.
async fn mock_ping_ok(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/.ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
        .mount(server)
        .await;
}

/// Start a MockServer and mount a GET /.ping -> 500 handler.
async fn mock_ping_fail(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/.ping"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Error"))
        .mount(server)
        .await;
}

#[tokio::test]
async fn init_creates_sb_directory_and_files() {
    let server = MockServer::start().await;
    mock_ping_ok(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &server.uri(), "--token", "testtoken"])
        .current_dir(temp_dir.path())
        .assert()
        .success();

    // .sb/ directory created
    assert!(
        temp_dir.path().join(".sb").is_dir(),
        ".sb/ directory should exist"
    );

    // config.toml created
    let config_path = temp_dir.path().join(".sb").join("config.toml");
    assert!(config_path.exists(), ".sb/config.toml should exist");

    // state.db created
    let state_db_path = temp_dir.path().join(".sb").join("state.db");
    assert!(state_db_path.exists(), ".sb/state.db should exist");

    // config.toml contains server_url
    let content = fs::read_to_string(&config_path).expect("read config.toml");
    assert!(
        content.contains(&server.uri()),
        "config.toml should contain server_url; got:\n{content}"
    );

    // config.toml contains token
    assert!(
        content.contains("testtoken"),
        "config.toml should contain token; got:\n{content}"
    );
}

#[tokio::test]
async fn init_writes_server_url_to_config_toml() {
    let server = MockServer::start().await;
    mock_ping_ok(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &server.uri(), "--token", "testtoken"])
        .current_dir(temp_dir.path())
        .assert()
        .success();

    let content = fs::read_to_string(temp_dir.path().join(".sb").join("config.toml"))
        .expect("read config.toml");
    assert!(
        content.contains("server_url"),
        "config.toml should contain server_url key"
    );
    assert!(
        content.contains(&server.uri()),
        "config.toml should contain the server URL"
    );
}

#[tokio::test]
async fn init_without_token_flag_does_not_write_token() {
    let server = MockServer::start().await;
    // SB_TOKEN env var will resolve for the ping, so we need the mock
    mock_ping_ok(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    // Pass SB_TOKEN env — it should NOT be persisted to config.toml (T-02-07)
    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &server.uri()])
        .current_dir(temp_dir.path())
        .env("SB_TOKEN", "envtoken")
        .assert()
        .success();

    let content = fs::read_to_string(temp_dir.path().join(".sb").join("config.toml"))
        .expect("read config.toml");
    assert!(
        content.contains("server_url"),
        "config.toml should contain server_url"
    );
    assert!(
        !content.contains("token"),
        "config.toml should NOT contain token field when only SB_TOKEN env was set; got:\n{content}"
    );
}

#[tokio::test]
async fn init_failed_ping_removes_sb_directory() {
    let server = MockServer::start().await;
    mock_ping_fail(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &server.uri(), "--token", "testtoken"])
        .current_dir(temp_dir.path())
        .assert()
        .failure()
        .code(1);

    // .sb/ should be completely removed after failed ping
    assert!(
        !temp_dir.path().join(".sb").exists(),
        ".sb/ directory should be removed after failed ping"
    );
}

#[tokio::test]
async fn init_failed_ping_exits_with_code_1() {
    let server = MockServer::start().await;
    mock_ping_fail(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &server.uri(), "--token", "testtoken"])
        .current_dir(temp_dir.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("500").or(predicate::str::contains("server")));
}

#[tokio::test]
async fn init_already_initialized_aborts() {
    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    // Manually create .sb/ to simulate already-initialized state
    fs::create_dir_all(temp_dir.path().join(".sb")).expect("create .sb dir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", "http://example.com", "--token", "t"])
        .current_dir(temp_dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("already initialized"));
}

#[tokio::test]
async fn init_state_db_is_valid_sqlite() {
    let server = MockServer::start().await;
    mock_ping_ok(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &server.uri(), "--token", "testtoken"])
        .current_dir(temp_dir.path())
        .assert()
        .success();

    let db_path = temp_dir.path().join(".sb").join("state.db");
    let conn = Connection::open(&db_path).expect("state.db should be a valid SQLite database");

    // Verify WAL mode
    let mode: String = conn
        .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
        .expect("query journal_mode");
    assert_eq!(mode, "wal", "state.db should use WAL journal mode");
}

#[tokio::test]
async fn init_normalizes_trailing_slash_in_url() {
    let server = MockServer::start().await;
    mock_ping_ok(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    let url_with_slash = format!("{}/", server.uri());

    Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &url_with_slash, "--token", "testtoken"])
        .current_dir(temp_dir.path())
        .assert()
        .success();

    let content = fs::read_to_string(temp_dir.path().join(".sb").join("config.toml"))
        .expect("read config.toml");
    assert!(
        !content.contains("//\""),
        "config.toml should not end URL with trailing slash; got:\n{content}"
    );
    // The stored URL should be the server URI without trailing slash
    let expected_url = server.uri();
    assert!(
        content.contains(&expected_url),
        "config.toml should contain normalized URL {expected_url}; got:\n{content}"
    );
}

#[tokio::test]
async fn init_http_url_warns_about_insecure() {
    let server = MockServer::start().await;
    mock_ping_ok(&server).await;

    let temp_dir = tempfile::TempDir::new().expect("create tempdir");

    // MockServer always uses http:// so this test inherently uses an HTTP URL
    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["init", &server.uri(), "--token", "testtoken"])
        .current_dir(temp_dir.path())
        .output()
        .expect("run command");

    let stderr = String::from_utf8(output.stderr).expect("valid utf8");
    assert!(
        stderr.contains("HTTP") || stderr.contains("http"),
        "stderr should warn about insecure HTTP connection; got:\n{stderr}"
    );
    assert!(
        stderr.contains("not encrypted") || stderr.contains("HTTPS"),
        "stderr should mention encryption or HTTPS; got:\n{stderr}"
    );
}
