mod common;

use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Create a temp dir with .sb/config.toml and .sb/state.db for testing.
fn setup_space(server_url: &str, shell_enabled: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    fs::create_dir_all(&sb_dir).unwrap();
    let mut config = format!("server_url = \"{server_url}\"\ntoken = \"testtoken\"\n");
    if shell_enabled {
        config.push_str("\n[shell]\nenabled = true\n");
    }
    fs::write(sb_dir.join("config.toml"), config).unwrap();
    let conn = Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    dir
}

#[tokio::test]
async fn shell_disabled_prints_warning() {
    // Config has shell.enabled = false (default — no [shell] section)
    let dir = setup_space("http://localhost:19999", false);

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["shell", "ls"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("arbitrary command execution"));
}

#[tokio::test]
async fn shell_enabled_sends_post_and_prints_stdout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.shell"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"stdout": "hello world\n", "stderr": "", "code": 0}"#),
        )
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["shell", "echo", "hello world"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

#[tokio::test]
async fn shell_with_a_rejected_token_exits_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.shell"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["shell", "ls"])
        .current_dir(dir.path())
        .assert()
        .code(3);
}

#[tokio::test]
async fn shell_enabled_prints_stderr() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.shell"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"stdout": "", "stderr": "not found\n", "code": 1}"#),
        )
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["shell", "badcmd"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[tokio::test]
async fn shell_no_command_returns_usage_error() {
    let dir = setup_space("http://localhost:19999", true);

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["shell"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(2);
}

#[tokio::test]
async fn shell_sends_correct_json_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.shell"))
        .and(body_json(
            serde_json::json!({"cmd": "ls", "args": ["-la", "/tmp"]}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"stdout": "total 0\n", "stderr": "", "code": 0}"#),
        )
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["shell", "ls", "-la", "/tmp"])
        .current_dir(dir.path())
        .assert()
        .success();

    // The body_json matcher ensures the correct JSON was sent
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected exactly one request");
}

#[tokio::test]
async fn shell_propagates_exit_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.shell"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"stdout": "", "stderr": "", "code": 42}"#),
        )
        .mount(&server)
        .await;

    let dir = setup_space(&server.uri(), true);

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["shell", "failing-cmd"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(42);
}

/// Run `sb --format human shell failing-cmd` against a server whose shell
/// answers with the given stderr and exit code.
async fn run_failing_shell(stderr: &str, code: i32) -> assert_cmd::assert::Assert {
    let server = MockServer::start().await;
    let body = serde_json::json!({"stdout": "", "stderr": stderr, "code": code});
    Mock::given(method("POST"))
        .and(path("/.shell"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);
    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "human", "shell", "failing-cmd"])
        .current_dir(dir.path())
        .assert()
}

#[tokio::test]
async fn shell_failure_with_empty_stderr_has_no_hint_line() {
    run_failing_shell("", 7)
        .await
        .code(7)
        .stderr(predicate::str::contains("process exited with code 7"))
        .stderr(predicate::str::contains("stderr:").not());
}

#[test]
fn token_with_invalid_header_characters_is_a_config_error_exit_1() {
    // The token is sent as a Bearer header; a newline cannot be one. This must
    // surface as a general error (exit 1) before any request is attempted.
    let dir = setup_space("http://127.0.0.1:9", true);
    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .env("SB_TOKEN", "bad\ntoken")
        .args(["--format", "human", "shell", "ls"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("invalid header characters"));
}
