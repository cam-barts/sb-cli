/// Integration tests for `sb sync` subcommands.
///
/// Uses assert_cmd to run the real binary and assert on exit code, stdout, stderr.
/// Uses wiremock for HTTP mocking and tempfile for isolated space directories.
use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::{Connection, OptionalExtension};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create an initialized space directory with .sb/config.toml and state.db.
fn setup_space(dir: &TempDir, server_url: &str) -> std::path::PathBuf {
    let space = dir.path().to_path_buf();
    let sb_dir = space.join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    // Create the default sync content directory
    std::fs::create_dir_all(space.join("space")).unwrap();

    // Write config.toml
    std::fs::write(
        sb_dir.join("config.toml"),
        format!("server_url = \"{server_url}\"\ntoken = \"test-token\"\n"),
    )
    .unwrap();

    // Create state.db with schema
    let conn = Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_state (
            path TEXT PRIMARY KEY NOT NULL,
            local_hash TEXT,
            remote_hash TEXT,
            remote_mtime INTEGER NOT NULL DEFAULT 0,
            local_mtime INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'synced'
        );
        CREATE TABLE IF NOT EXISTS sync_meta (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );",
    )
    .unwrap();

    space
}

/// Build an `sb` command rooted at the given space directory.
///
/// Pins `XDG_CONFIG_HOME` to a non-existent path so the dev's real XDG config
/// can't leak into the subprocess and contaminate the test.
fn sb_in(space: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("sb").expect("sb binary");
    cmd.current_dir(space)
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg");
    cmd
}

// ---------------------------------------------------------------------------
// page move updates state.db
// ---------------------------------------------------------------------------

#[test]
fn page_move_updates_state_db_deletes_old_path_inserts_new() {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        "server_url = \"https://sb.example.com\"\n[sync]\ndir = \".\"\n",
    )
    .unwrap();

    // Create state.db with a row for the old path
    let db_path = sb_dir.join("state.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS sync_state (path TEXT PRIMARY KEY NOT NULL, local_hash TEXT, remote_hash TEXT, remote_mtime INTEGER NOT NULL DEFAULT 0, local_mtime INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'synced'); CREATE TABLE IF NOT EXISTS sync_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);").unwrap();
    conn.execute(
        "INSERT INTO sync_state (path, local_hash, remote_hash, remote_mtime, local_mtime, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["old-page.md", "abc123", "abc123", 1700000000000i64, 1700000001000i64, "synced"],
    ).unwrap();
    drop(conn);

    // Create the source page file
    std::fs::write(dir.path().join("old-page.md"), "# Old Page").unwrap();

    // Run sb page move
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["page", "move", "old-page", "new-page"])
        .current_dir(dir.path())
        .assert()
        .success();

    // Verify state.db: old path deleted, new path inserted with status='new'
    let conn = Connection::open(&db_path).unwrap();

    let old_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sync_state WHERE path = ?1",
            rusqlite::params!["old-page.md"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_count, 0, "old path should be deleted from state.db");

    let new_status: Option<String> = conn
        .query_row(
            "SELECT status FROM sync_state WHERE path = ?1",
            rusqlite::params!["new-page.md"],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        new_status.as_deref(),
        Some("new"),
        "new path should have status='new' in state.db"
    );
}

#[test]
fn page_move_state_db_update_is_atomic_no_partial_state() {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        "server_url = \"https://sb.example.com\"\n[sync]\ndir = \".\"\n",
    )
    .unwrap();

    // Create state.db with old-page2.md row
    let db_path = sb_dir.join("state.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS sync_state (path TEXT PRIMARY KEY NOT NULL, local_hash TEXT, remote_hash TEXT, remote_mtime INTEGER NOT NULL DEFAULT 0, local_mtime INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'synced'); CREATE TABLE IF NOT EXISTS sync_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);").unwrap();
    conn.execute(
        "INSERT INTO sync_state (path, status) VALUES (?1, ?2)",
        rusqlite::params!["old-page2.md", "synced"],
    )
    .unwrap();
    drop(conn);

    std::fs::write(dir.path().join("old-page2.md"), "# Old Page 2").unwrap();

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["page", "move", "old-page2", "new-page2"])
        .current_dir(dir.path())
        .assert()
        .success();

    // After successful move: only new-page2.md should exist in state.db
    let conn = Connection::open(&db_path).unwrap();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        total, 1,
        "state.db should have exactly one row after atomic move"
    );

    let new_path: String = conn
        .query_row("SELECT path FROM sync_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(new_path, "new-page2.md");
}

#[test]
fn page_move_works_when_state_db_has_no_row_for_old_path() {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        "server_url = \"https://sb.example.com\"\n[sync]\ndir = \".\"\n",
    )
    .unwrap();

    // Create state.db with no rows (first move before any sync)
    let db_path = sb_dir.join("state.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS sync_state (path TEXT PRIMARY KEY NOT NULL, local_hash TEXT, remote_hash TEXT, remote_mtime INTEGER NOT NULL DEFAULT 0, local_mtime INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'synced'); CREATE TABLE IF NOT EXISTS sync_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);").unwrap();
    drop(conn);

    std::fs::write(dir.path().join("untracked-page.md"), "# Untracked").unwrap();

    // Should succeed even though old path has no state.db row
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["page", "move", "untracked-page", "moved-page"])
        .current_dir(dir.path())
        .assert()
        .success();

    // new-page should be inserted as 'new'
    let conn = Connection::open(&db_path).unwrap();
    let new_status: Option<String> = conn
        .query_row(
            "SELECT status FROM sync_state WHERE path = ?1",
            rusqlite::params!["moved-page.md"],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        new_status.as_deref(),
        Some("new"),
        "moved page should have status='new' even when old path had no state.db row"
    );
}

