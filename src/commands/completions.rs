//! Shell completion generation and installation.
//!
//! `sb completions <shell>` prints a completion script to stdout (composable
//! with a manual redirect). `sb completions [<shell>] --install` writes it to
//! the standard per-shell location, auto-detecting the shell from `$SHELL` when
//! it isn't given, and prints any follow-up step the shell needs.

use std::path::{Path, PathBuf};

use clap::CommandFactory;
use clap_complete::{generate, Shell};

use crate::cli::Cli;
use crate::error::{SbError, SbResult};

/// Entry point for `sb completions`.
pub fn execute(shell: Option<Shell>, install: bool, quiet: bool, color: bool) -> SbResult<()> {
    let shell = match shell {
        Some(s) => s,
        None if install => detect_shell()?,
        None => {
            return Err(SbError::Usage(
                "specify a shell: sb completions <bash|zsh|fish|elvish|powershell>".into(),
            ))
        }
    };

    if install {
        install_completions(shell, quiet, color)
    } else {
        let mut out = std::io::stdout();
        write_completions(shell, &mut out);
        Ok(())
    }
}

fn write_completions(shell: Shell, buf: &mut dyn std::io::Write) {
    let mut cmd = Cli::command();
    let bin = cmd.get_name().to_string();
    generate(shell, &mut cmd, bin, buf);
}

fn install_completions(shell: Shell, quiet: bool, color: bool) -> SbResult<()> {
    let (path, note) = completion_path(shell, &data_home()?, &config_home()?)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
            message: "failed to create completions directory".to_string(),
            path: parent.display().to_string(),
            source: Some(e),
        })?;
    }
    let mut buf: Vec<u8> = Vec::new();
    write_completions(shell, &mut buf);
    std::fs::write(&path, &buf).map_err(|e| SbError::Filesystem {
        message: "failed to write completion script".to_string(),
        path: path.display().to_string(),
        source: Some(e),
    })?;

    crate::output::print_success(
        &format!("installed {shell} completions to {}", path.display()),
        color,
        quiet,
    );
    if let Some(note) = note {
        if !quiet {
            eprintln!("{note}");
        }
    }
    Ok(())
}

/// Compute the install path and any follow-up instruction for a shell.
///
/// Pure over the two base directories so it is unit-testable without touching
/// the real filesystem or environment.
fn completion_path(
    shell: Shell,
    data_home: &Path,
    config_home: &Path,
) -> SbResult<(PathBuf, Option<String>)> {
    match shell {
        Shell::Bash => Ok((
            data_home.join("bash-completion/completions/sb"),
            Some(
                "Ensure bash-completion is installed and start a new shell to load completions."
                    .to_string(),
            ),
        )),
        Shell::Zsh => {
            let dir = data_home.join("zsh/site-functions");
            let note = format!(
                "Add this directory to your fpath (in ~/.zshrc, before `compinit`):\n  \
                 fpath+=({})\n  autoload -U compinit && compinit",
                dir.display()
            );
            Ok((dir.join("_sb"), Some(note)))
        }
        Shell::Fish => Ok((
            config_home.join("fish/completions/sb.fish"),
            None, // fish auto-loads this location
        )),
        other => Err(SbError::Usage(format!(
            "--install is not supported for {other}; redirect `sb completions {other}` output manually"
        ))),
    }
}

/// Detect the shell from `$SHELL`.
fn detect_shell() -> SbResult<Shell> {
    let sh = std::env::var("SHELL").unwrap_or_default();
    let base = sh.rsplit('/').next().unwrap_or("");
    shell_from_name(base).ok_or_else(|| {
        SbError::Usage(format!(
            "could not detect shell from $SHELL ({sh:?}); pass one explicitly: \
             sb completions <bash|zsh|fish>"
        ))
    })
}

/// Map a shell binary name to a `Shell`.
fn shell_from_name(name: &str) -> Option<Shell> {
    match name {
        "bash" => Some(Shell::Bash),
        "zsh" => Some(Shell::Zsh),
        "fish" => Some(Shell::Fish),
        "elvish" => Some(Shell::Elvish),
        "powershell" | "pwsh" => Some(Shell::PowerShell),
        _ => None,
    }
}

fn data_home() -> SbResult<PathBuf> {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x));
        }
    }
    Ok(home_dir()?.join(".local/share"))
}

fn config_home() -> SbResult<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x));
        }
    }
    Ok(home_dir()?.join(".config"))
}

fn home_dir() -> SbResult<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| SbError::Config {
            message: "HOME environment variable is not set".into(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_from_name_maps_known_shells() {
        assert_eq!(shell_from_name("bash"), Some(Shell::Bash));
        assert_eq!(shell_from_name("zsh"), Some(Shell::Zsh));
        assert_eq!(shell_from_name("fish"), Some(Shell::Fish));
        assert_eq!(shell_from_name("elvish"), Some(Shell::Elvish));
        assert_eq!(shell_from_name("unknown"), None);
        assert_eq!(shell_from_name(""), None);
    }

    #[test]
    fn completion_path_bash() {
        let data = PathBuf::from("/data");
        let config = PathBuf::from("/config");
        let (path, note) = completion_path(Shell::Bash, &data, &config).unwrap();
        assert_eq!(path, PathBuf::from("/data/bash-completion/completions/sb"));
        assert!(note.is_some());
    }

    #[test]
    fn completion_path_zsh_uses_underscore_sb_and_notes_fpath() {
        let data = PathBuf::from("/data");
        let config = PathBuf::from("/config");
        let (path, note) = completion_path(Shell::Zsh, &data, &config).unwrap();
        assert_eq!(path, PathBuf::from("/data/zsh/site-functions/_sb"));
        assert!(note.unwrap().contains("fpath+="));
    }

    #[test]
    fn completion_path_fish_needs_no_note() {
        let data = PathBuf::from("/data");
        let config = PathBuf::from("/config");
        let (path, note) = completion_path(Shell::Fish, &data, &config).unwrap();
        assert_eq!(path, PathBuf::from("/config/fish/completions/sb.fish"));
        assert!(note.is_none());
    }

    #[test]
    fn completion_path_unsupported_shell_errors() {
        let data = PathBuf::from("/data");
        let config = PathBuf::from("/config");
        assert!(completion_path(Shell::PowerShell, &data, &config).is_err());
    }

    #[test]
    fn write_completions_produces_script_for_each_shell() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut buf: Vec<u8> = Vec::new();
            write_completions(shell, &mut buf);
            assert!(!buf.is_empty(), "expected non-empty script for {shell}");
            let text = String::from_utf8_lossy(&buf);
            assert!(text.contains("sb"), "script should mention the binary name");
        }
    }
}
