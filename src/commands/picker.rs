//! Shared fzf-style item picker.
//!
//! Presents a list of items and returns the chosen one(s). Uses `fzf` when it's
//! on `PATH` and stdin is a terminal, otherwise falls back to a numbered prompt.
//! Callers (template selection, `page edit`/`read`/`delete` without an explicit
//! name, `sync resolve` without a path, …) supply the item list plus a short
//! noun for messages.
//!
//! `fzf` is never a build dependency — it is discovered on `PATH` at runtime and
//! the numbered fallback is a first-class path, multi-select included.

use crate::error::{SbError, SbResult};

/// Present `items` and return the chosen one, or `None` if the user cancels.
///
/// `noun` is the singular thing being chosen (e.g. `"page"`, `"template"`) and
/// is used both as the fzf prompt (`"<noun>> "`) and in messages. Errors when
/// there is nothing to choose from or when stdin is not a terminal (nothing to
/// drive an interactive picker).
pub async fn pick(items: &[String], noun: &str) -> SbResult<Option<String>> {
    Ok(pick_inner(items, noun, false).await?.into_iter().next())
}

/// Like [`pick`], but lets the user choose several items at once (Tab in fzf,
/// `1,3-5` or `a` in the numbered fallback). An empty Vec means "cancelled".
pub async fn pick_multi(items: &[String], noun: &str) -> SbResult<Vec<String>> {
    pick_inner(items, noun, true).await
}

async fn pick_inner(items: &[String], noun: &str, multi: bool) -> SbResult<Vec<String>> {
    if items.is_empty() {
        return Err(SbError::Usage(format!(
            "no {noun}s available to choose from"
        )));
    }
    if crate::output::no_input() {
        return Err(SbError::Usage(format!(
            "cannot pick a {noun} in non-interactive mode; pass one explicitly"
        )));
    }
    match pick_with_fzf(items, noun, multi).await? {
        FzfOutcome::Selected(v) => Ok(v),
        FzfOutcome::Cancelled => Ok(Vec::new()),
        FzfOutcome::Unavailable => pick_with_prompt(items, noun, multi).await,
    }
}

enum FzfOutcome {
    Selected(Vec<String>),
    Cancelled,
    Unavailable,
}

async fn pick_with_fzf(items: &[String], noun: &str, multi: bool) -> SbResult<FzfOutcome> {
    let input = items.join("\n");
    let items = items.to_vec();
    let prompt = format!("--prompt={noun}> ");
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new("fzf");
        cmd.arg(prompt).arg("--height=40%").arg("--reverse");
        if multi {
            // Tab toggles, Enter confirms. fzf prints one selection per line.
            cmd.arg("--multi");
        }
        let mut child = match cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() {
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
        match output.status.code() {
            // 0 = something was selected. Split on lines only, since item text
            // may contain spaces, and keep only lines that are genuinely items:
            // FZF_DEFAULT_OPTS is user-controlled and options like
            // --print-query or --header-lines inject extra lines that would
            // otherwise become bogus targets.
            Some(0) => {
                let sel: Vec<String> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|l| l.trim_end_matches('\r').to_string())
                    .filter(|l| items.contains(l))
                    .collect();
                if sel.is_empty() {
                    Ok(FzfOutcome::Cancelled)
                } else {
                    Ok(FzfOutcome::Selected(sel))
                }
            }
            // 1 = no match, 130 = Ctrl-C/Esc. Both mean "the user picked
            // nothing", which is a cancel, not a failure.
            Some(1) | Some(130) => Ok(FzfOutcome::Cancelled),
            // Anything else (2 = fzf error, 127 = not executable, a signal)
            // is a real failure. Reporting it as a cancel would make
            // `sb sync resolve` claim success while doing nothing.
            other => Err(SbError::Filesystem {
                message: match other {
                    Some(c) => format!("fzf exited with status {c}"),
                    None => "fzf was killed by a signal".to_string(),
                },
                path: "fzf".to_string(),
                source: None,
            }),
        }
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("selection task failed: {e}"),
    })?
}

async fn pick_with_prompt(items: &[String], noun: &str, multi: bool) -> SbResult<Vec<String>> {
    let items = items.to_vec();
    let noun = noun.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        if multi {
            eprintln!("Select one or more {noun}s:");
        } else {
            eprintln!("Select a {noun}:");
        }
        for (i, n) in items.iter().enumerate() {
            eprintln!("  {}) {}", i + 1, n);
        }
        if multi {
            eprint!(
                "Choice [1-{}, e.g. 1,3-5 or \"a\" for all] (empty to cancel): ",
                items.len()
            );
        } else {
            eprint!("Choice [1-{}] (empty to cancel): ", items.len());
        }
        std::io::stderr().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let idxs = parse_selection(&input, items.len(), multi).map_err(SbError::Usage)?;
        Ok(idxs.into_iter().map(|i| items[i].clone()).collect())
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("selection task failed: {e}"),
    })?
}

