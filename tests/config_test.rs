use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;

/// Helper: create a temp dir with `.sb/config.toml` containing the given TOML content.
fn setup_config_dir(toml_content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let sb_dir = dir.path().join(".sb");
    fs::create_dir_all(&sb_dir).expect("create .sb dir");
    fs::write(sb_dir.join("config.toml"), toml_content).expect("write config.toml");
    dir
}

#[test]
fn config_show_displays_file_values() {
    let dir = setup_config_dir(
        r#"
server_url = "https://sb.test"
token = "secret123"
"#,
    );

    Command::cargo_bin("sb")
        .unwrap()
        .args(["config", "show"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains(r#"server_url = "https://sb.test""#)
                .and(predicate::str::contains("# (config)"))
                .and(predicate::str::contains("sec...123")),
        );
}

#[test]
fn config_show_reveal_unmasks_token() {
    let dir = setup_config_dir(
        r#"
server_url = "https://sb.test"
token = "secret123"
"#,
    );

    Command::cargo_bin("sb")
        .unwrap()
        .args(["config", "show", "--reveal"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("secret123"));
}

#[test]
fn config_show_env_override() {
    let dir = setup_config_dir("");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["config", "show"])
        .current_dir(dir.path())
        .env("SB_SYNC_WORKERS", "8")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("workers = 8")
                .and(predicate::str::contains("# (env: SB_SYNC_WORKERS)")),
        );
}

#[test]
fn config_show_defaults_when_no_config() {
    let dir = tempfile::tempdir().expect("create tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .args(["config", "show"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("workers = 4")
                .and(predicate::str::contains("# (default)"))
                .and(predicate::str::contains("attachments = false")),
        );
}

#[test]
fn config_show_json_format() {
    let dir = setup_config_dir(
        r#"
server_url = "https://sb.test"
token = "secret123"
"#,
    );

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--format", "json", "config", "show"])
        .current_dir(dir.path())
        .output()
        .expect("run command");

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("valid utf8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");

    // Verify structure
    assert!(json["server"]["server_url"]["source"].is_string());
    assert!(json["server"]["token"]["source"].is_string());
    assert!(json["sync"]["workers"]["value"].is_number());
    assert_eq!(json["server"]["server_url"]["value"], "https://sb.test");
    assert_eq!(json["server"]["server_url"]["source"], "config");

    // Token should be masked by default
    assert_eq!(json["server"]["token"]["value"], "sec...123");
}

#[test]
fn config_show_json_reveal_shows_full_token() {
    let dir = setup_config_dir(
        r#"
token = "mysecrettoken"
"#,
    );

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--format", "json", "config", "show", "--reveal"])
        .current_dir(dir.path())
        .output()
        .expect("run command");

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("valid utf8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");

    assert_eq!(json["server"]["token"]["value"], "mysecrettoken");
}

#[test]
fn config_show_quiet_produces_no_output() {
    let dir = setup_config_dir(
        r#"
server_url = "https://sb.test"
"#,
    );

    Command::cargo_bin("sb")
        .unwrap()
        .args(["--quiet", "config", "show"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}
