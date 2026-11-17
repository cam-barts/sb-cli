use assert_cmd::Command;
use predicates::prelude::*;

/// `sb man` prints the roff source for `sb(1)` to stdout.
#[test]
fn man_prints_roff_for_sb() {
    Command::cargo_bin("sb")
        .unwrap()
        .arg("man")
        .assert()
        .success()
        .stdout(
            predicate::str::contains(".TH sb 1")
                .and(predicate::str::contains(".SH NAME"))
                // after_help (exit-code table) must carry into the page
                .and(predicate::str::contains("Exit codes:")),
        );
}

/// `sb man --install` writes `sb.1` plus a page per subcommand (including
/// nested ones) into the XDG man1 directory.
#[test]
fn man_install_writes_sb_1_and_nested_pages() {
    let home = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("data tempdir");

    Command::cargo_bin("sb")
        .unwrap()
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", data.path())
        .args(["man", "--install"])
        .assert()
        .success();

    let man1 = data.path().join("man/man1");
    for page in ["sb.1", "sb-page.1", "sb-page-read.1"] {
        let path = man1.join(page);
        assert!(path.is_file(), "expected man page at {}", path.display());
    }
    let body = std::fs::read_to_string(man1.join("sb-page-read.1")).unwrap();
    assert!(
        body.contains(".TH sb-page-read 1"),
        "nested page should carry its own title"
    );
}
