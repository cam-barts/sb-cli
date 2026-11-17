mod common;

/// Run `sb <args>` under a pseudo-terminal (util-linux `script`) so stdout is
/// a TTY, the only condition under which colour is ever enabled. Returns None
/// when `script` is unavailable so the caller can skip.
#[cfg(target_os = "linux")]
fn run_on_a_tty(args: &str, extra_env: &[(&str, &str)]) -> Option<String> {
    let sb = common::sb().unwrap();
    let cmdline = format!("{} {}", sb.get_program().to_string_lossy(), args);
    // `script` is a fresh process: the SB_* strip on `sb` above doesn't reach
    // it, so strip again here.
    let mut script = std::process::Command::new("script");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("SB_") {
            script.env_remove(key);
        }
    }
    let out = script
        .args(["-qec", &cmdline, "/dev/null"])
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        // Inherited colour/log settings would skew either leg: CLICOLOR=0 kills
        // the positive case, RUST_LOG adds tracing's own ANSI to the PTY.
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("RUST_LOG")
        .envs(extra_env.iter().copied())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `sb version` styles its labels, but only when colour is on. Colour needs a
/// TTY, so piped runs (every other test) can never show it; this drives a real
/// PTY and checks it is present by default and removed by `--no-color` (sb's
/// own `should_color`). The `NO_COLOR` leg checks the user-visible result
/// only: the console crate honours NO_COLOR itself, so that leg would pass
/// even if sb's NO_COLOR branch were removed.
#[cfg(target_os = "linux")]
#[test]
fn colour_is_on_for_a_tty_and_off_with_no_color_flag_or_env() {
    let Some(coloured) = run_on_a_tty("version", &[]) else {
        eprintln!("skipping: `script` not available");
        return;
    };
    assert!(
        coloured.contains("\x1b["),
        "a TTY should get coloured output, got: {coloured:?}"
    );

    let flagged = run_on_a_tty("--no-color version", &[]).unwrap();
    assert!(
        flagged.contains("sb "),
        "command should still run: {flagged:?}"
    );
    assert!(
        !flagged.contains("\x1b["),
        "--no-color must remove ANSI, got: {flagged:?}"
    );

    let env_off = run_on_a_tty("version", &[("NO_COLOR", "1")]).unwrap();
    assert!(
        env_off.contains("sb "),
        "command should still run: {env_off:?}"
    );
    assert!(
        !env_off.contains("\x1b["),
        "NO_COLOR must remove ANSI, got: {env_off:?}"
    );
}

#[test]
fn verbose_flag_emits_debug_lines_on_stderr_and_default_does_not() {
    let verbose = common::sb()
        .unwrap()
        .args(["--verbose", "version"])
        .output()
        .unwrap();
    assert!(verbose.status.success());
    let stderr = String::from_utf8(verbose.stderr).unwrap();
    assert!(
        stderr.contains("DEBUG") && stderr.contains("dispatching: version"),
        "--verbose should log DEBUG lines, got: {stderr:?}"
    );

    let quiet = common::sb()
        .unwrap()
        .env_remove("RUST_LOG")
        .arg("version")
        .output()
        .unwrap();
    assert!(quiet.status.success());
    let stderr = String::from_utf8(quiet.stderr).unwrap();
    assert!(
        !stderr.contains("DEBUG"),
        "no DEBUG lines without --verbose, got: {stderr:?}"
    );
}
