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

/// True when `name` is exactly `{prefix}<timestamp>{suffix}`, where `<timestamp>`
/// is the `YYYYmmddTHHMMSS` stamp `conflict_stash_path` writes.
///
/// Matching on the prefix alone is not enough. `A.md` has stem `A` and therefore
/// prefix `A.`, which also matches `A.B.20260901T130000.md` -- the stash
/// belonging to the *different* page `A.B.md`. Resolving `A.md` would then
/// overwrite it with A.B's remote content and delete the only copy of that
/// content. Requiring a real timestamp in the middle keeps the two apart, and
/// also excludes the un-stamped original filename.
fn is_stash_for(name: &str, prefix: &str, suffix: Option<&str>) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let middle = match suffix {
        Some(s) => match rest.strip_suffix(s) {
            Some(m) => m,
            None => return false,
        },
        None => rest,
    };
    is_stash_timestamp(middle)
}

/// True for the `YYYYmmddTHHMMSS` stamp `conflict_stash_path` writes.
fn is_stash_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 15
        && b[8] == b'T'
        && b[..8].iter().all(|c| c.is_ascii_digit())
        && b[9..].iter().all(|c| c.is_ascii_digit())
}

/// Recover the space-relative path a stash belongs to, inverting
/// `conflict_stash_path`. `rel_stash` is relative to `.sb/conflicts/` with
/// forward slashes; `None` means the name is not a stash.
///
/// `conflict_stash_path` inserts the stamp immediately before the extension, so
/// only the last two dot-separated components can hold it. Searching the whole
/// name would misread a page whose own title contains a timestamp. It also always
/// writes a stem in front of the stamp, so component 0 is never the stamp --
/// without that guard `20260101T000000.md` parses as a stash of a page called
/// `md`.
pub fn stash_origin(rel_stash: &str) -> Option<String> {
    let (dir, name) = match rel_stash.rsplit_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, rel_stash),
    };
    let mut parts: Vec<&str> = name.split('.').collect();
    let ts_idx = [parts.len().checked_sub(2), parts.len().checked_sub(1)]
        .into_iter()
        .flatten()
        .find(|&i| i > 0 && is_stash_timestamp(parts[i]))?;
    parts.remove(ts_idx);
    let origin = parts.join(".");
    if origin.is_empty() || origin == "." {
        return None;
    }
    Some(match dir {
        Some(d) => format!("{d}/{origin}"),
        None => origin,
    })
}

/// The directory `conflict_stash_path` puts stashes for `file_path` into.
pub fn stash_dir_for(sb_dir: &Path, file_path: &str) -> PathBuf {
    match Path::new(file_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
    {
        Some(parent) if !parent.is_empty() => sb_dir.join("conflicts").join(parent),
        _ => sb_dir.join("conflicts"),
    }
}

/// Every conflict stash currently on disk for `file_path`, oldest mtime first.
///
/// Empty when nothing has ever conflicted on this path -- a missing
/// `.sb/conflicts/` is "none", not an error. One listing function serves every
/// consumer: the stash writer (so a byte-identical copy is never appended),
/// `sb sync resolve` (so it consumes the whole set instead of leaving the older
/// ones behind), `sb sync prune-stashes`, and `find_stash_file`.
pub fn stash_files_for(sb_dir: &Path, file_path: &str) -> crate::error::SbResult<Vec<PathBuf>> {
    let p = Path::new(file_path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = p.extension().and_then(|s| s.to_str());
    let dir = stash_dir_for(sb_dir, file_path);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let prefix = format!("{stem}.");
    let suffix = ext.map(|e| format!(".{e}"));

    let mut matches: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| crate::error::SbError::Filesystem {
            message: "failed to read conflicts directory".to_string(),
            path: dir.display().to_string(),
            source: Some(e),
        })?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            is_stash_for(name, &prefix, suffix.as_deref())
        })
        .collect();

    matches.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    Ok(matches)
}

/// Stash the server's copy of `file_path`, reusing an existing stash that
/// already holds exactly these bytes.
///
/// A conflict row keeps conflicting on every sync until someone resolves it, so
/// appending unconditionally grows the set by one file per sync run: the incident
/// behind this check left 86 byte-identical copies of one page over 27 hours.
/// Returns whichever stash now holds `content`.
pub async fn write_conflict_stash(
    sb_dir: &Path,
    file_path: &str,
    content: &[u8],
) -> crate::error::SbResult<PathBuf> {
    let want = blake3::hash(content);
    for existing in stash_files_for(sb_dir, file_path)? {
        // An unreadable stash is not a match -- fall through and write a fresh one.
        if tokio::fs::read(&existing)
            .await
            .is_ok_and(|bytes| blake3::hash(&bytes) == want)
        {
            return Ok(existing);
        }
    }

    let stash_path = conflict_stash_path(sb_dir, file_path);
    if let Some(parent) = stash_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| crate::error::SbError::Filesystem {
                message: "failed to create conflict stash directory".into(),
                path: parent.display().to_string(),
                source: Some(e),
            })?;
    }
    tokio::fs::write(&stash_path, content).await.map_err(|e| {
        crate::error::SbError::Filesystem {
            message: "failed to write conflict stash file".into(),
            path: stash_path.display().to_string(),
            source: Some(e),
        }
    })?;
    Ok(stash_path)
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
    /// The server refused to accept this path (HTTP 403 — read-only path).
    /// Push skips the row until its `local_hash` changes, so editing the file
    /// again is what retries the upload. Pull never deletes such a file, and a
    /// changed server copy pulls normally (local reverted) or conflicts (local
    /// still holds the refused edit) — the row is never a dead end.
    ReadOnly,
}

