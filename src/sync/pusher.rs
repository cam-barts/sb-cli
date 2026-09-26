use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::client::SbClient;
use crate::error::{SbError, SbResult};
use crate::sync::db::StateDb;
use crate::sync::progress::SyncProgress;
use crate::sync::scanner::{FileFilter, LocalFileInfo, LocalScanner};
use crate::sync::{SyncResult, SyncStateRow, SyncStatus};

/// Summary of a push operation.
#[derive(Debug, Default)]
pub struct PushResult {
    pub uploaded: usize,
    pub conflicts: usize,
    pub deleted: usize,
    pub skipped: usize,
    /// Paths the server refused as read-only (403). Recorded, not fatal.
    pub readonly: usize,
    pub results: Vec<SyncResult>,
    /// Per-file failures. Push keeps going after one, so the caller can commit
    /// `results` before reporting these — one bad file no longer discards a
    /// whole push's worth of successful uploads.
    pub failures: Vec<(String, SbError)>,
}

/// Push local changes to the server.
///
/// Scans local files, compares against state.db, verifies server state via
/// X-Get-Meta, and uploads changes while detecting conflicts. Local deletions
/// are handled by deleting from server after conflict check.
///
/// Uses Semaphore + JoinSet for bounded concurrent file uploads.
pub async fn push(
    client: &SbClient,
    space_root: &Path,
    sb_dir: &Path,
    db_path: &Path,
    filter: &FileFilter,
    workers: u32,
    show_progress: bool,
) -> SbResult<PushResult> {
    // 1. Scan local files via spawn_blocking, passing the real filter directly now that
    //    FileFilter: Clone.
    let space_root_owned = space_root.to_path_buf();
    let scanner = LocalScanner::new(filter.clone());
    let local_files: Vec<LocalFileInfo> =
        tokio::task::spawn_blocking(move || scanner.scan(&space_root_owned))
            .await
            .map_err(|e| SbError::Internal {
                message: format!("scan task panicked: {e}"),
            })??;

    // 2. Load all state.db rows via spawn_blocking
    let db_path_owned = db_path.to_path_buf();
    let rows = tokio::task::spawn_blocking(move || {
        let db = StateDb::open(&db_path_owned)?;
        db.get_all_rows()
    })
    .await
    .map_err(|e| SbError::Filesystem {
        message: format!("spawn_blocking panicked loading state: {e}"),
        path: db_path.display().to_string(),
        source: None,
    })??;

    // 3. Build HashMaps
    let state_map: HashMap<String, SyncStateRow> =
        rows.into_iter().map(|r| (r.path.clone(), r)).collect();

    // 4. Build HashSet of local file paths
    let local_set: HashSet<String> = local_files.iter().map(|f| f.rel_path.clone()).collect();

    // Phase 1: Process local files — identify uploads and conflicts
    let mut actions: Vec<PushAction> = Vec::new();

    for info in &local_files {
        // Warn and skip files in _plug/ directory
        if info.rel_path.starts_with("_plug/") {
            tracing::warn!("locally modified file in _plug/ skipped: {}", info.rel_path);
            continue;
        }

        match state_map.get(&info.rel_path) {
            None => {
                // New local file: upload without meta check
                actions.push(PushAction::Upload { info: info.clone() });
            }
            Some(row) => match plan_for_row(&info.hash, row) {
                RowPlan::Skip => {}
                RowPlan::Upload => actions.push(PushAction::Upload { info: info.clone() }),
                RowPlan::CheckAndUpload => actions.push(PushAction::CheckAndUpload {
                    info: info.clone(),
                    stored_remote_mtime: row.remote_mtime,
                    stored_remote_etag: row.remote_etag.clone(),
                }),
            },
        }
    }

    // Phase 2: Detect local deletions
    for (path, row) in &state_map {
        if row.status != SyncStatus::Synced {
            continue; // Only handle clean synced rows
        }
        if local_set.contains(path) {
            continue; // Still exists locally
        }

        // File deleted locally: check server state
        actions.push(PushAction::CheckAndDelete {
            path: path.clone(),
            stored_remote_mtime: row.remote_mtime,
            stored_remote_etag: row.remote_etag.clone(),
        });
    }

    // Phase 3: Execute actions concurrently
    let action_count = actions.len();
    let semaphore = Arc::new(Semaphore::new(workers as usize));
    let mut join_set: JoinSet<(String, SbResult<PushOutcome>)> = JoinSet::new();

    let space_root = space_root.to_path_buf();
    let sb_dir = sb_dir.to_path_buf();
    let client = client.clone();

    for action in actions {
        let sem = semaphore.clone();
        let space = space_root.clone();
        let sb = sb_dir.clone();
        let cl = client.clone();

        let path_for_task = action.path().to_string();
        let refused_hash = action.local_hash().map(str::to_string);

        join_set.spawn(async move {
            let outcome = async {
                let _permit = sem.acquire().await.map_err(|e| SbError::Filesystem {
                    message: format!("semaphore closed: {e}"),
                    path: String::new(),
                    source: None,
                })?;
                execute_push_action(action, &cl, &space, &sb).await
            }
            .await;

            // A 403 is the server saying "this path is read-only", not a broken
            // token and not a reason to abandon the other files. Record it so the
            // next push skips it until the file changes again.
            let outcome = match outcome {
                Err(SbError::ReadOnly { .. }) => Ok(PushOutcome::ReadOnly(match refused_hash {
                    Some(local_hash) => {
                        tracing::warn!("{}: read-only on the server, not uploaded", path_for_task);
                        SyncResult::ReadOnly {
                            path: path_for_task.clone(),
                            local_hash,
                        }
                    }
                    // A refused DELETE. Marking the row read-only would leave it
                    // with no local file and no refused bytes: push only deletes
                    // 'synced' rows and pull leaves read-only ones alone, so
                    // nothing would ever retry and the file would never return.
                    // Drop the row instead — the next pull re-downloads the file,
                    // which is the honest report of "the server is keeping this".
                    None => {
                        tracing::warn!(
                            "{}: read-only on the server, not deleted; the next pull restores it",
                            path_for_task
                        );
                        SyncResult::Deleted {
                            path: path_for_task.clone(),
                        }
                    }
                })),
                other => other,
            };
            (path_for_task, outcome)
        });
    }

    // Create progress bar for TTY display
    let progress = SyncProgress::new(action_count as u64, show_progress);
    progress.set_message("Pushing");

    // Collect results
    let mut result = PushResult::default();

    while let Some(joined) = join_set.join_next().await {
        let (path, outcome) = joined.map_err(|e| SbError::Filesystem {
            message: format!("task panicked: {e}"),
            path: String::new(),
            source: None,
        })?;

        match outcome {
            Ok(PushOutcome::Uploaded(sync_result)) => {
                result.uploaded += 1;
                result.results.push(sync_result);
            }
            Ok(PushOutcome::Conflict(sync_result)) => {
                result.conflicts += 1;
                result.results.push(sync_result);
            }
            Ok(PushOutcome::Deleted(sync_result)) => {
                result.deleted += 1;
                result.results.push(sync_result);
            }
            Ok(PushOutcome::ReadOnly(sync_result)) => {
                result.readonly += 1;
                result.results.push(sync_result);
            }
            Ok(PushOutcome::Skipped(sync_result)) => {
                result.skipped += 1;
                result.results.push(sync_result);
            }
            // Collect, do not bail: returning here would drop the JoinSet
            // (aborting uploads still in flight) and throw away every result
            // already collected, because the caller only commits on Ok.
            Err(e) => {
                tracing::warn!("push failed for {path}: {e}");
                result.failures.push((path, e));
            }
        }
        progress.inc();
    }

    progress.finish();
    Ok(result)
}

