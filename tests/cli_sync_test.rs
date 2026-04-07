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
fn sb_in(space: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("sb").expect("sb binary");
    cmd.current_dir(space);
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
        "server_url = \"https://sb.example.com\"\n",
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
        "server_url = \"https://sb.example.com\"\n",
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
        "server_url = \"https://sb.example.com\"\n",
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
        .args(["sync", "conflicts"])
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
        "server_url = \"https://sb.example.com\"\n",
    )
    .unwrap();

    Command::cargo_bin("sb")
        .unwrap()
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
        .args(["sync", "resolve", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--keep-local"))
        .stdout(predicate::str::contains("--keep-remote"))
        .stdout(predicate::str::contains("--diff"))
        .stdout(predicate::str::contains("--force"));
}

/// `--keep-local` and `--keep-remote` cannot be used together (conflict_with)
#[test]
fn resolve_keep_local_and_keep_remote_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        "server_url = \"https://sb.example.com\"\n",
    )
    .unwrap();

    Command::cargo_bin("sb")
        .unwrap()
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
        .current_dir(&space)
        .args(["--token", "override-token", "sync", "pull"])
        .assert()
        .success();
}