// ---------------------------------------------------------------------------
// sb sync status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_status_shows_clean_state_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    sb_in(&space)
        .args(["sync", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0"));
}

#[tokio::test]
async fn sync_status_json_format_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    sb_in(&space)
        .args(["sync", "status", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("modified"))
        .stdout(predicate::str::contains("conflicts"))
        .stdout(predicate::str::contains("last_sync"));
}

// ---------------------------------------------------------------------------
// sb sync conflicts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_conflicts_shows_no_conflicts_when_clean() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    sb_in(&space)
        .args(["--format", "human", "sync", "conflicts"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No conflicts"));
}

#[tokio::test]
async fn sync_conflicts_json_format_returns_empty_array_when_clean() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    sb_in(&space)
        .args(["sync", "conflicts", "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[]"));
}

// ---------------------------------------------------------------------------
// sb sync status / sb sync conflicts -- server/editor-written conflict markers
//
// These are a DISTINCT kind of conflict from the state.db/stash-based one
// above: a plain content/filename scan, never touching state.db. REPORT
// ONLY -- sb never merges or resolves these.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_status_json_reports_marker_conflict_count_distinct_from_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // A file with inline conflict markers alongside a clean neighbour.
    std::fs::write(
        space.join("space").join("Marked.md"),
        "<<<<<<< SB sha256:1a2b3c4d\nours\n=======\ntheirs\n>>>>>>> SB sha256:5e6f7a8b\n",
    )
    .unwrap();
    std::fs::write(space.join("space").join("Clean.md"), "all good").unwrap();

    let assert = sb_in(&space)
        .args(["sync", "status", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(json["marker_conflicts"], 1, "stdout was: {stdout}");
    assert_eq!(
        json["conflicts"], 0,
        "stash conflicts must stay a distinct, unaffected count: {stdout}"
    );
}

#[tokio::test]
async fn sync_status_marker_hint_suppressed_under_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    std::fs::write(
        space.join("space").join("Marked.md"),
        "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n",
    )
    .unwrap();

    // Without --quiet: the human-readable hint appears on stderr.
    sb_in(&space)
        .args(["sync", "status", "--format", "json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("conflict markers"));

    // With --quiet: hint is suppressed, but the data (stdout) is unaffected.
    let assert = sb_in(&space)
        .args(["--quiet", "sync", "status", "--format", "json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("conflict markers").not());
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        json["marker_conflicts"], 1,
        "quiet must not suppress the data output: {stdout}"
    );
}

#[tokio::test]
async fn sync_conflicts_lists_plain_git_markers_as_marker_conflict_kind() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    std::fs::write(
        space.join("space").join("GitStyle.md"),
        "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature-branch\n",
    )
    .unwrap();

    let assert = sb_in(&space)
        .args(["sync", "conflicts", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let entries = json.as_array().expect("array");
    assert_eq!(entries.len(), 1, "stdout was: {stdout}");
    assert_eq!(entries[0]["kind"], "marker_conflict");
    assert_eq!(entries[0]["marker_kind"], "inline_markers");
    assert_eq!(entries[0]["path"], "GitStyle.md");
}

#[tokio::test]
async fn sync_conflicts_ignores_fenced_code_block_marker_example() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // A documentation page that demonstrates conflict markers inside a
    // fenced code block -- must NOT be reported as a real conflict.
    std::fs::write(
        space.join("space").join("HowConflictsWork.md"),
        "# How conflicts work\n\n```\n<<<<<<< SB sha256:1a2b3c4d\nyour version of the line\n=======\ntheir version of the line\n>>>>>>> SB sha256:5e6f7a8b\n```\n",
    )
    .unwrap();

    sb_in(&space)
        .args(["--format", "human", "sync", "conflicts"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No conflicts"));
}

#[tokio::test]
async fn sync_conflicts_lists_conflicted_sibling_file() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    std::fs::write(space.join("space").join("photo.jpg"), b"jpegbytes").unwrap();
    std::fs::write(
        space.join("space").join("photo.conflicted-a1b2c3.jpg"),
        b"other jpeg bytes",
    )
    .unwrap();

    let assert = sb_in(&space)
        .args(["sync", "conflicts", "--format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    let entries = json.as_array().expect("array");
    assert_eq!(entries.len(), 1, "stdout was: {stdout}");
    assert_eq!(entries[0]["kind"], "marker_conflict");
    assert_eq!(entries[0]["marker_kind"], "conflicted_sibling");
    assert_eq!(entries[0]["path"], "photo.conflicted-a1b2c3.jpg");
}

#[tokio::test]
async fn sync_conflicts_human_format_distinguishes_stash_from_marker_kind() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // A stash (metadata-driven) conflict, seeded directly in state.db.
    let conn = Connection::open(space.join(".sb").join("state.db")).unwrap();
    conn.execute(
        "INSERT INTO sync_state (path, local_hash, remote_hash, remote_mtime, local_mtime, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params!["Stashed.md", "lh", "rh", 1000i64, 2000i64, "conflict"],
    )
    .unwrap();
    drop(conn);

    // A marker (content-driven) conflict on an unrelated file.
    std::fs::write(
        space.join("space").join("Marked.md"),
        "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n",
    )
    .unwrap();

    sb_in(&space)
        .args(["--format", "human", "sync", "conflicts"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stash conflicts"))
        .stdout(predicate::str::contains("Stashed.md"))
        .stdout(predicate::str::contains("conflict markers"))
        .stdout(predicate::str::contains("Marked.md"));
}

// ---------------------------------------------------------------------------
// sb sync pull
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_pull_downloads_new_file_from_server() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // Mock GET /.fs -> file listing with one file
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"name":"test-note.md","lastModified":1700000000000,"created":1699000000000,"contentType":"text/markdown","size":20,"perm":"rw"}]"#,
        ))
        .mount(&server)
        .await;

    // Mock GET /.fs/test-note.md -> file content
    Mock::given(method("GET"))
        .and(path("/.fs/test-note.md"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("# Test Note\n")
                .insert_header("X-Last-Modified", "1700000000000"),
        )
        .mount(&server)
        .await;

    sb_in(&space).args(["sync", "pull"]).assert().success();

    // File should exist in the sync content directory after pull
    assert!(
        space.join("space/test-note.md").exists(),
        "test-note.md should be downloaded into space/ by sb sync pull"
    );
}

// ---------------------------------------------------------------------------
// sb sync push
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_push_exits_zero_with_no_local_changes() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // No local files -> push has nothing to do
    // Mock GET /.fs for any remote deletion check (pusher may call list_files)
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    sb_in(&space).args(["sync", "push"]).assert().success();
}

// ---------------------------------------------------------------------------
// sb sync (no subcommand) — pull then push
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_no_subcommand_runs_pull_then_push_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // Mock both pull (GET /.fs listing) and push (GET /.fs for deletions)
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    sb_in(&space).args(["sync"]).assert().success();
}

// ---------------------------------------------------------------------------
// sb sync pull --dry-run
// ---------------------------------------------------------------------------

/// --dry-run flag is accepted by the CLI parser for `sb sync pull`
#[tokio::test]
async fn dry_run_pull_flag_accepted_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // Mock GET /.fs listing — empty, so dry-run has nothing to plan
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    sb_in(&space)
        .args(["sync", "pull", "--dry-run"])
        .assert()
        .success();
}

/// --dry-run pull with a new remote file shows "download" action in human output
#[tokio::test]
async fn dry_run_pull_shows_download_action_for_new_remote_file() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // Mock GET /.fs listing — one new remote file
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"name":"dry-test.md","lastModified":1700000000000,"created":1699000000000,"contentType":"text/markdown","size":20}]"#,
        ))
        .mount(&server)
        .await;

    sb_in(&space)
        .args(["sync", "pull", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("download"))
        .stdout(predicate::str::contains("dry-test.md"));
}

