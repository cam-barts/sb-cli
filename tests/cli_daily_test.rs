/// Tests for the `sb daily` command.
///
/// Tests verify observable behavior: file creation, path resolution, content appending,
/// date arithmetic, and error cases. No implementation details are tested.
use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;
use tempfile::TempDir;

// ============================================================================
// Test helpers
// ============================================================================

/// Creates an isolated space directory with `.sb/config.toml`.
/// Returns the TempDir so it lives for the duration of the test.
fn setup_space(config_toml: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
    let mut f = std::fs::File::create(sb_dir.join("config.toml")).expect("create config.toml");
    f.write_all(config_toml.as_bytes())
        .expect("write config.toml");
    dir
}

fn default_space() -> TempDir {
    setup_space("server_url = \"https://sb.example.com\"\n")
}

/// Build an `sb` Command already configured to run from within the space dir.
fn sb_in(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("sb").expect("sb binary must exist");
    cmd.current_dir(dir.path());
    cmd
}

// ============================================================================
// Unit-level behavioral tests for resolve_daily_date
// (tested via the binary's observable output — path includes the date)
// ============================================================================

/// `sb daily --append TEXT` with default path template creates
/// `Journal/YYYY-MM-DD.md` (today's date) and writes TEXT to it.
#[test]
fn daily_append_creates_note_at_todays_date_path() {
    let space = default_space();
    let today = jiff::Zoned::now().date();
    let expected_filename = format!("Journal/{}.md", today.strftime("%Y-%m-%d"));

    sb_in(&space)
        .args(["daily", "--append", "test entry"])
        .assert()
        .success();

    let note_path = space.path().join(&expected_filename);
    assert!(
        note_path.exists(),
        "daily note should be created at {expected_filename}"
    );

    let content = std::fs::read_to_string(&note_path).expect("read note");
    assert!(
        content.contains("test entry"),
        "note should contain appended text; got: {content:?}"
    );
}

/// `sb daily --yesterday --append TEXT` creates a note with yesterday's date.
#[test]
fn daily_append_yesterday_targets_yesterday() {
    use jiff::ToSpan;

    let space = default_space();
    let yesterday = jiff::Zoned::now()
        .date()
        .checked_add((-1i32).days())
        .expect("date arithmetic");
    let expected_filename = format!("Journal/{}.md", yesterday.strftime("%Y-%m-%d"));

    sb_in(&space)
        .args(["daily", "--yesterday", "--append", "yesterday entry"])
        .assert()
        .success();

    let note_path = space.path().join(&expected_filename);
    assert!(
        note_path.exists(),
        "yesterday's daily note should be at {expected_filename}"
    );
}

/// `sb daily --offset -3 --append TEXT` creates a note 3 days in the past.
#[test]
fn daily_append_offset_minus3_targets_3_days_ago() {
    use jiff::ToSpan;

    let space = default_space();
    let target = jiff::Zoned::now()
        .date()
        .checked_add((-3i32).days())
        .expect("date arithmetic");
    let expected_filename = format!("Journal/{}.md", target.strftime("%Y-%m-%d"));

    sb_in(&space)
        .args(["daily", "--offset", "-3", "--append", "three days ago"])
        .assert()
        .success();

    let note_path = space.path().join(&expected_filename);
    assert!(
        note_path.exists(),
        "note 3 days ago should be at {expected_filename}"
    );
}

/// `sb daily --append TEXT` on an existing note appends text without recreating.
#[test]
fn daily_append_to_existing_note_preserves_prior_content() {
    let space = default_space();
    let today = jiff::Zoned::now().date();
    let note_path = space
        .path()
        .join(format!("Journal/{}.md", today.strftime("%Y-%m-%d")));

    // Pre-create with initial content
    std::fs::create_dir_all(note_path.parent().unwrap()).expect("create Journal dir");
    std::fs::write(&note_path, "# Daily note\n").expect("write initial content");

    sb_in(&space)
        .args(["daily", "--append", "appended entry"])
        .assert()
        .success();

    let content = std::fs::read_to_string(&note_path).expect("read note");
    assert!(
        content.contains("# Daily note"),
        "original content should be preserved; got: {content:?}"
    );
    assert!(
        content.contains("appended entry"),
        "appended text should be present; got: {content:?}"
    );
}

