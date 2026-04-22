/// Integration tests for `sb page` subcommands.
///
/// Uses assert_cmd to run the real binary and assert on exit code, stdout, stderr.
/// Uses tempfile for isolated, auto-cleaned space directories.
use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temporary space directory with `.sb/config.toml`.
fn setup_space() -> TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
    let mut f = std::fs::File::create(sb_dir.join("config.toml")).expect("create config.toml");
    f.write_all(b"server_url = \"https://sb.example.com\"\n")
        .expect("write config.toml");
    dir
}

/// Create a page file (with .md extension) in the space root.
fn write_page(dir: &TempDir, name: &str, content: &str) {
    let page_path = dir.path().join(format!("{name}.md"));
    if let Some(parent) = page_path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(&page_path, content).expect("write page file");
}

/// Build an `sb` command rooted at the given temp directory.
fn sb_cmd(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("sb").expect("sb binary");
    cmd.current_dir(dir.path());
    cmd
}

// ---------------------------------------------------------------------------
// Task 1: page list tests (RED phase — these will fail until execute_list is wired)
// ---------------------------------------------------------------------------

#[test]
fn page_list_shows_pages_without_md_extension() {
    let dir = setup_space();
    write_page(&dir, "my-notes", "# My Notes");
    write_page(&dir, "index", "# Index");

    sb_cmd(&dir)
        .args(["page", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("my-notes"))
        .stdout(predicate::str::contains("index"))
        // Must NOT show the .md extension in output
        .stdout(predicate::str::contains(".md").not());
}

#[test]
fn page_list_excludes_sb_directory_files() {
    let dir = setup_space();
    write_page(&dir, "real-page", "content");
    // state.db lives in .sb/ -- listing should not show it even if it had .md extension
    std::fs::write(dir.path().join(".sb").join("internal.md"), "secret")
        .expect("write internal file");

    sb_cmd(&dir)
        .args(["page", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("real-page"))
        .stdout(predicate::str::contains("internal").not());
}

#[test]
fn page_list_shows_nested_pages_with_slash_separator() {
    let dir = setup_space();
    write_page(&dir, "Journal/2026-04-05", "daily note");

    sb_cmd(&dir)
        .args(["page", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Journal/2026-04-05"));
}

#[test]
fn page_list_format_json_outputs_valid_json_array() {
    let dir = setup_space();
    write_page(&dir, "alpha", "content a");
    write_page(&dir, "beta", "content b");

    let output = sb_cmd(&dir)
        .args(["page", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("valid utf8");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");

    assert!(parsed.is_array(), "output should be a JSON array");
    let arr = parsed.as_array().unwrap();
    assert!(arr.len() >= 2, "should have at least 2 entries");

    // Each entry must have 'name' and 'modified' fields
    for entry in arr {
        assert!(entry.get("name").is_some(), "entry missing 'name' field");
        assert!(
            entry.get("modified").is_some(),
            "entry missing 'modified' field"
        );
    }

    let names: Vec<&str> = arr.iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"alpha"), "JSON output should contain alpha");
    assert!(names.contains(&"beta"), "JSON output should contain beta");
}

#[test]
fn page_list_limit_restricts_output_count() {
    let dir = setup_space();
    write_page(&dir, "page-a", "content");
    write_page(&dir, "page-b", "content");
    write_page(&dir, "page-c", "content");

    let output = sb_cmd(&dir)
        .args(["page", "list", "--limit", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("valid utf8");
    // Count non-empty lines (each page is one line)
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "limit 1 should produce exactly 1 line, got: {text:?}"
    );
}

#[test]
fn page_list_sort_name_orders_alphabetically() {
    let dir = setup_space();
    write_page(&dir, "zebra", "z");
    write_page(&dir, "apple", "a");
    write_page(&dir, "mango", "m");

    let output = sb_cmd(&dir)
        .args(["page", "list", "--sort", "name"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).expect("valid utf8");
    let names: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split_whitespace().next().unwrap_or(""))
        .collect();

    // Alphabetical: apple < mango < zebra
    let apple_pos = names
        .iter()
        .position(|&n| n == "apple")
        .expect("apple present");
    let mango_pos = names
        .iter()
        .position(|&n| n == "mango")
        .expect("mango present");
    let zebra_pos = names
        .iter()
        .position(|&n| n == "zebra")
        .expect("zebra present");
    assert!(apple_pos < mango_pos, "apple should come before mango");
    assert!(mango_pos < zebra_pos, "mango should come before zebra");
}

#[test]
fn page_list_in_non_initialized_directory_returns_error() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let xdg_dir = tempfile::tempdir().expect("create xdg tempdir");
    // Isolate from any real XDG config or SB_SPACE on the developer's machine

    Command::cargo_bin("sb")
        .expect("sb binary")
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", xdg_dir.path())
        .env_remove("SB_SPACE")
        .args(["page", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not initialized").or(predicate::str::contains("no .sb")));
}

// ---------------------------------------------------------------------------
// Task 2a: page read tests
// ---------------------------------------------------------------------------

#[test]
fn page_read_outputs_file_content_to_stdout() {
    let dir = setup_space();
    write_page(&dir, "test-page", "# Hello World\n\nSome content here.");

    sb_cmd(&dir)
        .args(["page", "read", "test-page"])
        .assert()
        .success()
        .stdout(predicate::str::contains("# Hello World"))
        .stdout(predicate::str::contains("Some content here."));
}

#[test]
fn page_read_nonexistent_page_returns_error_and_exit_1() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "read", "nonexistent-page"])
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("not found").or(predicate::str::contains("nonexistent-page")),
        );
}

#[test]
fn page_read_path_traversal_is_rejected() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "read", "../../etc/passwd"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("path traversal").or(predicate::str::contains("invalid")));
}