/// --dry-run pull does NOT create any files on disk
#[tokio::test]
async fn dry_run_pull_does_not_modify_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // Mock GET /.fs listing — one new remote file
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"name":"should-not-appear.md","lastModified":1700000000000,"created":1699000000000,"contentType":"text/markdown","size":20}]"#,
        ))
        .mount(&server)
        .await;

    sb_in(&space)
        .args(["sync", "pull", "--dry-run"])
        .assert()
        .success();

    // File must NOT exist — dry-run must not download
    assert!(
        !space.join("space/should-not-appear.md").exists(),
        "dry-run pull must not write files to disk"
    );
}

/// --dry-run pull with --format json produces valid JSON array with action/path/reason
#[tokio::test]
async fn dry_run_pull_json_format_produces_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"name":"json-test.md","lastModified":1700000000000,"created":1699000000000,"contentType":"text/markdown","size":10}]"#,
        ))
        .mount(&server)
        .await;

    let output = sb_in(&space)
        .args(["sync", "pull", "--dry-run", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("output must be valid JSON");
    let arr = parsed.as_array().expect("JSON must be an array");
    assert!(!arr.is_empty(), "JSON array must not be empty");
    let first = &arr[0];
    assert!(
        first.get("action").is_some(),
        "each entry must have 'action'"
    );
    assert!(first.get("path").is_some(), "each entry must have 'path'");
    assert!(
        first.get("reason").is_some(),
        "each entry must have 'reason'"
    );
}

// ---------------------------------------------------------------------------
// sb sync push --dry-run
// ---------------------------------------------------------------------------

/// --dry-run flag is accepted by the CLI parser for `sb sync push`
#[tokio::test]
async fn dry_run_push_flag_accepted_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // No local files — push dry-run has nothing to plan
    sb_in(&space)
        .args(["sync", "push", "--dry-run"])
        .assert()
        .success();
}

/// --dry-run push with a locally modified file shows "upload" action
#[tokio::test]
async fn dry_run_push_shows_upload_action_for_new_local_file() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // Create a local file not tracked in state.db (new local file)
    std::fs::write(space.join("space/local-new.md"), "# New local page\n").unwrap();

    sb_in(&space)
        .args(["sync", "push", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("upload"))
        .stdout(predicate::str::contains("local-new.md"));
}

// ---------------------------------------------------------------------------
// sb sync --dry-run (no subcommand)
// ---------------------------------------------------------------------------

/// --dry-run flag is accepted at the top-level `sb sync` command
#[tokio::test]
async fn dry_run_sync_flag_accepted_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    sb_in(&space).args(["sync", "--dry-run"]).assert().success();
}

// ---------------------------------------------------------------------------
// sb sync resolve
// ---------------------------------------------------------------------------

/// `sb sync resolve` without a path argument exits with usage error (code 2)
#[test]
fn resolve_without_path_exits_with_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        "server_url = \"https://sb.example.com\"\n[sync]\ndir = \".\"\n",
    )
    .unwrap();

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["sync", "resolve"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(2);
}

/// `sb sync resolve --help` lists all expected flags
#[test]
fn resolve_help_shows_all_flags() {
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args(["sync", "resolve", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--keep-local"))
        .stdout(predicate::str::contains("--keep-remote"))
        .stdout(predicate::str::contains("--diff"))
        .stdout(predicate::str::contains("--force"))
        .stdout(predicate::str::contains("--all"));
}

/// `--keep-local` and `--keep-remote` cannot be used together (conflict_with)
#[test]
fn resolve_keep_local_and_keep_remote_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        "server_url = \"https://sb.example.com\"\n[sync]\ndir = \".\"\n",
    )
    .unwrap();

    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .args([
            "sync",
            "resolve",
            "some/page.md",
            "--keep-local",
            "--keep-remote",
        ])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(2);
}

