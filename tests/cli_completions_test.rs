use assert_cmd::Command;
use predicates::prelude::*;

/// `sb completions zsh` prints a zsh completion script (defines `_sb`) to stdout.
#[test]
fn completions_zsh_prints_script() {
    Command::cargo_bin("sb")
        .unwrap()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("#compdef sb").or(predicate::str::contains("_sb")));
}

/// `sb completions bash` prints a bash completion script mentioning the binary.
#[test]
fn completions_bash_prints_script() {
    Command::cargo_bin("sb")
        .unwrap()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sb"));
}

/// `sb completions` without a shell and without --install is a usage error.
#[test]
fn completions_without_shell_errors() {
    Command::cargo_bin("sb")
        .unwrap()
        .arg("completions")
        .assert()
        .failure()
        .stderr(predicate::str::contains("specify a shell"));
}

/// `sb completions zsh --install` writes `_sb` to the XDG data location.
#[test]
fn completions_install_zsh_writes_underscore_sb() {
    let home = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("data tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", data.path())
        .args(["completions", "zsh", "--install"])
        .assert()
        .success();

    let installed = data.path().join("zsh/site-functions/_sb");
    assert!(
        installed.is_file(),
        "expected completion script at {}",
        installed.display()
    );
    let body = std::fs::read_to_string(&installed).unwrap();
    assert!(body.contains("_sb"), "installed script should define _sb");
}

/// `sb template --help` lists the list/new subcommands.
#[test]
fn template_help_lists_subcommands() {
    Command::cargo_bin("sb")
        .unwrap()
        .args(["template", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("list").and(predicate::str::contains("new")));
}
