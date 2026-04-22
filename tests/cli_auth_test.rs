use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::path::Path;

/// Create a temp dir with .sb/config.toml and .sb/state.db for testing
fn setup_initialized_space(temp_dir: &Path, server_url: &str, token: &str) {
    let sb_dir = temp_dir.join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    let config = format!("server_url = \"{server_url}\"\ntoken = \"{token}\"\n");
    std::fs::write(sb_dir.join("config.toml"), config).unwrap();
    let conn = Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
}

#[tokio::test]
async fn auth_set_with_flag_updates_config() {
    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    setup_initialized_space(temp_dir.path(), "http://example.com", "oldtoken");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["auth", "set", "--token", "newtoken"])
        .current_dir(temp_dir.path())
        .assert()
        .success();

    let config_path = temp_dir.path().join(".sb").join("config.toml");
    let content = std::fs::read_to_string(&config_path).expect("read config.toml");
    assert!(
        content.contains("newtoken"),
        "config.toml should contain new token; got:\n{content}"
    );
    assert!(
        content.contains("http://example.com"),
        "config.toml should still contain server_url; got:\n{content}"
    );
    assert!(
        !content.contains("oldtoken"),
        "config.toml should not contain old token; got:\n{content}"
    );
}

#[tokio::test]
async fn auth_set_piped_stdin() {
    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    setup_initialized_space(temp_dir.path(), "http://example.com", "oldtoken");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["auth", "set"])
        .current_dir(temp_dir.path())
        .write_stdin("stdintoken\n")
        .assert()
        .success();

    let config_path = temp_dir.path().join(".sb").join("config.toml");
    let content = std::fs::read_to_string(&config_path).expect("read config.toml");
    assert!(
        content.contains("stdintoken"),
        "config.toml should contain stdin token; got:\n{content}"
    );
    assert!(
        content.contains("http://example.com"),
        "config.toml should still contain server_url; got:\n{content}"
    );
}

#[tokio::test]
async fn auth_set_uninitialized_fails() {
    let temp_dir = tempfile::TempDir::new().expect("create tempdir");
    let xdg_dir = tempfile::TempDir::new().expect("create xdg tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["auth", "set", "--token", "t"])
        .current_dir(temp_dir.path())
        .env("XDG_CONFIG_HOME", xdg_dir.path())
        .env_remove("SB_SPACE")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("not initialized"));
}