// ---------------------------------------------------------------------------
// global --token flag threading
// ---------------------------------------------------------------------------

/// `sb --token <override> sync pull` uses the override token for HTTP requests.
///
/// The mock only responds to `Authorization: Bearer override-token`. If the
/// global flag is not threaded to the sync HTTP client, the config token
/// (`config-token`) would be sent and wiremock returns 404 (no matching mock),
/// causing the command to fail.
#[tokio::test]
async fn sync_pull_respects_global_token_flag() {
    let server = MockServer::start().await;

    // Mock file listing — only accept the override-token Authorization header
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .and(wiremock::matchers::header(
            "Authorization",
            "Bearer override-token",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    // Space configured with a DIFFERENT token — override must win
    let space = dir.path().to_path_buf();
    let sb_dir = space.join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::create_dir_all(space.join("space")).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        format!(
            "server_url = \"{}\"\ntoken = \"config-token\"\n",
            server.uri()
        ),
    )
    .unwrap();
    let conn = rusqlite::Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS sync_state (path TEXT PRIMARY KEY NOT NULL, local_hash TEXT, remote_hash TEXT, remote_mtime INTEGER NOT NULL DEFAULT 0, local_mtime INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'synced'); CREATE TABLE IF NOT EXISTS sync_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);").unwrap();
    drop(conn);

    // Run with --token flag placed BEFORE the subcommand (global flag)
    Command::cargo_bin("sb")
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .current_dir(&space)
        .args(["--token", "override-token", "sync", "pull"])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// sb sync resolve: multi-file walk (--all) and the picker's non-interactive guard
// ---------------------------------------------------------------------------

/// Seed a space with two conflicted files: local content, a stashed remote
/// version whose bytes differ, and matching `conflict` rows in state.db.
///
/// Returns the space root. Files are `Doc.md` (space root) and
/// `Journal/Entry.md` (nested), so the nested stash-dir pruning is exercised too.
fn setup_two_conflicts(dir: &TempDir, server_url: &str) -> std::path::PathBuf {
    let space = setup_space(dir, server_url);
    let sb_dir = space.join(".sb");

    std::fs::create_dir_all(space.join("space").join("Journal")).unwrap();
    std::fs::write(space.join("space").join("Doc.md"), "local doc\n").unwrap();
    std::fs::write(
        space.join("space").join("Journal").join("Entry.md"),
        "local entry\n",
    )
    .unwrap();

    std::fs::create_dir_all(sb_dir.join("conflicts").join("Journal")).unwrap();
    std::fs::write(
        sb_dir.join("conflicts").join("Doc.20260901T120000.md"),
        "remote doc\n",
    )
    .unwrap();
    std::fs::write(
        sb_dir
            .join("conflicts")
            .join("Journal")
            .join("Entry.20260901T120000.md"),
        "remote entry\n",
    )
    .unwrap();

    let conn = Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("ALTER TABLE sync_state ADD COLUMN conflict_at INTEGER NOT NULL DEFAULT 0;")
        .unwrap();
    for path in ["Doc.md", "Journal/Entry.md"] {
        conn.execute(
            "INSERT INTO sync_state
                (path, local_hash, remote_hash, remote_mtime, local_mtime, status, conflict_at)
             VALUES (?1, 'lh', 'rh', 1700000000000, 1700000001000, 'conflict', 1700000002000)",
            [path],
        )
        .unwrap();
    }
    space
}

/// Read a row's status back out of state.db.
fn status_of(space: &std::path::Path, path: &str) -> Option<String> {
    let conn = Connection::open(space.join(".sb").join("state.db")).unwrap();
    conn.query_row(
        "SELECT status FROM sync_state WHERE path = ?1",
        [path],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// `--all --keep-remote` overwrites every local file with its stash, clears the
/// conflict rows, and cleans up the stash files (including empty parent dirs).
#[test]
fn resolve_all_keep_remote_resolves_every_conflict() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-remote"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(space.join("space").join("Doc.md")).unwrap(),
        "remote doc\n"
    );
    assert_eq!(
        std::fs::read_to_string(space.join("space").join("Journal").join("Entry.md")).unwrap(),
        "remote entry\n"
    );
    assert_eq!(status_of(&space, "Doc.md").as_deref(), Some("synced"));
    assert_eq!(
        status_of(&space, "Journal/Entry.md").as_deref(),
        Some("synced")
    );
    assert!(!space
        .join(".sb")
        .join("conflicts")
        .join("Doc.20260901T120000.md")
        .exists());
    // Nested stash dir is pruned once it is empty.
    assert!(!space.join(".sb").join("conflicts").join("Journal").exists());
}

/// `--all --keep-local` uploads each local file to the server and leaves the
/// local bytes alone.
#[tokio::test]
async fn resolve_all_keep_local_uploads_every_file() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/.fs/Doc.md"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/.fs/Journal/Entry.md"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    // The meta GET after each upload feeds remote_mtime. Mock it, so the test
    // asserts a real round-tripped value instead of green-lighting the 0 that
    // `get_file_meta(...).unwrap_or(0)` writes when the call fails.
    Mock::given(method("GET"))
        .and(path("/.fs/Doc.md"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", "1750000000000"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.fs/Journal/Entry.md"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", "1750000000000"))
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, &server.uri());

    sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-local"])
        .assert()
        .success();

    // Local content untouched; both rows resolved.
    assert_eq!(
        std::fs::read_to_string(space.join("space").join("Doc.md")).unwrap(),
        "local doc\n"
    );
    assert_eq!(status_of(&space, "Doc.md").as_deref(), Some("synced"));
    assert_eq!(
        status_of(&space, "Journal/Entry.md").as_deref(),
        Some("synced")
    );
    // remote_mtime must come from the server, not the silent 0 fallback.
    assert_eq!(remote_mtime_of(&space, "Doc.md"), Some(1750000000000));
    assert_eq!(
        remote_mtime_of(&space, "Journal/Entry.md"),
        Some(1750000000000)
    );
    // Mock `.expect(1)` assertions fire on drop.
    drop(server);
}

/// `--all` with no keep flag and no terminal must fail fast, not block forever
/// on a prompt nobody can answer.
#[test]
fn resolve_all_without_keep_flag_errors_instead_of_hanging() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    sb_in(&space)
        .timeout(std::time::Duration::from_secs(20))
        .args(["sync", "resolve", "--all"])
        .assert()
        .failure()
        .code(2)
        // Two different exit-2 usage errors live in this command; name the one
        // under test so the assertion cannot drift onto the other.
        .stderr(predicate::str::contains("cannot prompt for a resolution"));

    // Nothing was touched.
    assert_eq!(status_of(&space, "Doc.md").as_deref(), Some("conflict"));
    assert_eq!(
        std::fs::read_to_string(space.join("space").join("Doc.md")).unwrap(),
        "local doc\n"
    );
}

