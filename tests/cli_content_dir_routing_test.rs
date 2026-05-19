//! Regression tests for the content-directory routing bug.
//!
//! When `sync.dir` points somewhere other than the space root (the directory
//! containing `.sb/`), write commands must target `space_root/sync.dir`, not
//! `space_root` itself. Otherwise files land in a parallel tree that
//! SilverBullet never sees.
//!
//! These tests configure `sync.dir = "content"` and assert that:
//!   1. Each write command creates files under `space_root/content/`.
//!   2. No `.md` files leak into `space_root/` itself.
//!
//! If a new command is added that writes to a page path, add it here.

use assert_cmd::Command;
use std::path::Path;
use tempfile::TempDir;

/// Create a space where `space_root` and `content_dir` are different paths.
///
/// Layout:
///   <tmp>/.sb/config.toml   (sync.dir = "content")
///   <tmp>/content/          (the content directory — where files must land)
fn split_space() -> TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
    std::fs::write(
        sb_dir.join("config.toml"),
        r#"server_url = "https://sb.example.com"
token = "test-token"
[sync]
dir = "content"
"#,
    )
    .expect("write config.toml");
    std::fs::create_dir_all(dir.path().join("content")).expect("create content dir");
    dir
}

fn sb_in(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("sb").expect("sb binary");
    cmd.current_dir(dir.path());
    cmd
}

/// Assert no `.md` file exists at the top level of `space_root`. This is the
/// failure mode the bug exhibited — a stray `Journal/YYYY-MM-DD.md` under
/// space_root instead of `content/Journal/...`.
fn assert_no_stray_pages_at_space_root(space_root: &Path) {
    for entry in std::fs::read_dir(space_root).expect("read space_root") {
        let entry = entry.expect("read entry");
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Only `.sb/` (state) and `content/` (configured content_dir) are expected.
        assert!(
            name == ".sb" || name == "content",
            "unexpected entry at space_root: {name:?} (writes must land under content/)"
        );
    }
}

// ---------------------------------------------------------------------------
// sb daily
// ---------------------------------------------------------------------------

#[test]
fn daily_append_writes_under_content_dir_not_space_root() {
    let space = split_space();
    let today = jiff::Zoned::now().date();
    let rel = format!("Journal/{}.md", today.strftime("%Y-%m-%d"));

    sb_in(&space)
        .args([
            "daily",
            "--append",
            "regression: routed through content_dir",
        ])
        .assert()
        .success();

    let in_content = space.path().join("content").join(&rel);
    let at_space_root = space.path().join(&rel);

    assert!(
        in_content.is_file(),
        "daily note must be created under content/: expected {}",
        in_content.display()
    );
    assert!(
        !at_space_root.exists(),
        "daily note must NOT be written at space_root: found {}",
        at_space_root.display()
    );
    assert_no_stray_pages_at_space_root(space.path());
}

#[test]
fn daily_reads_template_from_content_dir_not_space_root() {
    let space = split_space();
    std::fs::write(
        space.path().join("content").join("DailyTemplate.md"),
        "# from content_dir template\n",
    )
    .expect("seed template in content_dir");
    // A different template at space_root must be ignored.
    std::fs::write(
        space.path().join("DailyTemplate.md"),
        "# WRONG: from space_root template\n",
    )
    .expect("seed decoy template at space_root");

    // Override daily.template via env so we don't have to rewrite the config.
    sb_in(&space)
        .env("SB_DAILY_TEMPLATE", "DailyTemplate")
        .args(["daily", "--append", "boots up the template"])
        .assert()
        .success();

    let today = jiff::Zoned::now().date();
    let rel = format!("Journal/{}.md", today.strftime("%Y-%m-%d"));
    let note =
        std::fs::read_to_string(space.path().join("content").join(&rel)).expect("read daily note");
    assert!(
        note.contains("from content_dir template"),
        "template must be loaded from content_dir, got: {note:?}"
    );
    assert!(
        !note.contains("WRONG"),
        "decoy template at space_root must be ignored, got: {note:?}"
    );

    // Clean up the decoy so the stray-files check passes.
    std::fs::remove_file(space.path().join("DailyTemplate.md")).expect("remove decoy");
    assert_no_stray_pages_at_space_root(space.path());
}