/// Plan which files need to be pushed without executing any I/O.
///
/// Returns a list of planned actions for display in dry-run mode. Makes HTTP
/// GET meta requests to check server state (read-only), but does NOT upload,
/// download, delete, or write to filesystem or state.db.
///
/// plan_push makes only GET meta calls (read-only); no PUT/DELETE in dry-run path.
pub async fn plan_push(
    client: &SbClient,
    space_root: &Path,
    sb_dir: &Path,
    db_path: &Path,
    filter: &FileFilter,
) -> SbResult<Vec<crate::sync::SyncAction>> {
    use crate::sync::SyncAction;

    // 1. Scan local files via spawn_blocking, passing the real filter directly now that
    //    FileFilter: Clone.
    let space_root_owned = space_root.to_path_buf();
    let scanner = LocalScanner::new(filter.clone());
    let local_files: Vec<LocalFileInfo> =
        tokio::task::spawn_blocking(move || scanner.scan(&space_root_owned))
            .await
            .map_err(|e| SbError::Internal {
                message: format!("scan task panicked: {e}"),
            })??;

    // 2. Load all state.db rows
    let db_path_owned = db_path.to_path_buf();
    let rows = tokio::task::spawn_blocking(move || {
        let db = StateDb::open(&db_path_owned)?;
        db.get_all_rows()
    })
    .await
    .map_err(|e| SbError::Filesystem {
        message: format!("spawn_blocking panicked loading state: {e}"),
        path: db_path.display().to_string(),
        source: None,
    })??;

    // 3. Build HashMaps
    let state_map: HashMap<String, SyncStateRow> =
        rows.into_iter().map(|r| (r.path.clone(), r)).collect();
    let local_set: HashSet<String> = local_files.iter().map(|f| f.rel_path.clone()).collect();

    let mut actions: Vec<SyncAction> = Vec::new();

    // Phase 1: Process local files — identify uploads and conflicts.
    // Files not in state.db (new) are collected directly; locally modified files need
    // a server meta check. Parallelize those meta checks with JoinSet.
    struct ModifiedEntry {
        path: String,
        stored_remote_mtime: i64,
    }
    let mut modified_entries: Vec<ModifiedEntry> = Vec::new();

    for info in &local_files {
        // Skip _plug/ files
        if info.rel_path.starts_with("_plug/") {
            continue;
        }

        match state_map.get(&info.rel_path) {
            None => {
                // New local file: plan upload without meta check
                actions.push(SyncAction::Upload {
                    path: info.rel_path.clone(),
                    reason: "new local file".into(),
                });
            }
            Some(row) => match plan_for_row(&info.hash, row) {
                RowPlan::Skip => {}
                RowPlan::Upload => actions.push(SyncAction::Upload {
                    path: info.rel_path.clone(),
                    reason: "retrying a path the server refused as read-only".into(),
                }),
                RowPlan::CheckAndUpload => modified_entries.push(ModifiedEntry {
                    path: info.rel_path.clone(),
                    stored_remote_mtime: row.remote_mtime,
                }),
            },
        }
    }

    // Parallel meta fetches for modified files
    if !modified_entries.is_empty() {
        let mut join_set: JoinSet<(String, SbResult<i64>, i64)> = JoinSet::new();
        for entry in modified_entries {
            let cl = client.clone();
            join_set.spawn(async move {
                let result = cl.get_file_meta(&entry.path).await;
                (entry.path, result, entry.stored_remote_mtime)
            });
        }
        while let Some(task_result) = join_set.join_next().await {
            let (path, meta_result, stored_remote_mtime) =
                task_result.map_err(|e| SbError::Internal {
                    message: format!("meta task panicked: {e}"),
                })?;
            let server_mtime = meta_result?;
            if server_mtime != stored_remote_mtime {
                // Server also changed — conflict
                actions.push(SyncAction::Conflict {
                    path,
                    reason: "server changed since last sync".into(),
                });
            } else {
                // Server unchanged — safe to upload
                actions.push(SyncAction::Upload {
                    path,
                    reason: "locally modified".into(),
                });
            }
        }
    }

    // Phase 2: Detect local deletions.
    // Collect deleted-locally paths and parallelize their meta checks with JoinSet.
    struct DeletedEntry {
        path: String,
        stored_remote_mtime: i64,
    }
    let mut deleted_entries: Vec<DeletedEntry> = Vec::new();

    for (path, row) in &state_map {
        if row.status != crate::sync::SyncStatus::Synced {
            continue; // Only handle clean synced rows
        }
        if local_set.contains(path) {
            continue; // Still exists locally
        }
        deleted_entries.push(DeletedEntry {
            path: path.clone(),
            stored_remote_mtime: row.remote_mtime,
        });
    }

    // Parallel meta fetches for locally-deleted files
    if !deleted_entries.is_empty() {
        let mut join_set: JoinSet<(String, SbResult<i64>, i64)> = JoinSet::new();
        for entry in deleted_entries {
            let cl = client.clone();
            join_set.spawn(async move {
                let result = cl.get_file_meta(&entry.path).await;
                (entry.path, result, entry.stored_remote_mtime)
            });
        }
        while let Some(task_result) = join_set.join_next().await {
            let (path, meta_result, stored_remote_mtime) =
                task_result.map_err(|e| SbError::Internal {
                    message: format!("meta task panicked: {e}"),
                })?;
            match meta_result {
                Err(SbError::PageNotFound { .. }) => {
                    // Already deleted on server — plan as delete (both sides gone)
                    actions.push(SyncAction::DeleteRemote {
                        path,
                        reason: "deleted locally (already gone on server)".into(),
                    });
                }
                Err(e) => return Err(e),
                Ok(server_mtime) => {
                    if server_mtime != stored_remote_mtime {
                        // Server changed since last sync — conflict
                        actions.push(SyncAction::Conflict {
                            path,
                            reason: "deleted locally but server changed".into(),
                        });
                    } else {
                        // Server unchanged — plan remote delete
                        actions.push(SyncAction::DeleteRemote {
                            path,
                            reason: "deleted locally".into(),
                        });
                    }
                }
            }
        }
    }

    let _ = sb_dir; // sb_dir not needed for planning (no stash writes)
    Ok(actions)
}

