pub mod db;
pub mod progress;
pub mod puller;
pub mod pusher;
pub mod scanner;

use std::path::{Path, PathBuf};

/// Actions the sync engine can take on a single file.
///
/// All variants include a `reason` field for human/JSON dry-run output.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum SyncAction {
    Download {
        path: String,
        remote_mtime: i64,
        reason: String,
    },
    Upload {
        path: String,
        reason: String,
    },
    DeleteLocal {
        path: String,
        reason: String,
    },
    DeleteRemote {
        path: String,
        reason: String,
    },
    Conflict {
        path: String,
        reason: String,
    },
    Skip {
        path: String,
        reason: String,
    },
}

/// Compute the conflict stash path for a file.
///
/// Example: "Journal/2026-04-05.md" -> ".sb/conflicts/Journal/2026-04-05.20260405T143022.md"
pub fn conflict_stash_path(sb_dir: &Path, file_path: &str) -> PathBuf {
    let timestamp = jiff::Zoned::now().strftime("%Y%m%dT%H%M%S").to_string();
    let p = Path::new(file_path);
    let stem = p.with_extension("").to_string_lossy().into_owned();
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    let sep = if ext.is_empty() { "" } else { "." };
    sb_dir
        .join("conflicts")
        .join(format!("{stem}.{timestamp}{sep}{ext}"))
}

/// Sync status for a tracked file in state.db.
///
/// Represents the relationship between local and remote state.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Synced,
    Modified,
    New,
    Deleted,
    Conflict,
}

impl SyncStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncStatus::Synced => "synced",
            SyncStatus::Modified => "modified",
            SyncStatus::New => "new",
            SyncStatus::Deleted => "deleted",
            SyncStatus::Conflict => "conflict",
        }
    }
}

impl std::fmt::Display for SyncStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SyncStatus {
    type Err = crate::error::SbError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "synced" => Ok(SyncStatus::Synced),
            "modified" => Ok(SyncStatus::Modified),
            "new" => Ok(SyncStatus::New),
            "deleted" => Ok(SyncStatus::Deleted),
            "conflict" => Ok(SyncStatus::Conflict),
            other => Err(crate::error::SbError::Database {
                message: format!("unknown sync status in state.db: '{other}'"),
                source: None,
            }),
        }
    }
}

/// A row from the sync_state table.
///
/// Timestamps are Unix milliseconds — never convert to seconds.
#[derive(Debug, Clone)]
pub struct SyncStateRow {
    pub path: String,
    pub local_hash: Option<String>,
    pub remote_hash: Option<String>,
    pub remote_mtime: i64, // Unix ms — never convert to seconds
    pub local_mtime: i64,  // Unix ms
    pub status: SyncStatus,
    pub conflict_at: i64, // Unix ms timestamp when conflict was detected; 0 if not a conflict
}

/// Result of a single file sync operation, used for batch commit.
///
/// Passed to `StateDb::commit_batch()` to atomically record sync outcomes.
#[non_exhaustive]
#[derive(Debug)]
pub enum SyncResult {
    Synced {
        path: String,
        local_hash: String,
        remote_hash: String,
        remote_mtime: i64,
        local_mtime: i64,
    },
    Conflict {
        path: String,
        conflict_at: i64, // Unix ms timestamp at detection time
    },
    Deleted {
        path: String,
    },
}
