use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_subcommand_prints_version_info() {
    Command::cargo_bin("sb")
        .unwrap()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("sb "))
        .stdout(predicate::str::contains("commit:"))
        .stdout(predicate::str::contains("built:"))
        .stdout(predicate::str::contains("target:"));
}

#[test]
fn version_flag_prints_compact() {
    Command::cargo_bin("sb")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("sb "));
}

#[test]
fn unknown_subcommand_exits_2() {
    Command::cargo_bin("sb")
        .unwrap()
        .arg("nonexistent")
        .assert()
        .code(2);
}

#[test]
fn quiet_flag_suppresses_version_output() {
    Command::cargo_bin("sb")
        .unwrap()
        .args(["--quiet", "version"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn no_subcommand_shows_help() {
    Command::cargo_bin("sb")
        .unwrap()
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"));
}

#[test]
fn invalid_flag_exits_2() {
    Command::cargo_bin("sb")
        .unwrap()
        .arg("--nonexistent-flag")
        .assert()
        .code(2);
}

#[test]
fn json_format_config_show_no_config_produces_valid_json() {
    let tmpdir = tempfile::TempDir::new().unwrap();

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--format", "json", "config", "show"])
        .current_dir(tmpdir.path())
        .output()
        .expect("run command");

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("valid utf8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    assert!(json.is_object(), "JSON output should be an object");
}
