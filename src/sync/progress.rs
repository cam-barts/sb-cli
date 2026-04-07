use indicatif::{ProgressBar, ProgressStyle};

/// TTY-aware progress wrapper for sync operations.
///
/// When `show=true` AND stdout is a TTY, displays a progress bar.
/// Otherwise, all operations are no-ops. This ensures clean output
/// when piped or when `--quiet` is requested.
///
/// `ProgressBar` is `Send + Sync` — safe to clone and share across tasks.
pub struct SyncProgress {
    bar: Option<ProgressBar>,
}

impl SyncProgress {
    /// Create a new progress indicator.
    ///
    /// - `total`: total number of files to process (used for bar length)
    /// - `show`: pass `false` to suppress (e.g., `--quiet` flag)
    ///
    /// If stdout is not a TTY (piped/redirected), progress is suppressed
    /// regardless of `show`.
    pub fn new(total: u64, show: bool) -> Self {
        if !show || !crate::output::is_tty() {
            return SyncProgress { bar: None };
        }
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg} [{bar:40}] {pos}/{len}")
                .expect("hardcoded progress bar template is valid")
                .progress_chars("=> "),
        );
        pb.set_message("Syncing");
        SyncProgress { bar: Some(pb) }
    }

    /// Increment progress counter by 1.
    pub fn inc(&self) {
        if let Some(ref pb) = self.bar {
            pb.inc(1);
        }
    }

    /// Set the progress message (e.g., "Pulling", "Pushing").
    pub fn set_message(&self, msg: &str) {
        if let Some(ref pb) = self.bar {
            pb.set_message(msg.to_string());
        }
    }

    /// Mark progress as finished and clear the bar from the terminal.
    pub fn finish(&self) {
        if let Some(ref pb) = self.bar {
            pb.finish_and_clear();
        }
    }

    /// Get a clone of the inner `ProgressBar` for sharing across async tasks.
    ///
    /// Returns `None` if progress is suppressed.
    pub fn bar(&self) -> Option<ProgressBar> {
        self.bar.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // In cargo test environments, stdout is not a TTY, so is_tty() returns false.
    // This means SyncProgress::new(..., show=true) will still produce a no-op bar.
    // We test the no-op path explicitly (show=false) and the suppressed-TTY path (show=true in test).

    #[test]
    fn sync_progress_new_with_show_false_returns_noop() {
        // show=false should always produce a no-op (bar=None)
        let progress = SyncProgress::new(100, false);
        assert!(
            progress.bar.is_none(),
            "show=false should produce no-op progress"
        );
    }

    #[test]
    fn sync_progress_new_with_show_true_noop_in_non_tty() {
        // In test environments, stdout is not a TTY, so bar should be None
        let progress = SyncProgress::new(100, true);
        // Either None (test env, not a TTY) or Some (if somehow running in a TTY)
        // Either way, we just verify it doesn't panic
        let _ = progress.bar();
    }

    #[test]
    fn sync_progress_inc_does_not_panic_when_noop() {
        let progress = SyncProgress::new(100, false);
        // Should be no-op without panic
        progress.inc();
        progress.inc();
        progress.inc();
    }

    #[test]
    fn sync_progress_finish_does_not_panic_when_noop() {
        let progress = SyncProgress::new(100, false);
        progress.finish();
    }

    #[test]
    fn sync_progress_set_message_does_not_panic_when_noop() {
        let progress = SyncProgress::new(100, false);
        progress.set_message("Pulling");
        progress.set_message("Pushing");
    }

    #[test]
    fn sync_progress_bar_returns_none_when_noop() {
        let progress = SyncProgress::new(0, false);
        assert!(
            progress.bar().is_none(),
            "bar() should return None when no-op"
        );
    }

    #[test]
    fn sync_progress_all_methods_chain_without_panic() {
        // Full lifecycle: create -> set_message -> inc * N -> finish
        let progress = SyncProgress::new(5, false);
        progress.set_message("Syncing files");
        for _ in 0..5 {
            progress.inc();
        }
        progress.finish();
    }

    #[test]
    fn sync_progress_zero_total_does_not_panic() {
        let progress = SyncProgress::new(0, false);
        progress.inc(); // increment past zero total should not panic
        progress.finish();
    }
}