/// `--all` on a clean space says so and changes nothing.
#[test]
fn resolve_all_with_no_conflicts_reports_none() {
    let dir = TempDir::new().unwrap();
    let space = setup_space(&dir, "https://sb.example.com");

    sb_in(&space)
        .args([
            "sync",
            "resolve",
            "--all",
            "--keep-remote",
            "--format",
            "human",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("No conflicts"));
}

/// `--all` and an explicit path are mutually exclusive (clap-enforced).
#[test]
fn resolve_all_conflicts_with_explicit_path() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    sb_in(&space)
        .args(["sync", "resolve", "--all", "Doc.md"])
        .assert()
        .failure()
        .code(2)
        // Assert the clap constraint specifically: plain `resolve Doc.md` with
        // no keep flag also exits 2, via the no-input guard, so the code alone
        // would still pass with `conflicts_with` deleted.
        .stderr(predicate::str::contains("cannot be used with"));
}

/// An explicit path that is not in conflict still errors, as before.
#[test]
fn resolve_explicit_path_not_in_conflict_still_errors() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");
    let conn = Connection::open(space.join(".sb").join("state.db")).unwrap();
    conn.execute(
        "UPDATE sync_state SET status = 'synced' WHERE path = 'Doc.md'",
        [],
    )
    .unwrap();
    drop(conn);

    sb_in(&space)
        .args(["sync", "resolve", "Doc.md", "--keep-remote"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not in conflict"));
}

/// `--format json` reports what happened to each file, on stdout.
#[test]
fn resolve_all_emits_json_results() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    let out = sb_in(&space)
        .args([
            "sync",
            "resolve",
            "--all",
            "--keep-remote",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&out).expect("stdout is valid JSON");
    let entries = parsed.as_array().expect("array");
    assert_eq!(entries.len(), 2);
    for e in entries {
        assert_eq!(e["resolution"], "kept_remote");
    }
    let paths: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"Doc.md"), "got: {paths:?}");
    assert!(paths.contains(&"Journal/Entry.md"), "got: {paths:?}");
}

/// `--quiet` silences the progress chatter but still does the work.
#[test]
fn resolve_all_quiet_is_silent_but_effective() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    let assert = sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-remote", "--quiet"])
        .assert()
        .success();
    assert_eq!(
        String::from_utf8_lossy(&assert.get_output().stderr).trim(),
        ""
    );
    assert_eq!(
        std::fs::read_to_string(space.join("space").join("Doc.md")).unwrap(),
        "remote doc\n"
    );
    assert_eq!(status_of(&space, "Doc.md").as_deref(), Some("synced"));
}

/// One unresolvable file must not abandon the rest of the walk, and must not
/// leave the failed row looking resolved.
#[test]
fn resolve_all_continues_past_a_failing_file() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");
    // Remove Doc.md's stash so it cannot be resolved.
    std::fs::remove_file(
        space
            .join(".sb")
            .join("conflicts")
            .join("Doc.20260901T120000.md"),
    )
    .unwrap();

    sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-remote"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Failed to resolve 'Doc.md'"));

    // The good file resolved; the bad one is untouched, not half-written.
    assert_eq!(
        status_of(&space, "Journal/Entry.md").as_deref(),
        Some("synced")
    );
    assert_eq!(status_of(&space, "Doc.md").as_deref(), Some("conflict"));
    assert_eq!(
        std::fs::read_to_string(space.join("space").join("Doc.md")).unwrap(),
        "local doc\n"
    );
}

/// `sb sync conflicts` points at the interactive resolver, unless --quiet.
#[test]
fn conflicts_suggests_resolve_unless_quiet() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    sb_in(&space)
        .args(["sync", "conflicts", "--format", "human"])
        .assert()
        .success()
        .stderr(predicate::str::contains("sb sync resolve"));

    sb_in(&space)
        .args(["sync", "conflicts", "--quiet", "--format", "human"])
        .assert()
        .success()
        .stderr(predicate::str::contains("sb sync resolve").not());
}

