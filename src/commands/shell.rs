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