#[test]
fn page_read_remote_fetches_from_server() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // We need tokio runtime for wiremock
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/.fs/test-page.md"))
            .and(header("X-Sync-Mode", "true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("# Remote Content\n\nFrom server."),
            )
            .mount(&server)
            .await;

        // Write config pointing to our mock server with a token
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
        let config_content = format!("server_url = \"{}\"\ntoken = \"testtoken\"\n", server.uri());
        std::fs::write(sb_dir.join("config.toml"), config_content).expect("write config");

        Command::cargo_bin("sb")
            .expect("sb binary")
            .current_dir(dir.path())
            .args(["page", "read", "--remote", "test-page"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Remote Content"));
    });
}

// ---------------------------------------------------------------------------
// Task 2b: page create tests
// ---------------------------------------------------------------------------

#[test]
fn page_create_with_content_flag_creates_file() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "create", "new-page", "--content", "hello world"])
        .assert()
        .success();

    let file_path = dir.path().join("new-page.md");
    assert!(file_path.exists(), "new-page.md should exist");
    let content = std::fs::read_to_string(&file_path).expect("read file");
    assert!(
        content.contains("hello world"),
        "file should contain the content"
    );
}

#[test]
fn page_create_duplicate_fails_with_error() {
    let dir = setup_space();
    write_page(&dir, "existing-page", "original content");

    sb_cmd(&dir)
        .args([
            "page",
            "create",
            "existing-page",
            "--content",
            "new content",
        ])
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("already exists")
                .or(predicate::str::contains("existing-page")),
        );
}

#[test]
fn page_create_nested_creates_intermediate_directories() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args([
            "page",
            "create",
            "sub/dir/nested-page",
            "--content",
            "nested content",
        ])
        .assert()
        .success();

    let file_path = dir.path().join("sub").join("dir").join("nested-page.md");
    assert!(file_path.exists(), "nested file should be created");
    let content = std::fs::read_to_string(&file_path).expect("read file");
    assert!(content.contains("nested content"));
}

#[test]
fn page_create_reads_content_from_stdin() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "create", "stdin-page"])
        .write_stdin("piped content from stdin")
        .assert()
        .success();

    let file_path = dir.path().join("stdin-page.md");
    assert!(file_path.exists(), "stdin-page.md should exist");
    let content = std::fs::read_to_string(&file_path).expect("read file");
    assert!(content.contains("piped content from stdin"));
}

#[test]
fn page_create_path_traversal_is_rejected() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "create", "../../etc/evil", "--content", "attack"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("path traversal").or(predicate::str::contains("invalid")));
}

// ---------------------------------------------------------------------------
// Task 1: page edit tests (TDD RED)
// ---------------------------------------------------------------------------

#[test]
fn page_edit_no_editor_returns_error_mentioning_editor() {
    let dir = setup_space();
    write_page(&dir, "test-page", "# Test Page");

    sb_cmd(&dir)
        .args(["page", "edit", "test-page"])
        .env_remove("EDITOR")
        .env_remove("VISUAL")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("editor").or(predicate::str::contains("EDITOR")));
}

#[test]
fn page_edit_nonexistent_page_returns_not_found_error() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "edit", "nonexistent-page"])
        .env("EDITOR", "true")
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("not found").or(predicate::str::contains("nonexistent-page")),
        );
}

