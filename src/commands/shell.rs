use crate::commands::server::build_client;
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};

#[derive(serde::Serialize)]
struct ShellRequest<'a> {
    cmd: &'a str,
    args: Vec<&'a str>,
}

#[derive(serde::Deserialize)]
struct ShellResponse {
    stdout: String,
    stderr: String,
    code: i32,
}

pub async fn execute(
    cli_token: Option<&str>,
    command: &[String],
    _quiet: bool,
    _color: bool,
) -> SbResult<()> {
    let space_root = crate::commands::page::find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;

    // Check shell.enabled gate
    if !config.shell_enabled.value {
        eprintln!("Warning: sb shell allows arbitrary command execution on the server host.");
        eprintln!("This feature is disabled by default for security.");
        eprintln!();
        eprintln!("To enable, add to .sb/config.toml:");
        eprintln!("  [shell]");
        eprintln!("  enabled = true");
        return Err(SbError::Usage("shell.enabled is not set to true".into()));
    }

    // Validate command is not empty
    if command.is_empty() {
        return Err(SbError::Usage(
            "no command provided. Usage: sb shell <command> [args...]".into(),
        ));
    }

    let cmd = &command[0];
    let args: Vec<&str> = command[1..].iter().map(|s| s.as_str()).collect();

    let client = build_client(cli_token)?;

    let request_body = ShellRequest { cmd, args };

    // Send POST /.shell with JSON body
    let resp = client.post_json("/.shell", &request_body).await?;
    let status = resp.status();
    let url = format!("{}/.shell", client.base_url());
    let resp_body = resp.text().await.map_err(|e| SbError::HttpStatus {
        status: status.as_u16(),
        url: url.clone(),
        body: format!("failed to read shell response: {e}"),
    })?;

    let shell_resp: ShellResponse =
        serde_json::from_str(&resp_body).map_err(|e| SbError::HttpStatus {
            status: status.as_u16(),
            url,
            body: format!("invalid shell response JSON: {e}"),
        })?;

    // Print stdout to stdout, stderr to stderr
    if !shell_resp.stdout.is_empty() {
        print!("{}", shell_resp.stdout);
    }
    if !shell_resp.stderr.is_empty() {
        eprint!("{}", shell_resp.stderr);
    }

    // Non-zero exit code = propagate as ProcessFailed
    if shell_resp.code != 0 {
        return Err(SbError::ProcessFailed {
            code: shell_resp.code,
            stderr: shell_resp.stderr,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{make_space, SbSpaceGuard};
    use wiremock::matchers::{body_json_string, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn enable_shell(space_root: &std::path::Path) {
        crate::config::update_config_value(&space_root.join(".sb"), "shell", "enabled", true)
            .unwrap();
    }

    #[tokio::test]
    async fn errors_when_shell_disabled_by_default() {
        // Default config: shell.enabled = false. Must refuse before any HTTP.
        let tmp = make_space(Some("http://127.0.0.1:1"));
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, &["echo".to_string(), "hi".to_string()], true, false)
            .await
            .unwrap_err();
        assert!(matches!(err, SbError::Usage(_)));
    }

    #[tokio::test]
    async fn errors_when_command_is_empty() {
        let tmp = make_space(Some("http://127.0.0.1:1"));
        enable_shell(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, &[], true, false).await.unwrap_err();
        match err {
            SbError::Usage(msg) => assert!(msg.contains("no command")),
            other => panic!("expected Usage, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn sends_cmd_and_args_as_json_to_dot_shell() {
        let server = MockServer::start().await;
        // Wiremock matches an exact JSON body — proves we serialize cmd+args correctly.
        Mock::given(method("POST"))
            .and(path("/.shell"))
            .and(body_json_string(
                r#"{"cmd":"echo","args":["hello","world"]}"#,
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"stdout":"hello world\n","stderr":"","code":0}"#),
            )
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_shell(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let res = execute(
            None,
            &["echo".to_string(), "hello".to_string(), "world".to_string()],
            true,
            false,
        )
        .await;
        assert!(res.is_ok(), "{res:?}");
    }

    #[tokio::test]
    async fn nonzero_exit_code_returns_process_failed_with_code_and_stderr() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.shell"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"stdout":"","stderr":"oops","code":2}"#),
            )
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_shell(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, &["false".to_string()], true, false)
            .await
            .unwrap_err();
        match err {
            SbError::ProcessFailed { code, stderr } => {
                assert_eq!(code, 2);
                assert_eq!(stderr, "oops");
            }
            other => panic!("expected ProcessFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_json_response_returns_http_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/.shell"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let tmp = make_space(Some(&server.uri()));
        enable_shell(tmp.path());
        let _g = SbSpaceGuard::set(tmp.path());

        let err = execute(None, &["x".to_string()], true, false)
            .await
            .unwrap_err();
        match err {
            SbError::HttpStatus { body, .. } => {
                assert!(body.contains("invalid shell response JSON"))
            }
            other => panic!("expected HttpStatus, got: {other:?}"),
        }
    }
}