/// Read a row's remote_mtime back out of state.db.
fn remote_mtime_of(space: &std::path::Path, path: &str) -> Option<i64> {
    let conn = Connection::open(space.join(".sb").join("state.db")).unwrap();
    conn.query_row(
        "SELECT remote_mtime FROM sync_state WHERE path = ?1",
        [path],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

/// Two pages whose stems collide (`A.md` and `A.B.md`) must not steal each
/// other's stash.
///
/// `A.md` has stash prefix `A.`, which also matches `A.B.<ts>.md`. Matching on
/// the prefix alone made `--all` overwrite `A.md` with A.B's remote content and
/// delete A.B's only copy of it.
#[test]
fn resolve_all_does_not_confuse_stashes_of_stem_prefixed_pages() {
    let dir = TempDir::new().unwrap();
    let space = setup_space(&dir, "https://sb.example.com");
    let sb_dir = space.join(".sb");

    std::fs::write(space.join("space").join("A.md"), "LOCAL A\n").unwrap();
    std::fs::write(space.join("space").join("A.B.md"), "LOCAL A.B\n").unwrap();
    std::fs::create_dir_all(sb_dir.join("conflicts")).unwrap();
    std::fs::write(
        sb_dir.join("conflicts").join("A.20260901T120000.md"),
        "REMOTE A\n",
    )
    .unwrap();
    // Written second so it is the "most recent" match, which is what the old
    // mtime-sort would have handed to A.md.
    std::fs::write(
        sb_dir.join("conflicts").join("A.B.20260901T130000.md"),
        "REMOTE A.B\n",
    )
    .unwrap();

    let conn = Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("ALTER TABLE sync_state ADD COLUMN conflict_at INTEGER NOT NULL DEFAULT 0;")
        .unwrap();
    for p in ["A.md", "A.B.md"] {
        conn.execute(
            "INSERT INTO sync_state VALUES (?1,'l','r',1,1,'conflict',1700000002000)",
            [p],
        )
        .unwrap();
    }
    drop(conn);

    sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-remote"])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(space.join("space").join("A.md")).unwrap(),
        "REMOTE A\n"
    );
    assert_eq!(
        std::fs::read_to_string(space.join("space").join("A.B.md")).unwrap(),
        "REMOTE A.B\n"
    );
    assert_eq!(status_of(&space, "A.md").as_deref(), Some("synced"));
    assert_eq!(status_of(&space, "A.B.md").as_deref(), Some("synced"));
}

/// A poisoned state.db row must be skipped, not allowed to kill the whole walk.
#[test]
fn resolve_all_skips_an_invalid_row_and_resolves_the_rest() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");
    let conn = Connection::open(space.join(".sb").join("state.db")).unwrap();
    conn.execute(
        "INSERT INTO sync_state VALUES ('../escape.md','l','r',1,1,'conflict',1700000002000)",
        [],
    )
    .unwrap();
    drop(conn);

    sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-remote"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("../escape.md"));

    // The two good files still got resolved.
    assert_eq!(status_of(&space, "Doc.md").as_deref(), Some("synced"));
    assert_eq!(
        status_of(&space, "Journal/Entry.md").as_deref(),
        Some("synced")
    );
}

/// An explicitly named bad path is still a plain usage error, exit 2.
#[test]
fn resolve_explicit_traversal_path_is_a_usage_error() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    sb_in(&space)
        .args(["sync", "resolve", "../escape.md", "--keep-remote"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("must not contain '..'"));
}

/// `--diff` writes the diff to stdout, so nothing else may be written there.
#[test]
fn resolve_diff_leaves_stdout_free_of_json() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");

    let out = sb_in(&space)
        .args(["sync", "resolve", "Doc.md", "--diff", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("remote doc"), "diff output missing: {text}");
    assert!(
        !text.contains("\"resolution\""),
        "JSON leaked into diff output: {text}"
    );
}

/// The JSON contract must not change shape just because the space happens to
/// hold exactly one conflict.
#[test]
fn resolve_all_json_shape_is_stable_with_a_single_conflict() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");
    let conn = Connection::open(space.join(".sb").join("state.db")).unwrap();
    conn.execute("DELETE FROM sync_state WHERE path = 'Doc.md'", [])
        .unwrap();
    drop(conn);
    // Make the single remaining conflict fail.
    std::fs::remove_file(
        space
            .join(".sb")
            .join("conflicts")
            .join("Journal")
            .join("Entry.20260901T120000.md"),
    )
    .unwrap();

    let out = sb_in(&space)
        .args([
            "sync",
            "resolve",
            "--all",
            "--keep-remote",
            "--format",
            "json",
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&out)
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}): {}", String::from_utf8_lossy(&out)));
    let entries = parsed.as_array().expect("array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], "Journal/Entry.md");
    assert_eq!(entries[0]["resolution"], "failed");
}

/// When every file fails for the same reason, that reason survives intact
/// instead of being flattened into a generic error.
#[test]
fn resolve_all_preserves_the_error_category_when_everything_fails() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");
    // No token anywhere -> auth failure (exit 3), for both files.
    std::fs::write(
        space.join(".sb").join("config.toml"),
        "server_url = \"https://sb.example.com\"\n",
    )
    .unwrap();

    sb_in(&space)
        .env("SB_TOKEN", "")
        .args(["sync", "resolve", "--all", "--keep-local"])
        .assert()
        .failure()
        .code(3);
}

/// An absolute `[sync] dir` must never make the content dir escape the space.
#[test]
fn sync_refuses_a_content_dir_outside_the_space() {
    let dir = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    let space = setup_space(&dir, "https://sb.example.com");
    std::fs::write(
        space.join(".sb").join("config.toml"),
        format!(
            "server_url = \"https://sb.example.com\"\ntoken = \"t\"\n[sync]\ndir = \"{}\"\n",
            other.path().display()
        ),
    )
    .unwrap();

    sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-remote"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("outside the space"));
}

