use crate::cli::OutputFormat;
use crate::commands::server::{build_client, require_initialized, runtime_unavailable_error};
use crate::error::{SbError, SbResult};
use crate::output;
use jiff::Timestamp;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Execute `sb screenshot` — fetch a PNG of the SilverBullet headless
/// browser's current state.
///
/// Output destination rules:
/// - `output == Some("-")`: write raw PNG to stdout.
/// - `output == Some(path)`: write to `path`.
/// - `output == None` and stdout is a TTY: write to `./sb-screenshot-<UTC>.png`.
/// - `output == None` and stdout is piped: write raw PNG to stdout.
///
/// When `format == Json`, instead of writing the PNG, emit a JSON envelope
/// like `{"path": "...", "bytes": N}` to stdout. The PNG itself is still
/// written to disk unless the caller passes `-` (which is mutually exclusive
/// with `--format json`).
pub async fn execute(
    cli_token: Option<&str>,
    output_path: Option<&str>,
    format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    require_initialized()?;

    if matches!(format, OutputFormat::Json) && output_path == Some("-") {
        return Err(SbError::Usage(
            "--format json is incompatible with --output - (stdout PNG)".into(),
        ));
    }

    let client = build_client(cli_token)?;
    let bytes = match client.get_runtime_screenshot().await {
        Ok(b) => b,
        Err(SbError::HttpStatus { status: 503, .. }) => {
            return Err(runtime_unavailable_error());
        }
        Err(e) => return Err(e),
    };

    let resolved = resolve_destination(output_path, output::is_tty());

    match resolved {
        Destination::Stdout => {
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|e| SbError::Filesystem {
                    message: "failed to write screenshot to stdout".into(),
                    path: "<stdout>".into(),
                    source: Some(e),
                })?;
        }
        Destination::File(ref path) => {
            std::fs::write(path, &bytes).map_err(|e| SbError::Filesystem {
                message: "failed to write screenshot".into(),
                path: path.display().to_string(),
                source: Some(e),
            })?;
        }
    }

    match format {
        OutputFormat::Json => {
            // Stdout path can only happen when --format human -- already
            // validated above. Always print a file envelope here.
            let path = match &resolved {
                Destination::File(p) => p.display().to_string(),
                Destination::Stdout => "-".into(),
            };
            let json = serde_json::json!({
                "path": path,
                "bytes": bytes.len(),
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        OutputFormat::Human => {
            if let Destination::File(p) = &resolved {
                output::print_success(
                    &format!(
                        "Saved screenshot to {} ({} bytes)",
                        p.display(),
                        bytes.len()
                    ),
                    color,
                    quiet,
                );
            }
            // Stdout destination is silent on success -- caller piped it.
        }
    }

    Ok(())
}

/// Where the PNG bytes should land.
#[derive(Debug, Clone)]
pub(crate) enum Destination {
    Stdout,
    File(PathBuf),
}

/// Resolve the destination based on the `--output` flag and whether stdout
/// is a TTY. Pure function for unit testing.
pub(crate) fn resolve_destination(output_arg: Option<&str>, stdout_is_tty: bool) -> Destination {
    match output_arg {
        Some("-") => Destination::Stdout,
        Some(p) => Destination::File(PathBuf::from(p)),
        None => {
            if stdout_is_tty {
                Destination::File(default_filename())
            } else {
                Destination::Stdout
            }
        }
    }
}

/// Build the default screenshot filename: `./sb-screenshot-<UTC ISO 8601>.png`.
/// Colons in the timestamp are replaced with `-` for Windows compatibility.
pub(crate) fn default_filename() -> PathBuf {
    let ts = Timestamp::now().to_string();
    let safe = ts.replace([':', '.'], "-");
    Path::new(".").join(format!("sb-screenshot-{safe}.png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_destination_dash_means_stdout() {
        match resolve_destination(Some("-"), true) {
            Destination::Stdout => {}
            other => panic!("expected Stdout, got {other:?}"),
        }
    }

    #[test]
    fn resolve_destination_explicit_path_used_verbatim() {
        match resolve_destination(Some("/tmp/x.png"), true) {
            Destination::File(p) => assert_eq!(p, PathBuf::from("/tmp/x.png")),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn resolve_destination_no_arg_tty_uses_default_filename() {
        match resolve_destination(None, true) {
            Destination::File(p) => {
                let s = p.to_string_lossy().to_string();
                assert!(
                    s.contains("sb-screenshot-"),
                    "default filename should contain prefix, got {s}"
                );
                assert!(s.ends_with(".png"), "default filename should end in .png");
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn resolve_destination_no_arg_piped_uses_stdout() {
        match resolve_destination(None, false) {
            Destination::Stdout => {}
            other => panic!("expected Stdout when piped, got {other:?}"),
        }
    }

    #[test]
    fn default_filename_is_safe_for_windows() {
        let p = default_filename();
        let s = p.to_string_lossy().to_string();
        assert!(
            !s.contains(':'),
            "filename must not contain ':' (Windows reserved)"
        );
    }

    mod execute_tests {
        use super::super::*;
        use crate::test_util::{make_space, SbSpaceGuard};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        #[tokio::test]
        async fn execute_errors_when_json_format_with_dash_stdout_output() {
            let tmp = make_space(Some("http://127.0.0.1:1"));
            let _g = SbSpaceGuard::set(tmp.path());
            let err = execute(None, Some("-"), &OutputFormat::Json, true, false)
                .await
                .unwrap_err();
            match err {
                SbError::Usage(msg) => {
                    assert!(msg.contains("incompatible"), "got: {msg}")
                }
                other => panic!("expected Usage, got: {other:?}"),
            }
        }

        #[tokio::test]
        async fn execute_503_from_screenshot_becomes_runtime_unavailable() {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/.runtime/screenshot"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            let _g = SbSpaceGuard::set(tmp.path());
            let out = tmp.path().join("shot.png");
            let err = execute(
                None,
                Some(out.to_str().unwrap()),
                &OutputFormat::Human,
                true,
                false,
            )
            .await
            .unwrap_err();
            assert!(format!("{err}").contains("Runtime API not available"));
        }

        #[tokio::test]
        async fn execute_writes_png_to_file_and_emits_json_envelope() {
            let server = MockServer::start().await;
            let png_bytes = b"\x89PNG\r\n\x1a\nfake-body";
            Mock::given(method("GET"))
                .and(path("/.runtime/screenshot"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(png_bytes.to_vec()))
                .mount(&server)
                .await;
            let tmp = make_space(Some(&server.uri()));
            let _g = SbSpaceGuard::set(tmp.path());
            let out = tmp.path().join("shot.png");

            execute(
                None,
                Some(out.to_str().unwrap()),
                &OutputFormat::Json,
                true,
                false,
            )
            .await
            .expect("succeed");

            let written = std::fs::read(&out).unwrap();
            assert_eq!(written.as_slice(), png_bytes);
        }
    }
}
