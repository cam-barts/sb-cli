use console::Style;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

/// Process-global interaction flags, set once in `main` from `--no-input`/`--yes`
/// (mirroring how color is a process-global toggle). Interactive helpers consult
/// these instead of every command threading the flags through its signature.
static NO_INPUT: AtomicBool = AtomicBool::new(false);
static ASSUME_YES: AtomicBool = AtomicBool::new(false);

/// Record the `--no-input` flag. Call once during startup.
pub fn set_no_input(value: bool) {
    NO_INPUT.store(value, Ordering::Relaxed);
}

/// Record the `--yes`/`--force`-style assume-yes flag. Call once during startup.
pub fn set_assume_yes(value: bool) {
    ASSUME_YES.store(value, Ordering::Relaxed);
}

/// True when interaction is disabled: `--no-input` was passed, or stdin is not a
/// terminal (an agent/pipe can't answer a prompt either way).
pub fn no_input() -> bool {
    NO_INPUT.load(Ordering::Relaxed) || !std::io::stdin().is_terminal()
}

/// True when destructive operations should proceed without an interactive
/// confirmation (the user passed `--yes` or a command-level `--force`).
pub fn assume_yes() -> bool {
    ASSUME_YES.load(Ordering::Relaxed)
}

/// Central output configuration derived from CLI flags and environment
pub struct OutputConfig {
    pub quiet: bool,
    pub verbose: bool,
    pub color: bool,
}

impl OutputConfig {
    pub fn new(quiet: bool, verbose: bool, no_color_flag: bool) -> Self {
        let color = should_color(no_color_flag);
        Self {
            quiet,
            verbose,
            color,
        }
    }
}

