//! Shared helpers for integration-style tests across command modules.
//!
//! Why: `find_space_root()` reads `SB_SPACE` (and otherwise walks up from cwd),
//! and `cargo test` runs tests in parallel within a single binary. The mutex
//! guards both `SB_SPACE` and cwd so two tests don't trample each other.
//!
//! Note: `auth.rs` and `init.rs` use `std::env::current_dir()` directly rather
//! than `find_space_root()`, so tests for those commands must change cwd while
//! holding the same lock.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// Serializes SB_SPACE-modifying and cwd-modifying tests across the entire
/// test binary. Both axes share the lock because they jointly determine which
/// space a command resolves.
pub static SB_SPACE_MUTEX: Mutex<()> = Mutex::new(());

/// Strips every `SB_*` variable for its lifetime and puts the originals back on
/// drop (removing any a test set meanwhile). The developer's SB_SERVER_URL /
/// SB_TOKEN / SB_IDENTITY otherwise outrank a test's config and send real
/// requests to their server. Only sound while holding `SB_SPACE_MUTEX`; embed
/// it in a guard as a field declared BEFORE the lock, so it restores first.
pub struct SbEnvScrub {
    saved: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

fn sb_vars() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(k, _)| k.to_string_lossy().starts_with("SB_"))
        .collect()
}

impl SbEnvScrub {
    pub fn strip() -> Self {
        let saved = sb_vars();
        for (k, _) in &saved {
            std::env::remove_var(k);
        }
        Self { saved }
    }
}

impl Drop for SbEnvScrub {
    fn drop(&mut self) {
        for (k, _) in sb_vars() {
            std::env::remove_var(k);
        }
        for (k, v) in &self.saved {
            std::env::set_var(k, v);
        }
    }
}

/// RAII guard that:
/// - strips every `SB_*` variable, then sets `SB_SPACE` to the tempdir
/// - points `XDG_CONFIG_HOME` at a guaranteed-empty temp dir so the developer's
///   real `~/.config/sb/config.toml` doesn't leak into ResolvedConfig
/// - holds the cross-test mutex
///
/// Restores the environment on drop.
pub struct SbSpaceGuard {
    // Field order matters: fields drop in declaration order, so the scrub
    // restores the SB_* vars while the lock is still held.
    _scrub: SbEnvScrub,
    _lock: MutexGuard<'static, ()>,
    _xdg_tmp: tempfile::TempDir,
    prev_xdg: Option<String>,
}

impl SbSpaceGuard {
    pub fn set(path: &std::path::Path) -> Self {
        let lock = match SB_SPACE_MUTEX.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let scrub = SbEnvScrub::strip();
        std::env::set_var("SB_SPACE", path);
        // Isolate XDG so ResolvedConfig::load_from doesn't read the dev's real config.
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let xdg_tmp = tempfile::tempdir().expect("create xdg isolate dir");
        std::env::set_var("XDG_CONFIG_HOME", xdg_tmp.path());
        Self {
            _scrub: scrub,
            _lock: lock,
            _xdg_tmp: xdg_tmp,
            prev_xdg,
        }
    }
}

impl Drop for SbSpaceGuard {
    fn drop(&mut self) {
        match &self.prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}

/// RAII guard for the test cwd. Restores on drop.
pub struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    prev: PathBuf,
}

impl CwdGuard {
    pub fn set(path: &std::path::Path) -> Self {
        let lock = match SB_SPACE_MUTEX.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let prev = std::env::current_dir().expect("cwd readable");
        std::env::set_current_dir(path).expect("cd into tempdir");
        Self { _lock: lock, prev }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
    }
}

/// Create a `tempfile::TempDir` initialized as a SilverBullet space.
///
/// Writes `.sb/config.toml` with the given `server_url` (`None` => unset).
/// Returns the tempdir; the caller is responsible for grabbing `SbSpaceGuard`.
pub fn make_space(server_url: Option<&str>) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let sb_dir = tmp.path().join(".sb");
    std::fs::create_dir_all(&sb_dir).expect("create .sb");
    // sync.dir defaults to "space" — set "." so content lives at the space root
    // (the simplest shape for tests that seed pages directly at tmp.path()).
    let body = match server_url {
        Some(u) => format!(
            "server_url = \"{}\"\ntoken = \"test-token\"\n[sync]\ndir = \".\"\n",
            u
        ),
        None => "[sync]\ndir = \".\"\n".to_string(),
    };
    std::fs::write(sb_dir.join("config.toml"), body).expect("write config.toml");
    tmp
}
