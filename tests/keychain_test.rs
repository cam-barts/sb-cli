mod common;

use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

/// Helper: create a temp space with .sb/config.toml and .sb/state.db
fn setup_space(config_content: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    fs::create_dir_all(&sb_dir).unwrap();
    fs::write(sb_dir.join("config.toml"), config_content).unwrap();
    let conn = rusqlite::Connection::open(sb_dir.join("state.db")).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    dir
}

/// Verify that resolve_token with auth.keychain=true includes "OS keychain"
/// in the error message when no token is found from any source.
#[test]
fn resolve_token_error_mentions_keychain_when_enabled() {
    let dir = setup_space(
        r#"
server_url = "https://example.com"
[auth]
keychain = true
"#,
    );
    // Run any command that requires auth -- should fail with TokenNotFound
    // listing all checked sources including "OS keychain"
    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .current_dir(dir.path())
        .args(["server", "ping"])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("OS keychain"));
}

/// Verify that resolve_token with auth.keychain=false does NOT mention keychain
/// in the error message.
#[test]
fn resolve_token_error_omits_keychain_when_disabled() {
    let dir = setup_space(
        r#"
server_url = "https://example.com"
"#,
    );
    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .current_dir(dir.path())
        .args(["server", "ping"])
        .assert()
        // Must be the TokenNotFound path (exit 3 + source list), or "omits the
        // keychain" passes vacuously on any other failure.
        .code(3)
        .stderr(predicates::str::contains("SB_TOKEN environment variable"))
        .stderr(predicates::str::contains("OS keychain").not());
}

/// IGNORED: Real keychain test -- set and get a token.
/// Only run manually on a system with a working keychain:
///   cargo test keychain_roundtrip -- --ignored
#[test]
#[ignore]
fn keychain_roundtrip_set_get_delete() {
    let server_url = "https://sb-cli-test.example.com";
    let token = "test-token-roundtrip-12345";

    // Set
    sb_cli::keychain::set_token(server_url, token).expect("set_token should succeed");

    // Get
    let retrieved = sb_cli::keychain::get_token(server_url).expect("get_token should succeed");
    assert_eq!(retrieved, Some(token.to_string()));

    // Delete
    sb_cli::keychain::delete_token(server_url).expect("delete_token should succeed");

    // Verify gone
    let after_delete =
        sb_cli::keychain::get_token(server_url).expect("get_token after delete should succeed");
    assert_eq!(after_delete, None);
}

/// IGNORED: Real keychain test -- auth set with keychain enabled.
/// Only run manually:
///   cargo test keychain_auth_set -- --ignored
#[test]
#[ignore]
fn keychain_auth_set_stores_in_keychain() {
    let dir = setup_space(
        r#"
server_url = "https://sb-cli-test-authset.example.com"
[auth]
keychain = true
"#,
    );

    common::sb()
        .unwrap()
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg")
        .current_dir(dir.path())
        .args(["auth", "set", "--token", "keychain-stored-token"])
        .assert()
        .success()
        .stderr(predicates::str::contains("keychain"));

    // Verify token is in keychain
    let token = sb_cli::keychain::get_token("https://sb-cli-test-authset.example.com")
        .expect("get_token should succeed");
    assert_eq!(token, Some("keychain-stored-token".to_string()));

    // Cleanup
    sb_cli::keychain::delete_token("https://sb-cli-test-authset.example.com").ok();

    // Verify token is NOT in config.toml (should not have been written there)
    let config = fs::read_to_string(dir.path().join(".sb/config.toml")).unwrap();
    assert!(
        !config.contains("keychain-stored-token"),
        "token should NOT be in config.toml when keychain is enabled"
    );
}