/// Determine if color output should be enabled.
/// Color is OFF if any of: --no-color flag, NO_COLOR env var set (any value), stdout not a TTY.
///
pub fn should_color(no_color_flag: bool) -> bool {
    if no_color_flag {
        return false;
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Check if stdout is a TTY (suppress progress/color when piped)
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Resolve the user-facing output format.
///
/// When the user passes `--format <value>` explicitly, that value wins
/// (this is what an opaque `Some(_)` represents). Otherwise we default to
/// `Human` if stdout is a TTY and `Json` when piped, so that piping into
/// `jq` / `grep` "just works" while interactive usage stays pretty.
pub fn resolve_format(explicit: Option<crate::cli::OutputFormat>) -> crate::cli::OutputFormat {
    if let Some(f) = explicit {
        return f;
    }
    if is_tty() {
        crate::cli::OutputFormat::Human
    } else {
        crate::cli::OutputFormat::Json
    }
}

/// Print an error to stderr. In `Human` mode this is a colored `error:` line
/// with the source chain and an optional hint; in `Json` mode it is a single
/// structured object `{ "error", "code", "remediation" }` so an agent gets a
/// parseable failure body in addition to the process exit code. Errors always
/// go to stderr, never stdout, so a piped `--format json` data stream stays
/// clean even on failure.
pub fn print_error(error: &crate::error::SbError, color: bool, format: &crate::cli::OutputFormat) {
    match format {
        crate::cli::OutputFormat::Json => eprintln!("{}", format_error_json(error)),
        crate::cli::OutputFormat::Human => eprintln!("{}", format_error(error, color)),
    }
}

/// Render an error as a compact JSON object: `{ "error", "code", "remediation" }`.
/// `remediation` is `null` when the error carries no actionable hint.
pub fn format_error_json(error: &crate::error::SbError) -> String {
    let payload = serde_json::json!({
        "error": error.to_string(),
        "code": error.code_str(),
        "remediation": error.hint(),
    });
    // Compact single-line object — trivially serializable, unwrap is safe.
    serde_json::to_string(&payload).unwrap()
}

/// Print a success message (green label, plain data).
/// Respects quiet flag -- returns immediately if quiet.
pub fn print_success(message: &str, color: bool, quiet: bool) {
    if quiet {
        return;
    }
    if color {
        let style = Style::new().green();
        eprintln!("{}", style.apply_to(message));
    } else {
        eprintln!("{}", message);
    }
}

/// Print a warning message (yellow label) to stderr.
pub fn print_warning(message: &str, color: bool, quiet: bool) {
    if quiet {
        return;
    }
    let prefix = if color {
        Style::new()
            .yellow()
            .bold()
            .apply_to("warning:")
            .to_string()
    } else {
        "warning:".to_string()
    };
    eprintln!("{} {}", prefix, message);
}

/// Initialize tracing-subscriber for --verbose logging.
/// When verbose=true: DEBUG level to stderr.
/// When verbose=false: WARN level to stderr (unless RUST_LOG is set).
/// Always outputs to stderr so stdout stays clean for data.
pub fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;

    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

/// Format error output to a string buffer (for testing).
/// Returns the formatted error string as it would appear in stderr.
pub fn format_error(error: &crate::error::SbError, color: bool) -> String {
    let err_style = if color {
        Style::new().red().bold()
    } else {
        Style::new()
    };
    let hint_style = if color {
        Style::new().dim()
    } else {
        Style::new()
    };

    let mut output = format!("{} {}", err_style.apply_to("error:"), error);

    // Source chain
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        output.push_str(&format!("\n  {} {}", hint_style.apply_to("->"), cause));
        source = std::error::Error::source(cause);
    }

    // Actionable hint
    if let Some(hint) = error.hint() {
        output.push_str(&format!("\n  {} {}", hint_style.apply_to("->"), hint));
    }

    output
}

/// Detect if the filesystem at `path` is case-insensitive.
/// Creates a temp file with mixed case and checks if the lowercase variant exists.
/// Returns Ok(true) if case-insensitive, Ok(false) if case-sensitive.
/// Used during `sb init` to warn users about potential filename collisions.
pub fn detect_case_insensitive_fs(dir: &std::path::Path) -> std::io::Result<bool> {
    let probe_upper = dir.join(".sb_CaSe_PrObE");
    let probe_lower = dir.join(".sb_case_probe");

    // Clean up any leftover probes
    let _ = std::fs::remove_file(&probe_upper);
    let _ = std::fs::remove_file(&probe_lower);

    // Create file with mixed case
    std::fs::write(&probe_upper, "")?;

    // Check if lowercase path resolves to the same file
    let is_insensitive = probe_lower.exists();

    // Clean up
    let _ = std::fs::remove_file(&probe_upper);

    Ok(is_insensitive)
}

/// Check if a path is in the _plug/ directory.
/// Returns true for paths like "_plug/search.js", "_plug/nested/file.md".
/// Used during sync push to warn and skip plugin-managed files.
pub fn is_plug_path(path: &str) -> bool {
    path == "_plug" || path.starts_with("_plug/") || path.starts_with("_plug\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_color_returns_false_when_no_color_flag_set() {
        assert!(!should_color(true));
    }

    #[test]
    fn output_config_new_sets_quiet() {
        let config = OutputConfig::new(true, false, false);
        assert!(config.quiet);
        assert!(!config.verbose);
    }

    #[test]
    fn output_config_new_sets_verbose() {
        let config = OutputConfig::new(false, true, false);
        assert!(config.verbose);
        assert!(!config.quiet);
    }

    #[test]
    fn output_config_no_color_flag_disables_color() {
        let config = OutputConfig::new(false, false, true);
        assert!(!config.color);
    }

    #[test]
    fn format_error_with_color_contains_ansi_and_error_prefix() {
        // Force console crate to emit colors even in non-TTY test environment
        console::set_colors_enabled(true);
        console::set_colors_enabled_stderr(true);

        let err = crate::error::SbError::Config {
            message: "test error".to_string(),
        };
        let output = format_error(&err, true);
        assert!(
            output.contains("\x1b["),
            "colored output should contain ANSI escape"
        );
        assert!(
            output.contains("error:"),
            "output should contain error: prefix"
        );
    }

    #[test]
    fn format_error_without_color_contains_error_prefix_no_ansi() {
        let err = crate::error::SbError::Config {
            message: "test error".to_string(),
        };
        let output = format_error(&err, false);
        assert!(
            output.contains("error:"),
            "output should contain error: prefix"
        );
        assert!(
            !output.contains("\x1b["),
            "uncolored output should not contain ANSI escape"
        );
    }

    #[test]
    fn format_error_with_hint_contains_arrow() {
        let err = crate::error::SbError::Network {
            url: "http://localhost:3000".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connection refused",
            )),
        };
        let output = format_error(&err, false);
        assert!(
            output.contains("->"),
            "error with hint should contain -> arrow"
        );
    }

    #[test]
    fn format_error_json_has_error_code_remediation_fields() {
        let err = crate::error::SbError::PageNotFound {
            name: "missing".to_string(),
        };
        let json = format_error_json(&err);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["code"], "not_found");
        assert!(v["error"].as_str().unwrap().contains("missing"));
        assert!(
            v["remediation"].as_str().unwrap().contains("sb page list"),
            "remediation should carry the hint"
        );
    }

    #[test]
    fn format_error_json_remediation_is_null_without_hint() {
        let err = crate::error::SbError::Config {
            message: "bad config".to_string(),
        };
        let v: serde_json::Value = serde_json::from_str(&format_error_json(&err)).unwrap();
        assert_eq!(v["code"], "general");
        assert!(v["remediation"].is_null(), "no hint -> null remediation");
    }

    #[test]
    fn format_error_json_never_contains_ansi() {
        console::set_colors_enabled(true);
        let err = crate::error::SbError::Usage("bad".to_string());
        assert!(
            !format_error_json(&err).contains("\x1b["),
            "JSON errors must never carry ANSI codes"
        );
    }

    #[test]
    fn print_success_suppressed_when_quiet() {
        // Should not panic and should return immediately
        print_success("test", false, true);
    }

    #[test]
    fn print_warning_suppressed_when_quiet() {
        // Should not panic and should return immediately
        print_warning("test", false, true);
    }

    #[test]
    fn is_plug_path_matches_plug_directory() {
        assert!(is_plug_path("_plug/search.js"));
        assert!(is_plug_path("_plug/nested/file.md"));
        assert!(is_plug_path("_plug"));
    }

    #[test]
    fn is_plug_path_rejects_non_plug() {
        assert!(!is_plug_path("notes/regular.md"));
        assert!(!is_plug_path("my_plugin/file.md"));
        assert!(!is_plug_path("plugs/search.js"));
        assert!(!is_plug_path(""));
    }

    #[test]
    fn detect_case_insensitive_fs_works() {
        let tmpdir = tempfile::TempDir::new().unwrap();
        // This test will return true on Windows/macOS, false on Linux ext4
        // We just verify it doesn't error out
        let result = detect_case_insensitive_fs(tmpdir.path());
        assert!(result.is_ok());
    }
}