/// Internal push actions.
enum PushAction {
    /// Upload a new local file (not in state.db).
    Upload { info: LocalFileInfo },
    /// Check server mtime, then upload if unchanged (or conflict if changed).
    CheckAndUpload {
        info: LocalFileInfo,
        stored_remote_mtime: i64,
        /// ETag of the server copy we last saw, sent as `If-Match`. `None` on a
        /// server that does not do conditional writes.
        stored_remote_etag: Option<String>,
    },
    /// Check server mtime, then delete remote if unchanged (or conflict if changed).
    CheckAndDelete {
        path: String,
        stored_remote_mtime: i64,
        stored_remote_etag: Option<String>,
    },
}

impl PushAction {
    fn path(&self) -> &str {
        match self {
            PushAction::Upload { info } | PushAction::CheckAndUpload { info, .. } => &info.rel_path,
            PushAction::CheckAndDelete { path, .. } => path,
        }
    }

    /// blake3 hash of the local content this action would send, if any.
    /// `None` for a delete — there is no local file left to hash.
    fn local_hash(&self) -> Option<&str> {
        match self {
            PushAction::Upload { info } | PushAction::CheckAndUpload { info, .. } => {
                Some(&info.hash)
            }
            PushAction::CheckAndDelete { .. } => None,
        }
    }
}

/// What push should do with a local file that already has a state.db row.
#[derive(Debug, PartialEq, Eq)]
enum RowPlan {
    /// Nothing to send.
    Skip,
    /// Upload without a server meta check.
    Upload,
    /// Check the server first, then upload (or conflict).
    CheckAndUpload,
}

/// Decide what to do with a tracked file, given the hash of its current bytes.
///
/// The `local_hash` comparison is also the escape hatch for read-only paths: a
/// row the server refused stores the hash of the refused bytes, so it compares
/// equal and is skipped — until the file is edited, at which point it is worth
/// another try. A read-only row that was never on the server (`remote_mtime` 0)
/// retries with a plain upload; checking an mtime we never recorded against the
/// server's real one would fake a conflict on every attempt.
fn plan_for_row(local_hash: &str, row: &SyncStateRow) -> RowPlan {
    if local_hash == row.local_hash.as_deref().unwrap_or("") {
        return RowPlan::Skip;
    }
    if row.status == SyncStatus::ReadOnly && row.remote_mtime == 0 {
        return RowPlan::Upload;
    }
    RowPlan::CheckAndUpload
}

/// Internal outcomes from push actions.
enum PushOutcome {
    Uploaded(SyncResult),
    Conflict(SyncResult),
    Deleted(SyncResult),
    /// Server refused the path as read-only (403).
    ReadOnly(SyncResult),
    /// Nothing to send, but the `state.db` row still needs the enclosed result
    /// written — a "conflict" whose two sides held identical bytes.
    Skipped(SyncResult),
}

