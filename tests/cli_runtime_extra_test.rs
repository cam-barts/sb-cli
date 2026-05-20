use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mirror of the helper in cli_runtime_test.rs -- creates a temp space with
/// `.sb/config.toml` pointing at `server_url` and optionally records
/// runtime.available = true.
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

// ---------------- sb logs ----------------

#[tokio::test]
async fn logs_human_format_renders_entries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.runtime/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"clientLogs":[{"level":"log","message":"hello-client","timestamp":1700000000000}],"serverLogs":[{"level":"error","message":"boom-server","timestamp":1700000001000}]}"#,
        ))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "human", "logs"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("hello-client")
                .and(predicate::str::contains("boom-server"))
                .and(predicate::str::contains("[client]"))
                .and(predicate::str::contains("[server]")),
        );
}

#[tokio::test]
async fn logs_ndjson_format_emits_one_entry_per_line() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.runtime/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"clientLogs":[{"level":"log","message":"a","timestamp":1}],"serverLogs":[{"level":"warn","message":"b","timestamp":2}]}"#,
        ))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    let output = Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "json", "logs"])
        .current_dir(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("utf8");
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "expected 2 ndjson lines, got: {text:?}");
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("each line is valid JSON");
        assert!(v.get("stream").is_some());
        assert!(v.get("message").is_some());
    }
}

#[tokio::test]
async fn logs_source_client_filters_out_server_entries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.runtime/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"clientLogs":[{"level":"log","message":"only-client","timestamp":1}],"serverLogs":[{"level":"warn","message":"hidden-server","timestamp":2}]}"#,
        ))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "human", "logs", "--source", "client"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("only-client")
                .and(predicate::str::contains("hidden-server").not()),
        );
}

#[tokio::test]
async fn logs_503_maps_to_runtime_unavailable_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.runtime/logs"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["logs"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "https://silverbullet.md/Runtime%20API",
        ));
}

// ---------------- sb screenshot ----------------

#[tokio::test]
async fn screenshot_writes_png_to_specified_path() {
    let server = MockServer::start().await;
    let png = b"\x89PNG\r\n\x1a\nfake-bytes-here".to_vec();
    Mock::given(method("GET"))
        .and(path("/.runtime/screenshot"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "image/png")
                .set_body_bytes(png.clone()),
        )
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);
    let out_path = dir.path().join("shot.png");

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args([
            "--format",
            "human",
            "screenshot",
            "--output",
            out_path.to_str().unwrap(),
        ])
        .current_dir(dir.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("Saved screenshot"));

    let written = fs::read(&out_path).expect("output file should exist");
    assert_eq!(&written[..8], &png[..8], "PNG magic bytes preserved");
    assert_eq!(written.len(), png.len());
}

#[tokio::test]
async fn screenshot_dash_writes_png_to_stdout() {
    let server = MockServer::start().await;
    let png = b"\x89PNG\r\n\x1a\nstdout-bytes".to_vec();
    Mock::given(method("GET"))
        .and(path("/.runtime/screenshot"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "image/png")
                .set_body_bytes(png.clone()),
        )
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    let out = Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "human", "screenshot", "--output", "-"])
        .current_dir(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(&out[..8], &png[..8], "stdout should contain PNG magic");
    assert_eq!(out.len(), png.len());
}

#[tokio::test]
async fn screenshot_json_format_emits_envelope_with_path() {
    let server = MockServer::start().await;
    let png = b"\x89PNG\r\n\x1a\njson-mode".to_vec();
    Mock::given(method("GET"))
        .and(path("/.runtime/screenshot"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "image/png")
                .set_body_bytes(png.clone()),
        )
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);
    let out_path = dir.path().join("envelope.png");

    let stdout = Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args([
            "--format",
            "json",
            "screenshot",
            "--output",
            out_path.to_str().unwrap(),
        ])
        .current_dir(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value =
        serde_json::from_slice(&stdout).expect("json envelope should parse");
    assert_eq!(parsed["bytes"], png.len());
    assert!(parsed["path"].as_str().unwrap().contains("envelope.png"));
    assert!(out_path.exists(), "file written to disk");
}

#[tokio::test]
async fn screenshot_json_with_stdout_dash_is_usage_error() {
    let dir = setup_space("http://localhost:19999", true);
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "json", "screenshot", "--output", "-"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(2);
}

// ---------------- sb describe ----------------

#[tokio::test]
async fn describe_renders_field_table_in_human_format() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .and(body_string_contains(r#"tag "task""#))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"result":{"tag":"task","sampled":3,"fields":{"name":{"string":3},"done":{"boolean":2,"nil":1}}}}"#,
        ))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "human", "describe", "task"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Tag: task")
                .and(predicate::str::contains("name"))
                .and(predicate::str::contains("done"))
                .and(predicate::str::contains("boolean(2)")),
        );
}

#[tokio::test]
async fn describe_json_format_returns_structured_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"result":{"tag":"page","sampled":1,"fields":{"name":{"string":1}}}}"#,
        ))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    let stdout = Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "json", "describe", "page"])
        .current_dir(dir.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&stdout).expect("json output");
    assert_eq!(v["tag"], "page");
    assert_eq!(v["sampled"], 1);
    assert!(v["fields"]["name"].is_array());
}

#[tokio::test]
async fn describe_rejects_tag_with_unsafe_characters() {
    let dir = setup_space("http://localhost:19999", true);
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["describe", "foo\"bar"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(2);
}

#[tokio::test]
async fn describe_sends_lua_script_with_tag_and_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.runtime/lua_script"))
        .and(body_string_contains(r#"tag "template""#))
        .and(body_string_contains("limit 7"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"result":{"tag":"template","sampled":0,"fields":{}}}"#),
        )
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri(), true);

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--format", "json", "describe", "template", "--limit", "7"])
        .current_dir(dir.path())
        .assert()
        .success();

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "exactly one lua_script request");
}

#[tokio::test]
async fn describe_runtime_unavailable_prints_docs_link() {
    let dir = setup_space("http://localhost:19999", false);
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["describe", "task"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "https://silverbullet.md/Runtime%20API",
        ));
}
