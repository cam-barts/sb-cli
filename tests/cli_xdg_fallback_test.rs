//! Regression tests for XDG-config fallback.
//!
//! When the per-space `.sb/config.toml` is missing or doesn't define a field,
//! `~/.config/sb/config.toml` should fill in. The reported bug: the user
//! deleted `~/.sb/config.toml` (a tiny pointer file) and expected the full
//! config in `~/.config/sb/config.toml` to keep working. It silently broke.
//!
//! These tests reconstruct that layout in a tempdir: a per-space `.sb/` with
//! only a minimal config, and an XDG file with the real settings. They then
//! exercise commands end-to-end and assert the XDG values were applied.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;
use tempfile::TempDir;

/// Set up a (space_root, xdg_root) pair.
///
/// `space_toml` becomes `<space>/.sb/config.toml`.
/// `xdg_toml` becomes `<xdg>/sb/config.toml` and that XDG path is what we'll
/// pass via the `XDG_CONFIG_HOME` env var to the subprocess.
fn setup(space_toml: &str, xdg_toml: &str) -> (TempDir, TempDir) {
    let space = tempfile::tempdir().expect("create space tempdir");
    std::fs::create_dir_all(space.path().join(".sb")).expect("create .sb");
    std::fs::write(space.path().join(".sb").join("config.toml"), space_toml)
        .expect("write per-space config");

    let xdg = tempfile::tempdir().expect("create xdg tempdir");
    let xdg_sb = xdg.path().join("sb");
    std::fs::create_dir_all(&xdg_sb).expect("create xdg/sb dir");
    std::fs::write(xdg_sb.join("config.toml"), xdg_toml).expect("write xdg config");

    (space, xdg)
}

fn sb_in(space: &Path, xdg: &Path) -> Command {
    let mut cmd = Command::cargo_bin("sb").expect("sb binary");
    cmd.current_dir(space).env("XDG_CONFIG_HOME", xdg);
    cmd
}

#[test]
fn daily_appends_pick_up_daily_path_from_xdg() {
    // The exact bug shape from the user report: minimal per-space file, all
    // settings live in XDG. We assert that `daily.path` (custom path template)
    // is read from XDG when the per-space file doesn't define it.
    //
    // Note: this test does NOT verify routing through sync.dir — that's a
    // separate fix on its own branch (fix/route-writes-through-content-dir).
    // Here we only verify the XDG layer is consulted.
    let (space, xdg) = setup(
        // Per-space file: server_url only, no [daily] block.
        "server_url = \"https://example.com\"\n",
        // XDG file: custom daily.path. If XDG fallback works, this wins over
        // the built-in default "Journal/{{date}}" (singular).
        r#"[daily]
path = "Journals/{{date}}"
"#,
    );

    sb_in(space.path(), xdg.path())
        .args(["daily", "--append", "wrote a thing"])
        .assert()
        .success();

    let today = jiff::Zoned::now().date();
    let from_xdg = space
        .path()
        .join("Journals")
        .join(format!("{}.md", today.strftime("%Y-%m-%d")));
    let from_default = space
        .path()
        .join("Journal")
        .join(format!("{}.md", today.strftime("%Y-%m-%d")));
    assert!(
        from_xdg.is_file(),
        "daily.path from XDG must be honored; expected {}\nspace tree:\n{}",
        from_xdg.display(),
        dump_tree(space.path()),
    );
    assert!(
        !from_default.exists(),
        "built-in default path must NOT be used when XDG defines daily.path; \
         found stray file at {}",
        from_default.display()
    );
}

/// Recursively list paths under `root` (relative). Used for failure diagnostics
/// when an expected file doesn't materialize where the test predicts.
fn dump_tree(root: &Path) -> String {
    let mut out = String::new();
    fn walk(path: &Path, root: &Path, out: &mut String) {
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let rel = p.strip_prefix(root).unwrap_or(&p);
            out.push_str("  ");
            out.push_str(&rel.display().to_string());
            out.push('\n');
            if p.is_dir() {
                walk(&p, root, out);
            }
        }
    }
    walk(root, root, &mut out);
    out
}

#[tokio::test]
async fn server_ping_resolves_token_from_xdg_when_per_space_omits_token() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.ping"))
        .and(header("authorization", "Bearer xdg-secret"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (space, xdg) = setup(
        // Per-space file: server_url only, no token.
        &format!("server_url = \"{}\"\n", server.uri()),
        // XDG file: the token.
        r#"token = "xdg-secret"
"#,
    );

    sb_in(space.path(), xdg.path())
        .args(["server", "ping"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ms"));
}

#[tokio::test]
async fn per_space_token_overrides_xdg_token() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // The mock only accepts the per-space token. If XDG's token leaks through
    // the precedence chain, the request will not match and the test will fail.
    Mock::given(method("GET"))
        .and(path("/.ping"))
        .and(header("authorization", "Bearer per-space-wins"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (space, xdg) = setup(
        &format!(
            "server_url = \"{}\"\ntoken = \"per-space-wins\"\n",
            server.uri()
        ),
        r#"token = "xdg-loses"
"#,
    );

    sb_in(space.path(), xdg.path())
        .args(["server", "ping"])
        .assert()
        .success();
}

#[test]
fn missing_both_files_fails_with_error_mentioning_xdg() {
    // No per-space file, no XDG sb config: the error surface should mention
    // both the per-space and XDG paths so the user understands the lookup order.
    let space = tempfile::tempdir().expect("create space tempdir");
    let xdg = tempfile::tempdir().expect("create xdg tempdir");
    // Empty per-space config (server_url but no token) so we get past space
    // resolution and into the token-resolver error path.
    std::fs::create_dir_all(space.path().join(".sb")).expect("create .sb");
    std::fs::write(
        space.path().join(".sb/config.toml"),
        "server_url = \"https://example.com\"\n",
    )
    .expect("write per-space");

    Command::cargo_bin("sb")
        .expect("sb binary")
        .env("XDG_CONFIG_HOME", xdg.path())
        .env_remove("SB_TOKEN")
        .env_remove("SB_SPACE")
        .current_dir(space.path())
        .args(["server", "ping"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(".config/sb/config.toml"));
}