/// Stash the server's copy of `path` under `.sb/conflicts/` and report a conflict.
///
/// `local` is the hash and mtime of the bytes we were about to send, or `None`
/// for a delete — there is nothing left on disk to compare. When it matches the
/// server's copy there is no conflict: both sides hold the same content and only
/// a stale `state.db` baseline said otherwise, so the row is repaired instead.
/// Left unrepaired the row conflicts again on every sync, which is how one page
/// collected 86 identical stash files.
///
/// A failed download is logged rather than fatal: recording the conflict matters
/// more than having a stash to diff against.
async fn stash_and_conflict(
    client: &SbClient,
    sb_dir: &Path,
    path: String,
    why: &str,
    local: Option<(&str, i64)>,
) -> SbResult<PushOutcome> {
    match client.get_file(&path).await {
        Ok((content, remote_etag)) => {
            if let Some((local_hash, local_mtime)) = local {
                if crate::sync::scanner::hash_bytes(&content) == local_hash {
                    tracing::info!("no conflict on {path}: local and remote content are identical");
                    return Ok(PushOutcome::Skipped(SyncResult::Synced {
                        remote_mtime: client.get_file_meta(&path).await.unwrap_or(0),
                        path,
                        local_hash: local_hash.to_string(),
                        remote_hash: local_hash.to_string(),
                        remote_etag,
                        local_mtime,
                    }));
                }
            }
            let stash_path = crate::sync::write_conflict_stash(sb_dir, &path, &content).await?;
            tracing::warn!(
                "conflict: {} ({}; remote version stashed to {})",
                path,
                why,
                stash_path.display()
            );
        }
        Err(e) => {
            tracing::warn!("could not download conflict remote version for {path}: {e}");
        }
    }
    Ok(PushOutcome::Conflict(SyncResult::Conflict {
        path,
        conflict_at: jiff::Zoned::now().timestamp().as_millisecond(),
    }))
}

