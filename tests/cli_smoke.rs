use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_subcommand_prints_version_info() {
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
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
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("sb "));
}

#[test]
fn unknown_subcommand_or_flag_exits_2() {
    for arg in ["nonexistent", "--nonexistent-flag"] {
        Command::cargo_bin("sb")
            .unwrap()
            .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
            .arg(arg)
            .assert()
            .code(2);
    }
}

#[test]
fn quiet_flag_suppresses_version_output() {
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["--quiet", "version"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn no_subcommand_shows_help() {
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage:"));
}
