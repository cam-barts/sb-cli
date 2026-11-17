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
