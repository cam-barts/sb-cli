use crate::error::SbResult;
use console::Style;
use std::io::Write;
use tracing::debug;

pub fn execute(quiet: bool, color: bool) -> SbResult<()> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    execute_to(&mut handle, quiet, color)
}

/// Writer-based variant for testing. Quiet writes nothing.
pub fn execute_to(out: &mut dyn Write, quiet: bool, color: bool) -> SbResult<()> {
    debug!("executing version command (quiet={quiet}, color={color})");

    if quiet {
        return Ok(());
    }

    let version_style = if color {
        Style::new().bold()
    } else {
        Style::new()
    };
    let label_style = if color {
        Style::new().dim()
    } else {
        Style::new()
    };

    writeln!(
        out,
        "{}",
        version_style.apply_to(format!("sb {}", env!("CARGO_PKG_VERSION")))
    )
    .ok();
    writeln!(
        out,
        "{} {}",
        label_style.apply_to("commit:"),
        env!("SB_GIT_HASH")
    )
    .ok();
    writeln!(
        out,
        "{} {}",
        label_style.apply_to("built: "),
        env!("SB_BUILD_DATE")
    )
    .ok();
    writeln!(
        out,
        "{} {}",
        label_style.apply_to("target:"),
        env!("SB_TARGET")
    )
    .ok();
    writeln!(
        out,
        "{} {}",
        label_style.apply_to("features:"),
        compiled_features()
    )
    .ok();
    Ok(())
}

/// The set of opt-in features compiled into this binary, so a human or agent can
/// tell a slim build from an `ai` build. Prints `none` for the default build.
pub fn compiled_features() -> String {
    let mut features = Vec::new();
    if cfg!(feature = "skills") {
        features.push("skills");
    }
    if cfg!(feature = "mcp") {
        features.push("mcp");
    }
    if features.is_empty() {
        "none".to_string()
    } else {
        features.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_writes_nothing_to_output() {
        let mut buf = Vec::new();
        execute_to(&mut buf, true, false).unwrap();
        assert!(buf.is_empty(), "quiet flag should produce zero output");
    }

    #[test]
    fn output_includes_version_commit_built_target_labels() {
        let mut buf = Vec::new();
        execute_to(&mut buf, false, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Each line is a distinct documented label; verify presence so a refactor
        // that drops one would fail loudly.
        for marker in ["sb ", "commit:", "built:", "target:"] {
            assert!(
                s.contains(marker),
                "expected {marker:?} in output, got:\n{s}"
            );
        }
    }

    #[test]
    fn output_with_color_contains_ansi_escapes() {
        console::set_colors_enabled(true);
        let mut buf = Vec::new();
        execute_to(&mut buf, false, true).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\x1b["), "color mode should emit ANSI escapes");
    }

    #[test]
    fn output_without_color_contains_no_ansi_escapes() {
        console::set_colors_enabled(false);
        let mut buf = Vec::new();
        execute_to(&mut buf, false, false).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains("\x1b["),
            "no-color mode must not emit ANSI escapes"
        );
    }
}