/// `--quiet` also silences the stash warnings, not just the progress lines.
#[test]
fn resolve_quiet_silences_stash_warnings_too() {
    let dir = TempDir::new().unwrap();
    let space = setup_two_conflicts(&dir, "https://sb.example.com");
    // A second, older stash for Doc.md triggers the "found N stash files" warning.
    std::fs::write(
        space
            .join(".sb")
            .join("conflicts")
            .join("Doc.20260101T000000.md"),
        "older remote doc\n",
    )
    .unwrap();

    let assert = sb_in(&space)
        .args(["sync", "resolve", "--all", "--keep-remote", "--quiet"])
        .assert()
        .success();
    assert_eq!(
        String::from_utf8_lossy(&assert.get_output().stderr).trim(),
        ""
    );
    // Without --quiet the warning is expected to show up.
    let space2dir = TempDir::new().unwrap();
    let space2 = setup_two_conflicts(&space2dir, "https://sb.example.com");
    std::fs::write(
        space2
            .join(".sb")
            .join("conflicts")
            .join("Doc.20260101T000000.md"),
        "older remote doc\n",
    )
    .unwrap();
    sb_in(&space2)
        .args(["sync", "resolve", "--all", "--keep-remote"])
        .assert()
        .success()
        .stderr(predicate::str::contains("found 2 stash files"));
}

// ---------------------------------------------------------------------------
// remote_etag migration + conditional writes
// ---------------------------------------------------------------------------

/// Column names of sync_state, straight from SQLite.
fn sync_state_columns(db: &std::path::Path) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(sync_state)").unwrap();
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    cols
}

fn row_field(db: &std::path::Path, path: &str, column: &str) -> Option<String> {
    let conn = Connection::open(db).unwrap();
    conn.query_row(
        &format!("SELECT {column} FROM sync_state WHERE path = ?1"),
        rusqlite::params![path],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .unwrap()
    .flatten()
}

/// `setup_space` writes the pre-conflict_at, pre-remote_etag schema on purpose:
/// it is the schema real users have on disk. Opening it must migrate in place.
#[tokio::test]
async fn opening_a_pre_existing_state_db_adds_the_remote_etag_column() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    let db_path = space.join(".sb/state.db");

    // A row written by the old CLI, with only the old columns.
    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO sync_state (path, local_hash, remote_hash, remote_mtime, local_mtime, status)
         VALUES ('space/old.md', 'blake3local', 'blake3remote', 1700000000000, 1700000000000, 'synced')",
        [],
    )
    .unwrap();
    drop(conn);

    assert!(!sync_state_columns(&db_path).contains(&"remote_etag".to_string()));

    sb_in(&space).args(["sync", "status"]).assert().success();

    assert!(
        sync_state_columns(&db_path).contains(&"remote_etag".to_string()),
        "opening the db should have added remote_etag"
    );
    assert_eq!(
        row_field(&db_path, "space/old.md", "remote_etag"),
        None,
        "an old row has no ETag, so pushes stay unconditional for it"
    );
    assert_eq!(
        row_field(&db_path, "space/old.md", "remote_hash"),
        Some("blake3remote".to_string()),
        "the blake3 remote_hash must be untouched by the migration"
    );
}

/// Seed a modified-locally file plus its state.db row, migrating the db first.
/// Returns the state.db path.
fn seed_modified_file(space: &std::path::Path, etag: Option<&str>) -> std::path::PathBuf {
    std::fs::write(space.join("space/page.md"), "new local content").unwrap();
    let db_path = space.join(".sb/state.db");

    // Run a read-only command to apply the schema migration, then seed.
    sb_in(space).args(["sync", "status"]).assert().success();

    let conn = Connection::open(&db_path).unwrap();
    conn.execute(
        "INSERT INTO sync_state
         (path, local_hash, remote_hash, remote_mtime, local_mtime, status, conflict_at, remote_etag)
         VALUES ('page.md', 'stale_blake3', 'stale_blake3', 1700000000000, 0, 'synced', 0, ?1)",
        rusqlite::params![etag],
    )
    .unwrap();
    db_path
}

#[tokio::test]
async fn push_sends_if_match_and_stores_the_etag_the_put_returned() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    let db_path = seed_modified_file(&space, Some("\"sha256:old\""));

    Mock::given(method("GET"))
        .and(path("/.fs/page.md"))
        .and(wiremock::matchers::header("X-Get-Meta", "true"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000000000"))
        .mount(&server)
        .await;
    // Constrained on If-Match: a PUT without it finds no mock and 404s, which
    // would fail the command.
    Mock::given(method("PUT"))
        .and(path("/.fs/page.md"))
        .and(wiremock::matchers::header("If-Match", "\"sha256:old\""))
        .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"sha256:new\""))
        .expect(1)
        .mount(&server)
        .await;

    sb_in(&space).args(["sync", "push"]).assert().success();

    assert_eq!(
        row_field(&db_path, "page.md", "remote_etag"),
        Some("\"sha256:new\"".to_string()),
        "the ETag from the PUT response is what the next push conditions on"
    );
}