/// `sb daily --append TEXT` on a note created from empty template still writes content.
#[test]
fn daily_append_creates_note_and_appends_when_missing() {
    let space = default_space();
    let today = jiff::Zoned::now().date();
    let note_path = space
        .path()
        .join(format!("Journal/{}.md", today.strftime("%Y-%m-%d")));

    // Note does not exist yet
    assert!(!note_path.exists(), "note should not exist before test");

    sb_in(&space)
        .args(["daily", "--append", "first entry"])
        .assert()
        .success();

    assert!(note_path.exists(), "note should be created");
    let content = std::fs::read_to_string(&note_path).expect("read note");
    assert!(
        content.contains("first entry"),
        "note should contain the appended text; got: {content:?}"
    );
}

/// `sb daily` (no flags) with `$EDITOR` unset returns exit code 1 with a
/// helpful error message.
#[test]
fn daily_no_editor_set_exits_with_error() {
    let space = default_space();

    sb_in(&space)
        .env_remove("EDITOR")
        .arg("daily")
        .assert()
        .failure()
        .stderr(predicate::str::contains("editor").or(predicate::str::contains("EDITOR")));
}

/// Custom path template `Notes/Daily/{{date}}` with format `%Y/%m/%d` creates
/// the note at the correct nested path.
#[test]
fn daily_custom_path_template_creates_note_at_custom_path() {
    let space = setup_space(
        r#"server_url = "https://sb.example.com"
[daily]
path = "Notes/Daily/{{date}}"
dateFormat = "%Y/%m/%d"
"#,
    );

    let today = jiff::Zoned::now().date();
    let year = today.strftime("%Y").to_string();
    let month = today.strftime("%m").to_string();
    let day = today.strftime("%d").to_string();
    let expected_path = format!("Notes/Daily/{}/{}/{}.md", year, month, day);

    sb_in(&space)
        .args(["daily", "--append", "custom path test"])
        .assert()
        .success();

    let note_path = space.path().join(&expected_path);
    assert!(
        note_path.exists(),
        "note should be at custom path {expected_path}"
    );
}

/// When `daily.template` is configured and points to an existing local file,
/// the new daily note is created with that template's content.
#[test]
fn daily_uses_local_template_when_note_does_not_exist() {
    let space = setup_space(
        r#"server_url = "https://sb.example.com"
[daily]
template = "Templates/DailyTemplate"
"#,
    );

    // Create the template file
    let template_dir = space.path().join("Templates");
    std::fs::create_dir_all(&template_dir).expect("create Templates dir");
    std::fs::write(
        template_dir.join("DailyTemplate.md"),
        "# Daily\n\n## Goals\n\n## Notes\n",
    )
    .expect("write template");

    let today = jiff::Zoned::now().date();
    let note_path = space
        .path()
        .join(format!("Journal/{}.md", today.strftime("%Y-%m-%d")));

    sb_in(&space)
        .env_remove("EDITOR")
        .args(["daily", "--append", "after template"])
        .assert()
        .success();

    assert!(note_path.exists(), "note should be created");
    let content = std::fs::read_to_string(&note_path).expect("read note");
    assert!(
        content.contains("# Daily"),
        "template content should be in note; got: {content:?}"
    );
    assert!(
        content.contains("after template"),
        "appended text should follow template; got: {content:?}"
    );
}

/// `sb daily --offset 2 --append TEXT` creates a note 2 days in the future.
#[test]
fn daily_append_positive_offset_targets_future_date() {
    use jiff::ToSpan;

    let space = default_space();
    let target = jiff::Zoned::now()
        .date()
        .checked_add(2i32.days())
        .expect("date arithmetic");
    let expected_filename = format!("Journal/{}.md", target.strftime("%Y-%m-%d"));

    sb_in(&space)
        .args(["daily", "--offset", "2", "--append", "future entry"])
        .assert()
        .success();

    let note_path = space.path().join(&expected_filename);
    assert!(
        note_path.exists(),
        "note 2 days in future should be at {expected_filename}"
    );
}