impl SyncStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncStatus::Synced => "synced",
            SyncStatus::Modified => "modified",
            SyncStatus::New => "new",
            SyncStatus::Deleted => "deleted",
            SyncStatus::Conflict => "conflict",
            SyncStatus::ReadOnly => "readonly",
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
            "readonly" => Ok(SyncStatus::ReadOnly),
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
    /// blake3 hex digest of the content we believe the server holds. NEVER an ETag.
    pub remote_hash: Option<String>,
    /// The server's own `ETag` for that content, verbatim, or `None` when the
    /// server does not do conditional writes. SHA-256-based and opaque: never
    /// assigned from or compared against `remote_hash`.
    pub remote_etag: Option<String>,
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
        /// blake3, same algorithm as `local_hash`.
        remote_hash: String,
        /// The server's ETag for the bytes now on the server, if it sent one.
        remote_etag: Option<String>,
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
    /// The server refused this path as read-only (403). `local_hash` is the hash
    /// of the content it refused, so the next push can tell "still the rejected
    /// bytes, skip it" from "edited since, worth another try". Not optional: a
    /// row with no refused bytes has nothing to compare against and nothing that
    /// would ever clear it, which is how a refused DELETE used to strand.
    ReadOnly {
        path: String,
        local_hash: String,
    },
}

#[cfg(test)]
mod stash_tests {
    use super::*;

    #[test]
    fn stash_origin_inverts_conflict_stash_path() {
        let sb = Path::new("/space/.sb");
        for original in [
            "Doc.md",
            "Work/Career/Okta Auth0 SRE Manager.md",
            "A.B.md",
            "Makefile",
        ] {
            let stash = conflict_stash_path(sb, original);
            let rel = stash
                .strip_prefix(sb.join("conflicts"))
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert_eq!(stash_origin(&rel).as_deref(), Some(original), "{original}");
        }
    }

    #[test]
    fn stash_origin_rejects_names_without_a_stamp() {
        assert_eq!(stash_origin("Doc.md"), None);
        assert_eq!(stash_origin("Work/Doc.md"), None);
        // A page whose own title ends in a stamp-shaped word is not a stash.
        assert_eq!(stash_origin("20260101T000000.md"), None);
    }

    #[tokio::test]
    async fn write_conflict_stash_reuses_a_stash_with_the_same_bytes() {
        // The accumulation bug: an unresolved row re-conflicts every sync, so
        // without this the set grows by one identical file per run.
        let tmp = tempfile::tempdir().unwrap();
        let sb = tmp.path().join(".sb");
        let first = write_conflict_stash(&sb, "Work/Okta.md", b"same bytes")
            .await
            .unwrap();
        for _ in 0..5 {
            let again = write_conflict_stash(&sb, "Work/Okta.md", b"same bytes")
                .await
                .unwrap();
            assert_eq!(again, first);
        }
        assert_eq!(stash_files_for(&sb, "Work/Okta.md").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn write_conflict_stash_still_records_different_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = tmp.path().join(".sb");
        let existing = sb.join("conflicts").join("Doc.20260101T000000.md");
        std::fs::create_dir_all(existing.parent().unwrap()).unwrap();
        std::fs::write(&existing, b"old remote").unwrap();

        let fresh = write_conflict_stash(&sb, "Doc.md", b"new remote")
            .await
            .unwrap();
        assert_ne!(fresh, existing, "different content needs its own stash");
        assert_eq!(std::fs::read(&fresh).unwrap(), b"new remote");
        assert_eq!(stash_files_for(&sb, "Doc.md").unwrap().len(), 2);
    }

    #[test]
    fn stash_files_for_is_empty_when_nothing_ever_conflicted() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(stash_files_for(tmp.path(), "Doc.md").unwrap().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sync_status_round_trips_through_its_string_form() {
        // A status that does not survive as_str -> from_str is a row that either
        // fails to load from state.db or loads as the wrong thing.
        for status in [
            SyncStatus::Synced,
            SyncStatus::Modified,
            SyncStatus::New,
            SyncStatus::Deleted,
            SyncStatus::Conflict,
            SyncStatus::ReadOnly,
        ] {
            let parsed: SyncStatus = status
                .as_str()
                .parse()
                .unwrap_or_else(|_| panic!("'{}' should parse back", status.as_str()));
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn readonly_status_is_stored_as_readonly() {
        assert_eq!(SyncStatus::ReadOnly.as_str(), "readonly");
        assert_eq!(SyncStatus::ReadOnly.to_string(), "readonly");
    }

    #[test]
    fn unknown_status_string_is_rejected() {
        let err = "read-only".parse::<SyncStatus>().unwrap_err();
        assert!(err.to_string().contains("unknown sync status"));
    }
}
