/// Integration tests for `sb search`, against a wiremock stand-in for the
/// server's `/.search/query` endpoint.
mod common;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn setup_space(server_url: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let sb_dir = dir.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).unwrap();
    std::fs::write(
        sb_dir.join("config.toml"),
        format!("server_url = \"{server_url}\"\ntoken = \"testtoken\"\n"),
    )
    .unwrap();
    dir
}

fn sb_in(dir: &std::path::Path) -> Command {
    let mut cmd = common::sb().unwrap();
    cmd.current_dir(dir)
        .env("XDG_CONFIG_HOME", "/nonexistent-sb-test-xdg");
    cmd
}

fn hits() -> serde_json::Value {
    json!({
        "items": [{
            "page": "Garden/Tea", "pos": 120, "line": 7, "heading": "Brewing",
            "excerpt": "steep green tea at 80C", "highlights": [[12, 15]]
        }],
        "total": 1, "partial": false, "notice": null
    })
}

#[tokio::test]
async fn search_sends_the_query_and_flags_and_prints_hits_as_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.search/query"))
        .and(body_json(json!({
            "query": "green tea", "mode": "phrase", "scope": "pages", "limit": 5
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(hits()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["--format", "json", "search", "green", "tea"])
        .args(["--mode", "phrase", "--scope", "pages", "--limit", "5"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"page\": \"Garden/Tea\""));
}

#[tokio::test]
async fn search_leaves_mode_and_scope_to_the_server_when_not_given() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.search/query"))
        .and(body_json(json!({"query": "tea", "limit": 20})))
        .respond_with(ResponseTemplate::new(200).set_body_json(hits()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["--format", "human", "search", "tea"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Garden/Tea:7"))
        .stdout(predicate::str::contains("Brewing"))
        .stdout(predicate::str::contains("steep green tea at 80C"));
}

#[tokio::test]
async fn search_on_a_server_without_search_exits_not_found_with_a_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.search/query"))
        .respond_with(ResponseTemplate::new(404).set_body_string("404 page not found\n"))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["search", "tea"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("3.0"));
}

async fn mock_search(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/.search/query"))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn search_with_a_rejected_token_exits_auth() {
    let server = MockServer::start().await;
    mock_search(&server, ResponseTemplate::new(401)).await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path()).args(["search", "tea"]).assert().code(3);
}

// An auth proxy's redirect to its login page arrives as a 200 HTML page once
// reqwest follows it. That is not "no matches".
#[tokio::test]
async fn search_fails_on_a_200_that_is_not_a_search_result() {
    let server = MockServer::start().await;
    mock_search(
        &server,
        ResponseTemplate::new(200).set_body_string("<html>login</html>"),
    )
    .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["--format", "json", "search", "tea"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty());
}

#[tokio::test]
async fn search_busy_shows_the_servers_message() {
    let server = MockServer::start().await;
    mock_search(
        &server,
        ResponseTemplate::new(429).set_body_json(json!({
            "error": "busy", "message": "Search is busy; try again in a moment"
        })),
    )
    .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["search", "tea"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Search is busy"));
}

#[tokio::test]
async fn search_keeps_a_multi_line_excerpt_indented_under_its_hit() {
    let server = MockServer::start().await;
    mock_search(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "items": [{"page": "P", "pos": 0, "line": 3, "excerpt": "first\nsecond", "highlights": []}],
            "total": 1, "partial": false, "notice": null
        })),
    )
    .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["--format", "human", "search", "tea"])
        .assert()
        .success()
        .stdout("P:3\n    first\n    second\n");
}

#[test]
fn search_limit_outside_what_the_server_returns_is_a_usage_error() {
    let dir = setup_space("http://127.0.0.1:9");
    for limit in ["0", "51"] {
        sb_in(dir.path())
            .args(["search", "tea", "--limit", limit])
            .assert()
            .code(2);
    }
}

// A pre-3.0 server has no route, and its catch-all only takes GET: 405.
#[tokio::test]
async fn search_on_a_pre_search_server_answering_405_gets_the_same_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.search/query"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["search", "tea"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("3.0"));
}

#[tokio::test]
async fn search_rejected_by_the_server_is_a_usage_error_with_its_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/.search/query"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "query_too_short", "message": "Search for at least 2 characters"
        })))
        .mount(&server)
        .await;
    let dir = setup_space(&server.uri());

    sb_in(dir.path())
        .args(["search", "t"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("at least 2 characters"));
}
