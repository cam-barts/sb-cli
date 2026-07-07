//! Shared fzf-style item picker.
//!
//! Presents a list of items and returns the chosen one. Uses `fzf` when it's on
//! `PATH` and stdin is a terminal, otherwise falls back to a numbered prompt.
//! Callers (template selection, `page edit`/`read`/`delete` without an explicit
//! name, …) supply the item list plus a short noun for messages.

use std::io::IsTerminal;

use crate::error::{SbError, SbResult};

/// Present `items` and return the chosen one, or `None` if the user cancels.
///
/// `noun` is the singular thing being chosen (e.g. `"page"`, `"template"`) and
/// is used both as the fzf prompt (`"<noun>> "`) and in messages. Errors when
/// there is nothing to choose from or when stdin is not a terminal (nothing to
/// drive an interactive picker).
pub async fn pick(items: &[String], noun: &str) -> SbResult<Option<String>> {
    if items.is_empty() {
        return Err(SbError::Usage(format!(
            "no {noun}s available to choose from"
        )));
    }
    if !std::io::stdin().is_terminal() {
        return Err(SbError::Usage(format!(
            "cannot pick a {noun} in non-interactive mode; pass one explicitly"
        )));
    }
    match pick_with_fzf(items, noun).await? {
        FzfOutcome::Selected(s) => Ok(Some(s)),
        FzfOutcome::Cancelled => Ok(None),
        FzfOutcome::Unavailable => pick_with_prompt(items, noun).await,
    }
}

enum FzfOutcome {
    Selected(String),
    Cancelled,
    Unavailable,
}

async fn pick_with_fzf(items: &[String], noun: &str) -> SbResult<FzfOutcome> {
    let input = items.join("\n");
    let prompt = format!("--prompt={noun}> ");
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = match Command::new("fzf")
            .arg(prompt)
            .arg("--height=40%")
            .arg("--reverse")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FzfOutcome::Unavailable)
            }
            Err(e) => {
                return Err(SbError::Filesystem {
                    message: "failed to launch fzf".to_string(),
                    path: "fzf".to_string(),
                    source: Some(e),
                })
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        } // stdin dropped here → EOF for fzf
        let output = child.wait_with_output().map_err(|e| SbError::Filesystem {
            message: "fzf did not complete".to_string(),
            path: "fzf".to_string(),
            source: Some(e),
        })?;
        // fzf exits 0 on selection, 1 on no-match, 130 on Ctrl-C/Esc.
        if output.status.success() {
            let sel = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if sel.is_empty() {
                Ok(FzfOutcome::Cancelled)
            } else {
                Ok(FzfOutcome::Selected(sel))
            }
        } else {
            Ok(FzfOutcome::Cancelled)
        }
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("selection task failed: {e}"),
    })?
}

async fn pick_with_prompt(items: &[String], noun: &str) -> SbResult<Option<String>> {
    let items = items.to_vec();
    let noun = noun.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        eprintln!("Select a {noun}:");
        for (i, n) in items.iter().enumerate() {
            eprintln!("  {}) {}", i + 1, n);
        }
        eprint!("Choice [1-{}] (empty to cancel): ", items.len());
        std::io::stderr().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        match trimmed.parse::<usize>() {
            Ok(n) if n >= 1 && n <= items.len() => Ok(Some(items[n - 1].clone())),
            _ => Err(SbError::Usage(format!("invalid selection: {trimmed}"))),
        }
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("selection task failed: {e}"),
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pick_errors_on_empty_items() {
        let err = pick(&[], "page").await.unwrap_err();
        match err {
            SbError::Usage(m) => assert!(m.contains("no pages available"), "got: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pick_errors_in_non_interactive_mode() {
        // The test harness runs without a TTY on stdin.
        let items = vec!["a".to_string(), "b".to_string()];
        let err = pick(&items, "page").await.unwrap_err();
        match err {
            SbError::Usage(m) => assert!(m.contains("non-interactive"), "got: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }
}