/// Turn a numbered-prompt reply into 0-based indices into a `len`-item list.
///
/// Empty input means "cancel" and yields an empty Vec. When `multi`, accepts
/// `a`/`all`, comma-separated indices, and inclusive `n-m` ranges; the result is
/// deduped and returned in list order, not input order. When not `multi`, only a
/// single index is accepted, preserving the original single-select behaviour.
fn parse_selection(input: &str, len: usize, multi: bool) -> Result<Vec<usize>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if !multi {
        return match trimmed.parse::<usize>() {
            Ok(n) if n >= 1 && n <= len => Ok(vec![n - 1]),
            _ => Err(format!("invalid selection: {trimmed}")),
        };
    }
    if trimmed.eq_ignore_ascii_case("a") || trimmed.eq_ignore_ascii_case("all") {
        return Ok((0..len).collect());
    }
    let mut hit = vec![false; len];
    // Accept spaces as separators as well as commas: "1 2" is at least as
    // natural to type as "1,2".
    for part in trimmed.split([',', ' ']).filter(|p| !p.trim().is_empty()) {
        let part = part.trim();
        let (lo, hi) = match part.split_once('-') {
            Some((a, b)) => (parse_index(a, len, trimmed)?, parse_index(b, len, trimmed)?),
            None => {
                let n = parse_index(part, len, trimmed)?;
                (n, n)
            }
        };
        if lo > hi {
            return Err(format!("invalid selection: {trimmed}"));
        }
        for slot in hit.iter_mut().take(hi + 1).skip(lo) {
            *slot = true;
        }
    }
    Ok((0..len).filter(|i| hit[*i]).collect())
}

/// Parse one 1-based index token into a 0-based index, bounded by `len`.
fn parse_index(token: &str, len: usize, whole: &str) -> Result<usize, String> {
    match token.trim().parse::<usize>() {
        Ok(n) if n >= 1 && n <= len => Ok(n - 1),
        _ => Err(format!("invalid selection: {whole}")),
    }
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

    #[tokio::test]
    async fn pick_multi_errors_on_empty_items() {
        let err = pick_multi(&[], "conflict").await.unwrap_err();
        match err {
            SbError::Usage(m) => assert!(m.contains("no conflicts available"), "got: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pick_multi_errors_in_non_interactive_mode() {
        let items = vec!["a".to_string(), "b".to_string()];
        let err = pick_multi(&items, "conflict").await.unwrap_err();
        match err {
            SbError::Usage(m) => assert!(m.contains("non-interactive"), "got: {m}"),
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_selection_handles_ranges_and_lists() {
        assert_eq!(parse_selection("1,3-5", 5, true).unwrap(), vec![0, 2, 3, 4]);
        assert_eq!(parse_selection("2", 5, true).unwrap(), vec![1]);
        assert_eq!(parse_selection(" 1 , 2 ", 5, true).unwrap(), vec![0, 1]);
        assert_eq!(parse_selection("3-3", 5, true).unwrap(), vec![2]);
    }

    #[test]
    fn parse_selection_selects_all() {
        assert_eq!(parse_selection("a", 3, true).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_selection("ALL", 3, true).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn parse_selection_dedupes_and_orders_by_list() {
        // Repeated index collapses; input order does not leak into the result.
        assert_eq!(parse_selection("2,2", 5, true).unwrap(), vec![1]);
        assert_eq!(parse_selection("3,1", 5, true).unwrap(), vec![0, 2]);
        assert_eq!(parse_selection("2-4,3", 5, true).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn parse_selection_empty_is_cancel() {
        assert!(parse_selection("", 5, true).unwrap().is_empty());
        assert!(parse_selection("  \n", 5, true).unwrap().is_empty());
        assert!(parse_selection("\n", 5, false).unwrap().is_empty());
    }

    #[test]
    fn parse_selection_rejects_out_of_range_and_garbage() {
        for bad in ["0", "6", "3-1", "x", "-1", "1-", "1--2", "5-9", "1,x"] {
            assert!(
                parse_selection(bad, 5, true).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn parse_selection_tolerates_spaces_and_stray_separators() {
        // Typing "1 2" instead of "1,2" should not be punished, and neither
        // should a trailing or doubled separator.
        assert_eq!(parse_selection("1 2", 5, true).unwrap(), vec![0, 1]);
        assert_eq!(parse_selection("1, 3-4", 5, true).unwrap(), vec![0, 2, 3]);
        assert_eq!(parse_selection("1,", 5, true).unwrap(), vec![0]);
        assert_eq!(parse_selection("1,,2", 5, true).unwrap(), vec![0, 1]);
    }

    #[test]
    fn parse_selection_single_mode_rejects_lists() {
        // Single-select keeps its original contract: one index, nothing fancy.
        assert_eq!(parse_selection("2", 5, false).unwrap(), vec![1]);
        assert!(parse_selection("1,2", 5, false).is_err());
        assert!(parse_selection("a", 5, false).is_err());
        assert!(parse_selection("0", 5, false).is_err());
        assert!(parse_selection("6", 5, false).is_err());
    }
}