#[test]
fn page_edit_existing_page_with_editor_true_succeeds() {
    let dir = setup_space();
    write_page(&dir, "test-page", "# Test Page");

    // EDITOR=true succeeds immediately without modifying the file
    sb_cmd(&dir)
        .args(["page", "edit", "test-page"])
        .env("EDITOR", "true")
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Task 1: page delete tests (TDD RED)
// ---------------------------------------------------------------------------

#[test]
fn page_delete_force_removes_file() {
    let dir = setup_space();
    write_page(&dir, "to-delete", "# To Delete");

    let file_path = dir.path().join("to-delete.md");
    assert!(file_path.exists(), "file should exist before delete");

    sb_cmd(&dir)
        .args(["page", "delete", "to-delete", "--force"])
        .assert()
        .success();

    assert!(
        !file_path.exists(),
        "file should be deleted after --force delete"
    );
}

#[test]
fn page_delete_nonexistent_with_force_returns_error() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "delete", "nonexistent", "--force"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("not found").or(predicate::str::contains("nonexistent")));
}

#[test]
fn page_delete_without_force_and_non_tty_stdin_returns_usage_error() {
    let dir = setup_space();
    write_page(&dir, "safe-page", "# Safe Page");

    // When stdin is piped (non-TTY) and --force is not set, deletion should be refused
    sb_cmd(&dir)
        .args(["page", "delete", "safe-page"])
        .write_stdin("") // piping stdin makes it non-TTY
        .assert()
        .failure()
        .code(2) // Usage error exit code
        .stderr(
            predicate::str::contains("non-interactive").or(predicate::str::contains("--force")),
        );
}

// ---------------------------------------------------------------------------
// Task 2: page append tests (TDD)
// ---------------------------------------------------------------------------

#[test]
fn page_append_existing_adds_newline_then_content() {
    let dir = setup_space();
    write_page(&dir, "notes", "line1");

    sb_cmd(&dir)
        .args(["page", "append", "notes", "--content", "line2"])
        .assert()
        .success();

    let file_path = dir.path().join("notes.md");
    let content = std::fs::read_to_string(&file_path).expect("read file");
    assert!(
        content.contains("line1\nline2"),
        "appended content should follow original with newline separator, got: {content:?}"
    );
}

#[test]
fn page_append_nonexistent_creates_page_with_content() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "append", "new-page", "--content", "first line"])
        .assert()
        .success();

    let file_path = dir.path().join("new-page.md");
    assert!(file_path.exists(), "page should be created by append");
    let content = std::fs::read_to_string(&file_path).expect("read file");
    assert!(
        content.contains("first line"),
        "created page should contain the appended content, got: {content:?}"
    );
    // No leading newline when creating a new page
    assert!(
        !content.starts_with('\n'),
        "new page should not start with a newline, got: {content:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 2: page move tests (TDD)
// ---------------------------------------------------------------------------

#[test]
fn page_move_renames_file_on_disk() {
    let dir = setup_space();
    write_page(&dir, "old-name", "# Original Content");

    sb_cmd(&dir)
        .args(["page", "move", "old-name", "new-name"])
        .assert()
        .success();

    let old_path = dir.path().join("old-name.md");
    let new_path = dir.path().join("new-name.md");
    assert!(!old_path.exists(), "old file should be gone after move");
    assert!(new_path.exists(), "new file should exist after move");

    let content = std::fs::read_to_string(&new_path).expect("read moved file");
    assert!(
        content.contains("Original Content"),
        "moved file should preserve content"
    );
}

#[test]
fn page_move_creates_intermediate_directories() {
    let dir = setup_space();
    write_page(&dir, "flat-page", "# Flat Page");

    sb_cmd(&dir)
        .args(["page", "move", "flat-page", "nested/sub/page"])
        .assert()
        .success();

    let new_path = dir.path().join("nested").join("sub").join("page.md");
    assert!(
        new_path.exists(),
        "nested target file should be created by move"
    );
}

#[test]
fn page_move_source_not_found_returns_error() {
    let dir = setup_space();

    sb_cmd(&dir)
        .args(["page", "move", "missing-source", "target"])
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("not found").or(predicate::str::contains("missing-source")),
        );
}

#[test]
fn page_move_target_already_exists_returns_error() {
    let dir = setup_space();
    write_page(&dir, "source-page", "source content");
    write_page(&dir, "target-page", "target content");

    sb_cmd(&dir)
        .args(["page", "move", "source-page", "target-page"])
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::contains("already exists").or(predicate::str::contains("target-page")),
        );
}
