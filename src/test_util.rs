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

/// RAII guard that:
/// - sets `SB_SPACE` to the tempdir for the duration of the test
/// - points `XDG_CONFIG_HOME` at a guaranteed-empty temp dir so the developer's
///   real `~/.config/sb/config.toml` doesn't leak into ResolvedConfig
/// - holds the cross-test mutex
///
/// Restores both env vars on drop. Tests that want to exercise XDG content
/// should pair this with `XdgGuard` (which is applied AFTER this one, so it wins).
pub struct SbSpaceGuard {
    _lock: MutexGuard<'static, ()>,
    _xdg_tmp: tempfile::TempDir,
    prev_sb_space: Option<String>,
    prev_xdg: Option<String>,
}

impl SbSpaceGuard {
    pub fn set(path: &std::path::Path) -> Self {
        let lock = match SB_SPACE_MUTEX.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let prev_sb_space = std::env::var("SB_SPACE").ok();
        std::env::set_var("SB_SPACE", path);
        // Isolate XDG so ResolvedConfig::load_from doesn't read the dev's real config.
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let xdg_tmp = tempfile::tempdir().expect("create xdg isolate dir");
        std::env::set_var("XDG_CONFIG_HOME", xdg_tmp.path());
        Self {
            _lock: lock,
            _xdg_tmp: xdg_tmp,
            prev_sb_space,
            prev_xdg,
        }
    }
}

impl Drop for SbSpaceGuard {
    fn drop(&mut self) {
        match &self.prev_sb_space {
            Some(v) => std::env::set_var("SB_SPACE", v),
            None => std::env::remove_var("SB_SPACE"),
        }
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

/// Override XDG_CONFIG_HOME to a tempdir-local path so user-config writes don't
/// touch the real ~/.config/sb/. Restores on drop. Does NOT take the mutex
/// (compose with other guards as needed).
pub struct XdgGuard {
    prev: Option<String>,
}

impl XdgGuard {
    pub fn set(path: &std::path::Path) -> Self {
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", path);
        Self { prev }
    }
}

impl Drop for XdgGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
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