// ---------------------------------------------------------------------------
// sb page
// ---------------------------------------------------------------------------

#[test]
fn page_create_writes_under_content_dir_not_space_root() {
    let space = split_space();

    sb_in(&space)
        .args(["page", "create", "Notes/Idea", "--content", "hello"])
        .assert()
        .success();

    assert!(
        space.path().join("content/Notes/Idea.md").is_file(),
        "page must be created under content/"
    );
    assert!(
        !space.path().join("Notes/Idea.md").exists(),
        "page must NOT be created at space_root"
    );
    assert_no_stray_pages_at_space_root(space.path());
}

#[test]
fn page_append_writes_under_content_dir_not_space_root() {
    let space = split_space();

    sb_in(&space)
        .args(["page", "append", "AppendTarget", "--content", "first line"])
        .assert()
        .success();

    assert!(
        space.path().join("content/AppendTarget.md").is_file(),
        "appended page must live under content/"
    );
    assert!(
        !space.path().join("AppendTarget.md").exists(),
        "appended page must NOT be at space_root"
    );
    assert_no_stray_pages_at_space_root(space.path());
}

#[test]
fn page_delete_targets_content_dir_not_space_root() {
    let space = split_space();
    // Seed a real page in content_dir AND a decoy at space_root with the same name.
    std::fs::write(space.path().join("content").join("DeleteMe.md"), "real").expect("seed real");
    std::fs::write(space.path().join("DeleteMe.md"), "decoy").expect("seed decoy");

    sb_in(&space)
        .args(["page", "delete", "DeleteMe", "--force"])
        .assert()
        .success();

    assert!(
        !space.path().join("content/DeleteMe.md").exists(),
        "delete must remove the file in content_dir"
    );
    assert!(
        space.path().join("DeleteMe.md").exists(),
        "delete must NOT touch the decoy at space_root"
    );

    // Clean up the decoy ourselves.
    std::fs::remove_file(space.path().join("DeleteMe.md")).expect("remove decoy");
    assert_no_stray_pages_at_space_root(space.path());
}

#[test]
fn page_move_targets_content_dir_not_space_root() {
    let space = split_space();
    std::fs::write(space.path().join("content").join("Src.md"), "body").expect("seed src");

    sb_in(&space)
        .args(["page", "move", "Src", "Moved/Here"])
        .assert()
        .success();

    assert!(
        !space.path().join("content/Src.md").exists(),
        "src must be gone from content_dir"
    );
    assert!(
        space.path().join("content/Moved/Here.md").is_file(),
        "dst must land under content_dir"
    );
    assert!(
        !space.path().join("Moved/Here.md").exists(),
        "dst must NOT land at space_root"
    );
    assert_no_stray_pages_at_space_root(space.path());
}

#[test]
fn page_list_only_sees_files_under_content_dir() {
    let space = split_space();
    std::fs::write(space.path().join("content").join("Visible.md"), "ok").expect("seed visible");
    // A file at space_root must be invisible to `page list`.
    std::fs::write(space.path().join("Hidden.md"), "should not be listed").expect("seed hidden");

    let out = sb_in(&space)
        .args(["page", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).expect("utf-8 stdout");

    assert!(
        stdout.contains("Visible"),
        "page list must include content_dir page"
    );
    assert!(
        !stdout.contains("Hidden"),
        "page list must not include files outside content_dir; got: {stdout}"
    );

    std::fs::remove_file(space.path().join("Hidden.md")).expect("remove decoy");
    assert_no_stray_pages_at_space_root(space.path());
}
