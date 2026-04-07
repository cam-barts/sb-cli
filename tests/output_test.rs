use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn no_color_flag_suppresses_ansi() {
    // --no-color should produce output without ANSI escape sequences
    Command::cargo_bin("sb")
        .unwrap()
        .args(["--no-color", "version"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[").not());
}

#[test]
fn piped_output_suppresses_ansi() {
    // When piped (not a TTY), output should not contain ANSI escapes
    // assert_cmd runs in non-TTY context by default
    Command::cargo_bin("sb")
        .unwrap()
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[").not());
}

#[test]
fn no_color_env_suppresses_ansi() {
    Command::cargo_bin("sb")
        .unwrap()
        .env("NO_COLOR", "1")
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains("\x1b[").not());
}

#[test]
fn verbose_flag_produces_stderr_output() {
    // --verbose should produce debug output on stderr
    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--verbose", "version"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.is_empty(),
        "stderr should contain debug output when --verbose is used, got empty stderr"
    );
}

#[test]
fn json_format_config_show_is_valid_json() {
    let tmpdir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmpdir.path().join(".sb")).unwrap();
    std::fs::write(
        tmpdir.path().join(".sb/config.toml"),
        "server_url = \"https://test.example.com\"\n",
    )
    .unwrap();

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--format", "json", "config", "show"])
        .current_dir(tmpdir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    // Verify it's valid JSON
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("config show --format json should produce valid JSON");
    assert!(parsed.is_object());
}

#[test]
fn quiet_suppresses_all_informational_output() {
    Command::cargo_bin("sb")
        .unwrap()
        .args(["--quiet", "version"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

#[test]
fn json_format_config_show_defaults_is_valid_json() {
    // Even without any config file, --format json config show should produce valid JSON
    let tmpdir = tempfile::TempDir::new().unwrap();

    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--format", "json", "config", "show"])
        .current_dir(tmpdir.path())
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .expect("config show --format json with defaults should produce valid JSON");
    assert!(parsed.is_object());
}

#[test]
fn no_color_flag_on_stderr_suppresses_ansi() {
    // Verify stderr also has no ANSI when --no-color is set
    let output = Command::cargo_bin("sb")
        .unwrap()
        .args(["--no-color", "version"])
        .output()
        .unwrap();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("\x1b["),
        "stderr should not contain ANSI escapes with --no-color"
    );
}
