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
use crate::sync::{conflict_stash_path, SyncResult, SyncStateRow, SyncStatus};

/// Summary of a push operation.
#[derive(Debug, Default)]
pub struct PushResult {
    pub uploaded: usize,
    pub conflicts: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub results: Vec<SyncResult>,
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
            Some(row) => {
                let stored_local_hash = row.local_hash.as_deref().unwrap_or("");
                if info.hash != stored_local_hash {
                    // Locally modified: check server via X-Get-Meta
                    actions.push(PushAction::CheckAndUpload {
                        info: info.clone(),
                        stored_remote_mtime: row.remote_mtime,
                    });
                }
                // If hash matches: unmodified, skip
            }
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
        });
    }

    // Phase 3: Execute actions concurrently
    let action_count = actions.len();
    let semaphore = Arc::new(Semaphore::new(workers as usize));
    let mut join_set: JoinSet<SbResult<PushOutcome>> = JoinSet::new();

    let space_root = space_root.to_path_buf();
    let sb_dir = sb_dir.to_path_buf();
    let client = client.clone();

    for action in actions {
        let sem = semaphore.clone();
        let space = space_root.clone();
        let sb = sb_dir.clone();
        let cl = client.clone();

        join_set.spawn(async move {
            let _permit = sem.acquire().await.map_err(|e| SbError::Filesystem {
                message: format!("semaphore closed: {e}"),
                path: String::new(),
                source: None,
            })?;
            execute_push_action(action, &cl, &space, &sb).await
        });
    }

    // Create progress bar for TTY display
    let progress = SyncProgress::new(action_count as u64, show_progress);
    progress.set_message("Pushing");

    // Collect results
    let mut result = PushResult::default();

    while let Some(outcome) = join_set.join_next().await {
        let outcome = outcome.map_err(|e| SbError::Filesystem {
            message: format!("task panicked: {e}"),
            path: String::new(),
            source: None,
        })??;

        match outcome {
            PushOutcome::Uploaded(sync_result) => {
                result.uploaded += 1;
                result.results.push(sync_result);
            }
            PushOutcome::Conflict(sync_result) => {
                result.conflicts += 1;
                result.results.push(sync_result);
            }
            PushOutcome::Deleted(sync_result) => {
                result.deleted += 1;
                result.results.push(sync_result);
            }
            PushOutcome::Skipped => {
                result.skipped += 1;
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
            Some(row) => {
                let stored_local_hash = row.local_hash.as_deref().unwrap_or("");
                if info.hash != stored_local_hash {
                    // Locally modified — queue for parallel meta check
                    modified_entries.push(ModifiedEntry {
                        path: info.rel_path.clone(),
                        stored_remote_mtime: row.remote_mtime,
                    });
                }
                // If hash matches: unmodified, no action
            }
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
    },
    /// Check server mtime, then delete remote if unchanged (or conflict if changed).
    CheckAndDelete {
        path: String,
        stored_remote_mtime: i64,
    },
}

/// Internal outcomes from push actions.
enum PushOutcome {
    Uploaded(SyncResult),
    Conflict(SyncResult),
    Deleted(SyncResult),
    #[allow(dead_code)]
    Skipped, // reserved for future use
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

            client
                .put_file(&info.rel_path, bytes::Bytes::from(content))
                .await?;

            // Get new remote_mtime after upload
            let new_remote_mtime = client.get_file_meta(&info.rel_path).await.unwrap_or(0);

            let local_mtime = crate::sync::scanner::mtime_ms_from_path(&local_path).await;

            Ok(PushOutcome::Uploaded(SyncResult::Synced {
                path: info.rel_path,
                local_hash: info.hash.clone(),
                remote_hash: info.hash,
                remote_mtime: new_remote_mtime,
                local_mtime,
            }))
        }

        PushAction::CheckAndUpload {
            info,
            stored_remote_mtime,
        } => {
            // Check server state via X-Get-Meta
            let server_mtime = client.get_file_meta(&info.rel_path).await?;

            if server_mtime != stored_remote_mtime {
                // Server has also changed — conflict
                // Download remote version and stash it
                match client.get_file(&info.rel_path).await {
                    Ok(content) => {
                        let stash_path = conflict_stash_path(sb_dir, &info.rel_path);
                        if let Some(parent) = stash_path.parent() {
                            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                                SbError::Filesystem {
                                    message: "failed to create conflict stash directory".into(),
                                    path: parent.display().to_string(),
                                    source: Some(e),
                                }
                            })?;
                        }
                        tokio::fs::write(&stash_path, &content).await.map_err(|e| {
                            SbError::Filesystem {
                                message: "failed to write conflict stash file".into(),
                                path: stash_path.display().to_string(),
                                source: Some(e),
                            }
                        })?;
                        tracing::warn!(
                            "conflict: {} (remote version stashed to {})",
                            info.rel_path,
                            stash_path.display()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "could not download conflict remote version for {}: {e}",
                            info.rel_path
                        );
                    }
                }
                return Ok(PushOutcome::Conflict(SyncResult::Conflict {
                    path: info.rel_path,
                    conflict_at: jiff::Zoned::now().timestamp().as_millisecond(),
                }));
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

            client
                .put_file(&info.rel_path, bytes::Bytes::from(content))
                .await?;

            // Get updated remote_mtime
            let new_remote_mtime = client.get_file_meta(&info.rel_path).await.unwrap_or(0);

            let local_mtime = crate::sync::scanner::mtime_ms_from_path(&local_path).await;

            Ok(PushOutcome::Uploaded(SyncResult::Synced {
                path: info.rel_path,
                local_hash: info.hash.clone(),
                remote_hash: info.hash,
                remote_mtime: new_remote_mtime,
                local_mtime,
            }))
        }

        PushAction::CheckAndDelete {
            path,
            stored_remote_mtime,
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
                // Download remote version to stash
                match client.get_file(&path).await {
                    Ok(content) => {
                        let stash_path = conflict_stash_path(sb_dir, &path);
                        if let Some(parent) = stash_path.parent() {
                            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                                SbError::Filesystem {
                                    message: "failed to create conflict stash directory".into(),
                                    path: parent.display().to_string(),
                                    source: Some(e),
                                }
                            })?;
                        }
                        tokio::fs::write(&stash_path, &content).await.map_err(|e| {
                            SbError::Filesystem {
                                message: "failed to write conflict stash file".into(),
                                path: stash_path.display().to_string(),
                                source: Some(e),
                            }
                        })?;
                        tracing::warn!(
                            "conflict: {} (deleted locally but server changed; stashed to {})",
                            path,
                            stash_path.display()
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            "could not download conflict remote version for {}: {e}",
                            path
                        );
                    }
                }
                return Ok(PushOutcome::Conflict(SyncResult::Conflict {
                    path,
                    conflict_at: jiff::Zoned::now().timestamp().as_millisecond(),
                }));
            }

            // Server unchanged — safe to delete remote
            client.delete_file(&path).await?;
            Ok(PushOutcome::Deleted(SyncResult::Deleted { path }))
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
}