/// The likely production path: a server with no conditional-write support.
#[tokio::test]
async fn push_without_a_stored_etag_sends_no_if_match_and_still_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    let db_path = seed_modified_file(&space, None);

    Mock::given(method("GET"))
        .and(path("/.fs/page.md"))
        .and(wiremock::matchers::header("X-Get-Meta", "true"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000000000"))
        .mount(&server)
        .await;
    // Any PUT carrying an If-Match would match this mock and fail the run.
    Mock::given(method("PUT"))
        .and(path("/.fs/page.md"))
        .and(wiremock::matchers::header_exists("If-Match"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/.fs/page.md"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    sb_in(&space).args(["sync", "push"]).assert().success();

    assert_eq!(
        row_field(&db_path, "page.md", "status"),
        Some("synced".to_string())
    );
    assert_eq!(row_field(&db_path, "page.md", "remote_etag"), None);
}

// ---------------------------------------------------------------------------
// 403 is a read-only path, not an auth failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_403_on_one_file_still_pushes_and_commits_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    let db_path = space.join(".sb/state.db");

    for name in ["a.md", "b.md", "c.md"] {
        std::fs::write(space.join("space").join(name), "content").unwrap();
    }

    Mock::given(method("PUT"))
        .and(path("/.fs/b.md"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::header("X-Get-Meta", "true"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000009000"))
        .mount(&server)
        .await;

    sb_in(&space)
        .args(["--format", "human", "sync", "push"])
        .assert()
        .success()
        .stderr(predicate::str::contains("read-only"));

    for name in ["a.md", "c.md"] {
        assert_eq!(
            row_field(&db_path, name, "status"),
            Some("synced".to_string()),
            "{name} uploaded; one read-only sibling must not discard its state"
        );
    }
    assert_eq!(
        row_field(&db_path, "b.md", "status"),
        Some("readonly".to_string()),
        "the refused path is recorded so the next push skips it"
    );
}

#[tokio::test]
async fn a_401_on_push_still_exits_3_but_a_403_does_not() {
    // 401: credentials really are wrong.
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    std::fs::write(space.join("space/page.md"), "content").unwrap();

    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    sb_in(&space).args(["sync", "push"]).assert().code(3);

    // 403: the path is read-only. Recorded, reported, and not an auth exit.
    let dir2 = tempfile::tempdir().unwrap();
    let server2 = MockServer::start().await;
    let space2 = setup_space(&dir2, &server2.uri());
    std::fs::write(space2.join("space/page.md"), "content").unwrap();

    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server2)
        .await;
    Mock::given(method("GET"))
        .and(wiremock::matchers::header("X-Get-Meta", "true"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", "1"))
        .mount(&server2)
        .await;

    let assertion = sb_in(&space2).args(["sync", "push"]).assert();
    let code = assertion.get_output().status.code().unwrap();
    assert_ne!(code, 3, "a read-only path is not an authentication failure");
    assert_eq!(
        row_field(&space2.join(".sb/state.db"), "page.md", "status"),
        Some("readonly".to_string())
    );
}

// ---------------------------------------------------------------------------
// read-only (403) paths: the file survives, and the row is never a dead end
// ---------------------------------------------------------------------------

/// Push a local file the server refuses with 403, leaving a `readonly` row.
async fn refuse_push_of(space: &std::path::Path, server: &MockServer, name: &str, body: &str) {
    std::fs::write(space.join("space").join(name), body).unwrap();

    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(403))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(server)
        .await;

    sb_in(space).args(["sync", "push"]).assert().success();
    assert_eq!(
        row_field(&space.join(".sb/state.db"), name, "status"),
        Some("readonly".to_string()),
        "precondition: the 403 must have been recorded"
    );
}

/// A file the server refused is absent from the listing by definition. Pull
/// must not read that as "deleted on the server" and destroy the user's edit.
#[tokio::test]
async fn a_refused_file_survives_the_next_pull() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    refuse_push_of(&space, &server, "Config.md", "my refused edit\n").await;

    sb_in(&space).args(["sync", "pull"]).assert().success();

    assert_eq!(
        std::fs::read_to_string(space.join("space/Config.md"))
            .expect("the refused file must still be on disk"),
        "my refused edit\n"
    );
}

/// The same thing through the front door: `sb sync` twice in a row.
#[tokio::test]
async fn a_refused_file_survives_two_full_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    refuse_push_of(&space, &server, "Config.md", "my refused edit\n").await;

    for _ in 0..2 {
        sb_in(&space).args(["sync"]).assert().success();
    }

    assert_eq!(
        std::fs::read_to_string(space.join("space/Config.md"))
            .expect("the refused file must still be on disk"),
        "my refused edit\n"
    );
}

/// Dry-run must agree with what pull actually does, or it is worse than useless.
#[tokio::test]
async fn dry_run_pull_does_not_plan_to_delete_a_refused_file() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());
    refuse_push_of(&space, &server, "Config.md", "my refused edit\n").await;

    sb_in(&space)
        .args(["sync", "pull", "--dry-run", "--format", "human"])
        .assert()
        .success()
        .stdout(predicate::str::contains("delete_local").not());
}

/// A server that refuses DELETE keeps the file. The next pull brings it back,
/// rather than leaving a row nothing can reach.
#[tokio::test]
async fn a_refused_delete_is_undone_by_the_next_pull() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let space = setup_space(&dir, &server.uri());

    // Tracked and synced, but deleted locally.
    Connection::open(space.join(".sb/state.db"))
        .unwrap()
        .execute(
            "INSERT INTO sync_state
                (path, local_hash, remote_hash, remote_mtime, local_mtime, status)
             VALUES ('Config.md', 'known', 'known', 1700000000000, 0, 'synced')",
            [],
        )
        .unwrap();

    Mock::given(method("GET"))
        .and(wiremock::matchers::header("X-Get-Meta", "true"))
        .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000000000"))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.fs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"name":"Config.md","lastModified":1700000000000,"created":1699000000000,"contentType":"text/markdown","size":15}]"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/.fs/Config.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string("server content\n"))
        .mount(&server)
        .await;

    sb_in(&space).args(["sync", "push"]).assert().success();
    sb_in(&space).args(["sync", "pull"]).assert().success();

    assert_eq!(
        std::fs::read_to_string(space.join("space/Config.md"))
            .expect("a delete the server refused must leave the file where it is"),
        "server content\n"
    );
    assert_eq!(
        row_field(&space.join(".sb/state.db"), "Config.md", "status"),
        Some("synced".to_string())
    );
}
