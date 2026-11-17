//! Man page generation and installation.
//!
//! `sb man` renders the top-level `sb.1` roff page to stdout (read it with
//! `sb man | man -l -`). `sb man --install` writes one page per command —
//! `sb.1`, `sb-page.1`, `sb-page-read.1`, … — into the XDG man1 directory, so
//! `man sb` and `man sb-page-read` both answer.
//!
//! Generated from the same `clap` command graph as `--help` and
//! `sb completions`, so there is no second description to keep in sync.

use std::path::{Path, PathBuf};

use clap::CommandFactory;

use crate::cli::Cli;
use crate::error::{SbError, SbResult};

/// Entry point for `sb man`.
pub fn execute(install: bool, quiet: bool, color: bool) -> SbResult<()> {
    if install {
        install_pages(quiet, color)
    } else {
        render_root(&mut std::io::stdout())
    }
}

/// Render the top-level page only. The per-subcommand pages are an install-time
/// concern: on stdout there is nowhere for 70 files to go.
fn render_root(buf: &mut dyn std::io::Write) -> SbResult<()> {
    clap_mangen::Man::new(Cli::command())
        .render(buf)
        .map_err(|e| SbError::Filesystem {
            message: "failed to write man page".to_string(),
            path: "stdout".to_string(),
            source: Some(e),
        })
}

fn install_pages(quiet: bool, color: bool) -> SbResult<()> {
    let dir = man1_dir(&super::completions::data_home()?);

    std::fs::create_dir_all(&dir).map_err(|e| SbError::Filesystem {
        message: "failed to create man directory".to_string(),
        path: dir.display().to_string(),
        source: Some(e),
    })?;
    clap_mangen::generate_to(Cli::command(), &dir).map_err(|e| SbError::Filesystem {
        message: "failed to write man pages".to_string(),
        path: dir.display().to_string(),
        source: Some(e),
    })?;

    crate::output::print_success(
        &format!("installed man pages to {}", dir.display()),
        color,
        quiet,
    );
    if !quiet {
        eprintln!(
            "Run `man sb` to read them. If man cannot find them, add the parent to $MANPATH:\n  \
             export MANPATH=\"{}:$MANPATH\"",
            dir.parent().unwrap_or(&dir).display()
        );
    }
    Ok(())
}

/// The man1 directory under a data home. Pure, so it is testable without
/// touching the real filesystem or environment.
fn man1_dir(data_home: &Path) -> PathBuf {
    data_home.join("man/man1")
}
