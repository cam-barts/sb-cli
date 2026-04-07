use console::Style;
use std::io::IsTerminal;

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

/// Print an error with colored "error:" prefix and optional hint.
/// Output goes to stderr.
pub fn print_error(error: &crate::error::SbError, color: bool) {
    eprintln!("{}", format_error(error, color));
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
