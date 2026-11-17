//! `[template]` config section: `dir` narrows discovery and qualifies bare
//! template names; `default` is the template used when `--template` is omitted.
//!
//! All offline (`runtime.available` defaults to false), so discovery takes the
//! local frontmatter-scan path and nothing talks to a server.

mod common;

use assert_cmd::Command;
use tempfile::TempDir;

/// A space whose config sets `[template] dir`/`default`, seeded with one
/// template inside that folder and one decoy outside it.
fn template_space(template_section: &str) -> TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
    std::fs::write(
        sb_dir.join("config.toml"),
        format!(
            r#"server_url = "https://sb.example.com"
token = "test-token"
[sync]
dir = "."
{template_section}"#
        ),
    )
    .expect("write config.toml");

    std::fs::create_dir_all(dir.path().join("Templates")).expect("create Templates dir");
    std::fs::write(
        dir.path().join("Templates").join("Project.md"),
        "---\ntags: meta/template/page\ndescription: A project\n---\n# In the template dir\n",
    )
    .expect("seed in-dir template");
    std::fs::write(
        dir.path().join("Stray.md"),
        "---\ntags: meta/template/page\n---\n# Outside the template dir\n",
    )
    .expect("seed out-of-dir template");
    dir
}

fn sb_in(dir: &TempDir) -> Command {
    let mut cmd = common::sb().expect("sb binary");
    cmd.current_dir(dir.path())
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg");
    cmd
}

#[test]
fn template_list_without_dir_shows_every_template() {
    let space = template_space("");
    let out = sb_in(&space)
        .args(["template", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = String::from_utf8(out).expect("utf8");
    assert!(json.contains("Templates/Project"), "got: {json}");
    assert!(json.contains("Stray"), "got: {json}");
}

#[test]
fn template_list_is_narrowed_by_template_dir() {
    let space = template_space("[template]\ndir = \"Templates\"\n");
    let out = sb_in(&space)
        .args(["template", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = String::from_utf8(out).expect("utf8");
    assert!(json.contains("Templates/Project"), "got: {json}");
    assert!(
        !json.contains("Stray"),
        "a template outside template.dir must be filtered out, got: {json}"
    );
}

#[test]
fn template_new_uses_template_default_and_resolves_bare_name() {
    // No `--template` flag: `template.default` ("Project") supplies the name,
    // and `template.dir` qualifies it to `Templates/Project`.
    let space = template_space("[template]\ndir = \"Templates\"\ndefault = \"Project\"\n");
    sb_in(&space)
        .args(["template", "new", "Projects/Thing", "--no-edit"])
        .assert()
        .success();
    let page =
        std::fs::read_to_string(space.path().join("Projects").join("Thing.md")).expect("new page");
    assert!(page.contains("In the template dir"), "got: {page:?}");
}

#[test]
fn template_dir_qualifies_an_explicit_bare_template_name() {
    let space = template_space("[template]\ndir = \"Templates\"\n");
    sb_in(&space)
        .args([
            "template",
            "new",
            "Projects/Thing",
            "--template",
            "Project",
            "--no-edit",
        ])
        .assert()
        .success();
    let page =
        std::fs::read_to_string(space.path().join("Projects").join("Thing.md")).expect("new page");
    assert!(page.contains("In the template dir"), "got: {page:?}");
}

#[test]
fn fully_qualified_template_name_still_works_with_template_dir_set() {
    let space = template_space("[template]\ndir = \"Templates\"\n");
    sb_in(&space)
        .args([
            "template",
            "new",
            "Projects/Thing",
            "--template",
            "Templates/Project",
            "--no-edit",
        ])
        .assert()
        .success();
    let page =
        std::fs::read_to_string(space.path().join("Projects").join("Thing.md")).expect("new page");
    assert!(page.contains("In the template dir"), "got: {page:?}");
}