/// Execute a single push action.
async fn execute_push_action(
    action: PushAction,
    client: &SbClient,
    space_root: &Path,
    sb_dir: &Path,
) -> SbResult<PushOutcome> {
    match action {
        PushAction::Upload { info } => {
            let local_path = space_root.join(&info.rel_path);
            let content = tokio::fs::read(&local_path)
                .await
                .map_err(|e| SbError::Filesystem {
                    message: "failed to read local file for upload".into(),
                    path: local_path.display().to_string(),
                    source: Some(e),
                })?;

            // No stored ETag for a file we have never synced, so no If-Match.
            let new_etag = client
                .put_file(&info.rel_path, bytes::Bytes::from(content), None)
                .await?;

            // Get new remote_mtime after upload
            let new_remote_mtime = client.get_file_meta(&info.rel_path).await.unwrap_or(0);

            let local_mtime = crate::sync::scanner::mtime_ms_from_path(&local_path).await;

            Ok(PushOutcome::Uploaded(SyncResult::Synced {
                path: info.rel_path,
                local_hash: info.hash.clone(),
                remote_hash: info.hash,
                remote_etag: new_etag,
                remote_mtime: new_remote_mtime,
                local_mtime,
            }))
        }

        PushAction::CheckAndUpload {
            info,
            stored_remote_mtime,
            stored_remote_etag,
        } => {
            // Check server state via X-Get-Meta
            let server_mtime = client.get_file_meta(&info.rel_path).await?;

            if server_mtime != stored_remote_mtime {
                // Server has also changed — conflict
                return stash_and_conflict(
                    client,
                    sb_dir,
                    info.rel_path.clone(),
                    "server changed since last sync",
                    Some((&info.hash, info.mtime_ms)),
                )
                .await;
            }

            // Server unchanged — safe to upload
            let local_path = space_root.join(&info.rel_path);
            let content = tokio::fs::read(&local_path)
                .await
                .map_err(|e| SbError::Filesystem {
                    message: "failed to read local file for upload".into(),
                    path: local_path.display().to_string(),
                    source: Some(e),
                })?;

            // If-Match when we hold an ETag: the mtime check above has a window
            // between GET and PUT, and a server that supports conditional writes
            // closes it. No stored ETag means no header and a plain
            // last-write-wins PUT, exactly as before.
            let put = client
                .put_file(
                    &info.rel_path,
                    bytes::Bytes::from(content),
                    stored_remote_etag.as_deref(),
                )
                .await;

            let new_etag = match put {
                Ok(etag) => etag,
                // 412: the server copy moved under us. Same outcome as an mtime
                // mismatch, so `sb sync resolve` handles it unchanged.
                Err(SbError::PreconditionFailed { .. }) => {
                    return stash_and_conflict(
                        client,
                        sb_dir,
                        info.rel_path.clone(),
                        "If-Match rejected: server copy changed",
                        Some((&info.hash, info.mtime_ms)),
                    )
                    .await;
                }
                Err(e) => return Err(e),
            };

            // Get updated remote_mtime
            let new_remote_mtime = client.get_file_meta(&info.rel_path).await.unwrap_or(0);

            let local_mtime = crate::sync::scanner::mtime_ms_from_path(&local_path).await;

            Ok(PushOutcome::Uploaded(SyncResult::Synced {
                path: info.rel_path,
                local_hash: info.hash.clone(),
                remote_hash: info.hash,
                remote_etag: new_etag,
                remote_mtime: new_remote_mtime,
                local_mtime,
            }))
        }

        PushAction::CheckAndDelete {
            path,
            stored_remote_mtime,
            stored_remote_etag,
        } => {
            // Check server state
            let server_mtime = match client.get_file_meta(&path).await {
                Ok(mtime) => mtime,
                Err(SbError::PageNotFound { .. }) => {
                    // Already deleted on server — just remove state row
                    return Ok(PushOutcome::Deleted(SyncResult::Deleted { path }));
                }
                Err(e) => return Err(e),
            };

            if server_mtime != stored_remote_mtime {
                // Server has changed since last sync — conflict
                return stash_and_conflict(
                    client,
                    sb_dir,
                    path,
                    "deleted locally but server changed",
                    None,
                )
                .await;
            }

            // Server unchanged — safe to delete remote, conditionally when we
            // hold an ETag for the version we think we are deleting.
            match client
                .delete_file(&path, stored_remote_etag.as_deref())
                .await
            {
                Ok(()) => Ok(PushOutcome::Deleted(SyncResult::Deleted { path })),
                Err(SbError::PreconditionFailed { .. }) => {
                    stash_and_conflict(
                        client,
                        sb_dir,
                        path,
                        "If-Match rejected: server copy changed",
                        None,
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use wiremock::matchers::{header as wm_header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::sync::db::StateDb;

    fn make_client(base_url: &str) -> SbClient {
        SbClient::new(base_url, "testtoken").expect("SbClient::new")
    }

    fn make_filter() -> FileFilter {
        FileFilter::new(&[], &[], false).expect("create filter")
    }

    fn setup_space(dir: &Path) -> PathBuf {
        let sb_dir = dir.join(".sb");
        fs::create_dir_all(&sb_dir).expect("create .sb dir");
        sb_dir.join("state.db")
    }

    // Test: push uploads a locally modified file when server metadata confirms unchanged
    #[tokio::test]
    async fn push_uploads_locally_modified_file_when_server_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        // Write a local file with content different from the stored hash
        let file_path = dir.path().join("page.md");
        fs::write(&file_path, b"modified content").expect("write file");
        let _modified_hash = crate::sync::scanner::hash_file(&file_path).expect("hash");

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

        // Pre-populate state.db: stored local hash is old (file was modified locally)
        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "page.md".to_string(),
                local_hash: original_hash.to_string(),
                remote_hash: original_hash.to_string(),
                remote_etag: None,
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        // X-Get-Meta check: server mtime matches stored (unchanged)
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000000000"),
            )
            .mount(&server)
            .await;
        // PUT upload
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // POST-upload meta check
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000001000"),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.uploaded, 1, "should upload 1 file");
        assert_eq!(result.conflicts, 0);
    }

    // Test: push marks conflict when server file has changed since last sync
    #[tokio::test]
    async fn push_marks_conflict_when_server_changed_since_last_sync() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        let file_path = dir.path().join("page.md");
        fs::write(&file_path, b"local modification").expect("write file");

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "page.md".to_string(),
                local_hash: original_hash.to_string(),
                remote_hash: original_hash.to_string(),
                remote_etag: None,
                remote_mtime: 1700000000000, // stored mtime
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        // X-Get-Meta returns DIFFERENT mtime (server changed) — first call
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000002000"), // different!
            )
            .mount(&server)
            .await;
        // Conflict stash: download remote content
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("server version"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.conflicts, 1, "should detect 1 conflict");
        assert_eq!(result.uploaded, 0, "should not upload on conflict");
    }

    // Test: a server mtime bump over content identical to ours is not a conflict.
    // The server touching a file it re-indexed is enough to trigger this, and each
    // such push used to leave another byte-identical stash behind.
    #[tokio::test]
    async fn push_repairs_the_row_instead_of_stashing_when_content_is_identical() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        let agreed = "the one true content";
        let file_path = dir.path().join("page.md");
        fs::write(&file_path, agreed.as_bytes()).expect("write file");

        let stale = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";
        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "page.md".to_string(),
                local_hash: stale.to_string(),
                remote_hash: stale.to_string(),
                remote_etag: None,
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000002000"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string(agreed))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.conflicts, 0, "identical content is not a conflict");
        assert_eq!(result.skipped, 1, "the row should be repaired, silently");
        assert!(
            !dir.path().join(".sb/conflicts").exists(),
            "nothing should have been stashed"
        );

        let mut db = StateDb::open(&db_path).expect("open db");
        db.commit_batch(&result.results).expect("commit");
        let row = db.get_row("page.md").expect("get row").expect("row exists");
        assert_eq!(row.status, SyncStatus::Synced);
        assert_eq!(row.remote_mtime, 1700000002000);
        assert_eq!(
            row.local_hash.as_deref(),
            Some(crate::sync::scanner::hash_bytes(agreed.as_bytes()).as_str())
        );
    }

    // Test: push on conflict stashes remote version to .sb/conflicts/
    #[tokio::test]
    async fn push_conflict_stashes_remote_version() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        let file_path = dir.path().join("page.md");
        fs::write(&file_path, b"local modification").expect("write file");

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "page.md".to_string(),
                local_hash: original_hash.to_string(),
                remote_hash: original_hash.to_string(),
                remote_etag: None,
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000002000"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("remote stash content"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.conflicts, 1);

        // Verify stash was created
        let conflicts_dir = dir.path().join(".sb/conflicts");
        assert!(conflicts_dir.exists(), "conflicts dir should be created");
        let stash_files: Vec<_> = fs::read_dir(&conflicts_dir)
            .expect("read conflicts dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(stash_files.len(), 1, "should have 1 stash file");
        let stash_content = fs::read_to_string(stash_files[0].path()).expect("read stash");
        assert_eq!(stash_content, "remote stash content");
    }

    // Test: push uploads new local files (on disk but not in state.db) without meta check
    #[tokio::test]
    async fn push_uploads_new_local_files_without_meta_check() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        StateDb::open(&db_path).expect("open db");

        let file_path = dir.path().join("new-page.md");
        fs::write(&file_path, b"new content").expect("write file");

        let server = MockServer::start().await;
        // PUT only — no X-Get-Meta check for new files
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/new-page.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // Post-upload meta check
        Mock::given(method("GET"))
            .and(wm_path("/.fs/new-page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000001000"),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.uploaded, 1, "should upload new file");
        assert_eq!(result.conflicts, 0);
    }

    // Test: push sends DELETE for locally deleted files when server unchanged
    #[tokio::test]
    async fn push_deletes_remote_file_when_locally_deleted_and_server_unchanged() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

        // State.db has "old-page.md" as synced, but the file doesn't exist locally
        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "old-page.md".to_string(),
                local_hash: original_hash.to_string(),
                remote_hash: original_hash.to_string(),
                remote_etag: None,
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }
        // File does NOT exist on disk

        let server = MockServer::start().await;
        // X-Get-Meta: server unchanged
        Mock::given(method("GET"))
            .and(wm_path("/.fs/old-page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000000000"), // same as stored
            )
            .mount(&server)
            .await;
        // DELETE call
        Mock::given(method("DELETE"))
            .and(wm_path("/.fs/old-page.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.deleted, 1, "should delete 1 remote file");
        assert_eq!(result.conflicts, 0);
    }

    // Test: push marks conflict for locally deleted files when server has changed
    #[tokio::test]
    async fn push_marks_conflict_when_locally_deleted_but_server_changed() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "old-page.md".to_string(),
                local_hash: original_hash.to_string(),
                remote_hash: original_hash.to_string(),
                remote_etag: None,
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        // X-Get-Meta: server HAS changed
        Mock::given(method("GET"))
            .and(wm_path("/.fs/old-page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000002000"), // different!
            )
            .mount(&server)
            .await;
        // Conflict stash: download remote content
        Mock::given(method("GET"))
            .and(wm_path("/.fs/old-page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("server changed content"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.conflicts, 1, "should detect 1 conflict");
        assert_eq!(result.deleted, 0, "should not delete on conflict");
    }

    // Test: push skips and warns for files in _plug/ directory
    #[tokio::test]
    async fn push_skips_plug_directory_files_with_warning() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        StateDb::open(&db_path).expect("open db");

        // Create a _plug/ file locally
        let plug_dir = dir.path().join("_plug");
        fs::create_dir_all(&plug_dir).expect("create _plug dir");
        fs::write(plug_dir.join("core.js"), b"plugin code").expect("write plugin");

        // Also create a normal file
        fs::write(dir.path().join("note.md"), b"normal content").expect("write note");

        let server = MockServer::start().await;
        // Only the normal file should be uploaded
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/note.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/note.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000001000"),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        // _plug/core.js should be skipped (not uploaded)
        // note.md should be uploaded (new file)
        assert_eq!(result.uploaded, 1, "only note.md should be uploaded");
        assert_eq!(
            result.skipped, 0,
            "skipped count may vary but _plug was not uploaded"
        );
    }

    // Test: push uses concurrent workers (Semaphore limits simultaneous uploads)
    #[tokio::test]
    async fn push_uses_concurrent_workers_with_semaphore() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        StateDb::open(&db_path).expect("open db");

        // Create 3 new local files
        for i in 1..=3 {
            fs::write(
                dir.path().join(format!("page{i}.md")),
                format!("content {i}"),
            )
            .expect("write file");
        }

        let server = MockServer::start().await;
        for i in 1..=3 {
            Mock::given(method("PUT"))
                .and(wm_path(format!("/.fs/page{i}.md")))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(wm_path(format!("/.fs/page{i}.md")))
                .and(wm_header("X-Get-Meta", "true"))
                .respond_with(
                    ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000001000"),
                )
                .mount(&server)
                .await;
        }

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        // Use workers=2 to limit concurrency
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            2,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.uploaded, 3, "all 3 new files should be uploaded");
    }

    // Test: push returns Vec of SyncResult entries for batch commit
    #[tokio::test]
    async fn push_returns_sync_results_for_batch_commit() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        StateDb::open(&db_path).expect("open db");

        fs::write(dir.path().join("page1.md"), b"content").expect("write file");

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/page1.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page1.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000001000"),
            )
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.results.len(), 1, "should have 1 SyncResult");
        match &result.results[0] {
            SyncResult::Synced { path, .. } => assert_eq!(path, "page1.md"),
            other => panic!("expected Synced result, got: {other:?}"),
        }
    }

    // Test: push handles server 404 on delete (file already gone) — resolves as Deleted
    #[tokio::test]
    async fn push_handles_server_404_on_delete_as_already_deleted() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "gone.md".to_string(),
                local_hash: original_hash.to_string(),
                remote_hash: original_hash.to_string(),
                remote_etag: None,
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        // X-Get-Meta returns 404 (already gone on server)
        Mock::given(method("GET"))
            .and(wm_path("/.fs/gone.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");

        assert_eq!(result.deleted, 1, "should count as deleted");
        assert_eq!(result.conflicts, 0);
    }

    // ------------------------------------------------------------------
    // plan_for_row: which files push touches, and the read-only escape hatch
    // ------------------------------------------------------------------

    fn row_with(status: SyncStatus, local_hash: &str, remote_mtime: i64) -> SyncStateRow {
        SyncStateRow {
            path: "page.md".into(),
            local_hash: Some(local_hash.into()),
            remote_hash: Some("blake3remote".into()),
            remote_etag: None,
            remote_mtime,
            local_mtime: 0,
            status,
            conflict_at: 0,
        }
    }

    #[test]
    fn unmodified_file_is_skipped() {
        let row = row_with(SyncStatus::Synced, "hash_a", 1000);
        assert_eq!(plan_for_row("hash_a", &row), RowPlan::Skip);
    }

    #[test]
    fn modified_file_is_checked_then_uploaded() {
        let row = row_with(SyncStatus::Synced, "hash_a", 1000);
        assert_eq!(plan_for_row("hash_b", &row), RowPlan::CheckAndUpload);
    }

    #[test]
    fn readonly_row_is_skipped_while_the_local_file_is_unchanged() {
        // The whole point: stop re-POSTing a file the server already refused.
        let row = row_with(SyncStatus::ReadOnly, "refused_hash", 0);
        assert_eq!(plan_for_row("refused_hash", &row), RowPlan::Skip);
    }

    #[test]
    fn editing_a_readonly_file_makes_push_try_again() {
        // The escape hatch. A permanently unpushable file would be worse than
        // the repeated 403s.
        let row = row_with(SyncStatus::ReadOnly, "refused_hash", 0);
        assert_eq!(
            plan_for_row("edited_hash", &row),
            RowPlan::Upload,
            "a never-synced read-only row has no remote_mtime worth checking"
        );
    }

    #[test]
    fn readonly_row_that_was_once_synced_keeps_its_conflict_check() {
        // remote_mtime is real here, so the server-changed check still applies.
        let row = row_with(SyncStatus::ReadOnly, "refused_hash", 1700000000000);
        assert_eq!(plan_for_row("edited_hash", &row), RowPlan::CheckAndUpload);
    }

    // ------------------------------------------------------------------
    // If-Match on the wire
    // ------------------------------------------------------------------

    /// state.db row for a file that is modified locally, with an optional ETag.
    fn seed_modified(dir: &Path, db_path: &Path, etag: Option<&str>) {
        fs::write(dir.join("page.md"), b"new local content").expect("write file");
        let db = StateDb::open(db_path).expect("open db");
        db.upsert_row(&SyncStateRow {
            path: "page.md".into(),
            local_hash: Some("stale_blake3_hash".into()),
            remote_hash: Some("stale_blake3_hash".into()),
            remote_etag: etag.map(str::to_string),
            remote_mtime: 1700000000000,
            local_mtime: 0,
            status: SyncStatus::Synced,
            conflict_at: 0,
        })
        .expect("seed row");
    }

    async fn mock_meta(server: &MockServer, mtime: &str) {
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(ResponseTemplate::new(200).insert_header("X-Last-Modified", mtime))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn push_sends_if_match_when_an_etag_is_stored_and_records_the_new_one() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        seed_modified(dir.path(), &db_path, Some("\"sha256:old\""));

        let server = MockServer::start().await;
        mock_meta(&server, "1700000000000").await;
        // Only a PUT carrying the stored ETag matches; without the header
        // wiremock 404s and the push fails.
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/page.md"))
            .and(wm_header("If-Match", "\"sha256:old\""))
            .respond_with(ResponseTemplate::new(200).insert_header("ETag", "\"sha256:new\""))
            .expect(1)
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let result = push(
            &make_client(&server.uri()),
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed");
        assert_eq!(result.uploaded, 1);
        assert!(result.failures.is_empty(), "{:?}", result.failures);

        // The ETag the PUT returned is what gets persisted.
        let mut db = StateDb::open(&db_path).expect("open db");
        db.commit_batch(&result.results).expect("commit");
        let row = db.get_row("page.md").expect("get").expect("exists");
        assert_eq!(row.remote_etag, Some("\"sha256:new\"".to_string()));
        assert_ne!(
            row.remote_hash, row.remote_etag,
            "blake3 hash and SHA-256 ETag are different things"
        );
    }

    #[tokio::test]
    async fn push_sends_no_if_match_when_the_column_is_null() {
        // The 2.10.0 server, and every state.db written before this feature:
        // unconditional PUT, same behaviour as before.
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        seed_modified(dir.path(), &db_path, None);

        let server = MockServer::start().await;
        mock_meta(&server, "1700000000000").await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/page.md"))
            .and(wiremock::matchers::header_exists("If-Match"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let result = push(
            &make_client(&server.uri()),
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push should succeed against a server without ETags");
        assert_eq!(result.uploaded, 1);

        let mut db = StateDb::open(&db_path).expect("open db");
        db.commit_batch(&result.results).expect("commit");
        assert_eq!(
            db.get_row("page.md")
                .expect("get")
                .expect("exists")
                .remote_etag,
            None
        );
    }

    #[tokio::test]
    async fn a_412_conflicts_and_stashes_instead_of_clobbering() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        seed_modified(dir.path(), &db_path, Some("\"sha256:stale\""));

        let server = MockServer::start().await;
        mock_meta(&server, "1700000000000").await;
        // Server copy moved between our meta check and the PUT.
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(412))
            .mount(&server)
            .await;
        // The conflict path downloads the server copy to stash it.
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("server side content"))
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let mut result = push(
            &make_client(&server.uri()),
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("a 412 is a conflict, not a push failure");

        assert_eq!(result.conflicts, 1);
        assert_eq!(result.uploaded, 0);
        assert!(result.failures.is_empty());

        // The local file is untouched...
        assert_eq!(
            fs::read_to_string(dir.path().join("page.md")).expect("read local"),
            "new local content"
        );
        // ...the server copy is stashed for `sb sync resolve`...
        let stashed: Vec<String> = walkdir::WalkDir::new(sb_dir.join("conflicts"))
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| fs::read_to_string(e.path()).expect("read stash"))
            .collect();
        assert_eq!(stashed, vec!["server side content".to_string()]);

        // ...and the row lands in conflict, which is what `resolve` looks for.
        let mut db = StateDb::open(&db_path).expect("open db");
        db.commit_batch(&std::mem::take(&mut result.results))
            .expect("commit");
        assert_eq!(
            db.get_row("page.md").expect("get").expect("exists").status,
            SyncStatus::Conflict
        );
    }

    // ------------------------------------------------------------------
    // 403: read-only paths
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn one_403_does_not_stop_the_other_files_from_uploading() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        for name in ["a.md", "b.md", "c.md"] {
            fs::write(dir.path().join(name), b"content").expect("write");
        }

        let server = MockServer::start().await;
        // b.md is read-only; a.md and c.md upload fine. All three are new
        // files, so push goes straight to PUT.
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/b.md"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000009000"),
            )
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let mut result = push(
            &make_client(&server.uri()),
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("a read-only path must not fail the whole push");

        assert_eq!(result.uploaded, 2, "a.md and c.md should still upload");
        assert_eq!(result.readonly, 1);
        assert!(result.failures.is_empty(), "403 is recorded, not a failure");

        let mut db = StateDb::open(&db_path).expect("open db");
        db.commit_batch(&std::mem::take(&mut result.results))
            .expect("commit");
        for name in ["a.md", "c.md"] {
            assert_eq!(
                db.get_row(name).expect("get").expect("exists").status,
                SyncStatus::Synced,
                "{name} uploaded, so its work must survive b.md's refusal"
            );
        }
        assert_eq!(
            db.get_row("b.md").expect("get").expect("exists").status,
            SyncStatus::ReadOnly
        );
    }

    #[tokio::test]
    async fn a_readonly_row_is_skipped_with_no_http_call_until_the_file_changes() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        let file = dir.path().join("Library/Std/Config.md");
        fs::create_dir_all(file.parent().unwrap()).expect("mkdir");
        fs::write(&file, b"refused content").expect("write");

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/Library/Std/Config.md"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1) // exactly one attempt across BOTH pushes below
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let client = make_client(&server.uri());

        let mut first = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("first push");
        assert_eq!(first.readonly, 1);
        StateDb::open(&db_path)
            .expect("open db")
            .commit_batch(&std::mem::take(&mut first.results))
            .expect("commit");

        // Second push: file unchanged, so nothing is attempted at all.
        let second = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("second push");
        assert_eq!(second.readonly, 0);
        assert_eq!(second.uploaded, 0);
        assert!(second.failures.is_empty());
        // .expect(1) above is the real assertion: a second PUT would fail it.
    }

    /// A read-only row that later uploads must come all the way back to
    /// 'synced'. Editing the file is only the retry; this is the flag clearing.
    #[tokio::test]
    async fn a_successful_push_returns_a_readonly_row_to_synced() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        let file = dir.path().join("Library/Std/Config.md");
        fs::create_dir_all(file.parent().unwrap()).expect("mkdir");
        fs::write(&file, b"refused content").expect("write");

        let server = MockServer::start().await;
        // Refused once, then the path becomes writable (SB_READ_ONLY off).
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/Library/Std/Config.md"))
            .respond_with(ResponseTemplate::new(403))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/Library/Std/Config.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000009000"),
            )
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let client = make_client(&server.uri());

        let mut first = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("first push");
        assert_eq!(first.readonly, 1);
        StateDb::open(&db_path)
            .expect("open db")
            .commit_batch(&std::mem::take(&mut first.results))
            .expect("commit");

        // Edit the file: that, and only that, is what retries the upload.
        fs::write(&file, b"edited content").expect("rewrite");

        let mut second = push(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("second push");
        assert_eq!(second.uploaded, 1, "an edit must get another try");

        let mut db = StateDb::open(&db_path).expect("open db");
        db.commit_batch(&std::mem::take(&mut second.results))
            .expect("commit");
        assert_eq!(
            db.get_row("Library/Std/Config.md")
                .expect("get")
                .expect("exists")
                .status,
            SyncStatus::Synced,
            "the flag must clear, not linger on a file that now uploads fine"
        );
    }

    #[tokio::test]
    async fn a_still_readonly_path_stays_readonly_after_an_edit() {
        // The other half: the retry happens, the server refuses again, and the
        // row must go back to 'readonly' with the newly refused hash so the
        // push after this one skips it again.
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        let file = dir.path().join("Library/Std/Config.md");
        fs::create_dir_all(file.parent().unwrap()).expect("mkdir");
        fs::write(&file, b"refused content").expect("write");

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/Library/Std/Config.md"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let client = make_client(&server.uri());
        let mut db = StateDb::open(&db_path).expect("open db");

        for content in [b"refused content".as_slice(), b"edited again".as_slice()] {
            fs::write(&file, content).expect("write");
            let mut result = push(
                &client,
                dir.path(),
                &sb_dir,
                &db_path,
                &make_filter(),
                4,
                false,
            )
            .await
            .expect("push");
            assert_eq!(result.readonly, 1);
            db.commit_batch(&std::mem::take(&mut result.results))
                .expect("commit");
        }

        let row = db
            .get_row("Library/Std/Config.md")
            .expect("get")
            .expect("exists");
        assert_eq!(row.status, SyncStatus::ReadOnly);
        assert_eq!(
            row.local_hash,
            Some(crate::sync::scanner::hash_file(&file).expect("hash")),
            "the row must track the latest refused bytes, not the first"
        );
        assert_eq!(
            fs::read_to_string(&file).expect("read"),
            "edited again",
            "a refusal never touches the file"
        );
    }

    #[tokio::test]
    async fn a_refused_delete_leaves_no_row_behind_to_strand_the_file() {
        // A 403 on DELETE used to write local_hash = NULL, status = 'readonly':
        // push only deletes 'synced' rows and pull leaves read-only rows alone,
        // so the file was gone locally, alive on the server, and unreachable by
        // either half of sync forever.
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());

        // Tracked and synced, but no longer on disk: a local deletion.
        {
            let db = StateDb::open(&db_path).expect("open db");
            db.upsert_row(&SyncStateRow {
                path: "Library/Std/Config.md".into(),
                local_hash: Some("known".into()),
                remote_hash: Some("known".into()),
                remote_etag: None,
                remote_mtime: 1700000000000,
                local_mtime: 0,
                status: SyncStatus::Synced,
                conflict_at: 0,
            })
            .expect("seed row");
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000000000"),
            )
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let mut result = push(
            &make_client(&server.uri()),
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("a refused delete is recorded, not fatal");

        assert_eq!(result.readonly, 1, "the refusal is still reported");
        assert!(result.failures.is_empty());

        let mut db = StateDb::open(&db_path).expect("open db");
        db.commit_batch(&std::mem::take(&mut result.results))
            .expect("commit");
        assert!(
            db.get_row("Library/Std/Config.md").expect("get").is_none(),
            "no row means the next pull sees an untracked server file and restores it"
        );
    }

    #[tokio::test]
    async fn a_500_is_collected_as_a_failure_without_losing_the_other_upload() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_space(dir.path());
        fs::write(dir.path().join("good.md"), b"content").expect("write");
        fs::write(dir.path().join("bad.md"), b"content").expect("write");

        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/bad.md"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(wm_path("/.fs/good.md"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_header("X-Get-Meta", "true"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("X-Last-Modified", "1700000009000"),
            )
            .mount(&server)
            .await;

        let sb_dir = dir.path().join(".sb");
        let result = push(
            &make_client(&server.uri()),
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("push returns Ok so the caller can commit the successes");

        assert_eq!(result.uploaded, 1, "good.md must not be thrown away");
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].0, "bad.md");
    }
}
