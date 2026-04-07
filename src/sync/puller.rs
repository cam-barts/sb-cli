use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::client::{FileMeta, SbClient};
use crate::commands::page::validate_page_path;
use crate::error::{SbError, SbResult};
use crate::sync::db::StateDb;
use crate::sync::progress::SyncProgress;
use crate::sync::scanner::{hash_file, FileFilter};
use crate::sync::{conflict_stash_path, SyncResult, SyncStateRow, SyncStatus};

/// Summary of a pull operation.
#[derive(Debug, Default)]
pub struct PullResult {
    pub downloaded: usize,
    pub conflicts: usize,
    pub deleted: usize,
    pub skipped: usize,
    pub results: Vec<SyncResult>,
}

/// Pull remote changes into the local space.
///
/// Fetches the server file listing, compares against state.db, and downloads
/// new/changed files while detecting conflicts. Remote deletions are handled
/// by removing unmodified local files or marking conflicts for modified ones.
///
/// Uses Semaphore + JoinSet for bounded concurrent file downloads.
pub async fn pull(
    client: &SbClient,
    space_root: &Path,
    sb_dir: &Path,
    db_path: &Path,
    filter: &FileFilter,
    workers: u32,
    show_progress: bool,
) -> SbResult<PullResult> {
    // 1. Fetch server file listing
    let server_files = client.list_files().await?;

    // 2. Load all state.db rows via spawn_blocking
    let db_path_owned = db_path.to_path_buf();
    let rows = tokio::task::spawn_blocking(move || {
        let db = StateDb::open(&db_path_owned)?;
        db.get_all_rows()
    })
    .await
    .map_err(|e| SbError::Filesystem {
        message: format!("spawn_blocking panicked: {e}"),
        path: db_path.display().to_string(),
        source: None,
    })??;

    // 3. Build HashMaps for O(1) lookup
    let state_map: HashMap<String, SyncStateRow> =
        rows.into_iter().map(|r| (r.path.clone(), r)).collect();

    // 4. Build HashSet of server file names (for deletion detection)
    let server_set: HashSet<String> = server_files.iter().map(|m| m.name.clone()).collect();

    // Phase 1: Process server files — identify downloads and conflicts
    let mut actions: Vec<FileAction> = Vec::new();

    for meta in &server_files {
        // Skip files starting with .sb/
        if meta.name.starts_with(".sb/") {
            continue;
        }

        // Apply glob filter
        if !filter.should_sync(&meta.name) {
            continue;
        }

        // Validate path for traversal attacks
        if !is_safe_path(&meta.name) {
            tracing::warn!("skipping server file with unsafe path: {}", meta.name);
            continue;
        }
        // Extra: validate via validate_page_path for .md files
        if meta.name.ends_with(".md") {
            let name_without_ext = meta.name.trim_end_matches(".md");
            if validate_page_path(space_root, name_without_ext).is_err() {
                tracing::warn!("skipping server file with invalid path: {}", meta.name);
                continue;
            }
        }

        match state_map.get(&meta.name) {
            None => {
                // New remote file: download it
                actions.push(FileAction::Download { meta: meta.clone() });
            }
            Some(row) => {
                if meta.last_modified > row.remote_mtime {
                    // Remote changed: check if local is also modified
                    let local_path = space_root.join(&meta.name);
                    let local_hash = if local_path.exists() {
                        let p = local_path.clone();
                        tokio::task::spawn_blocking(move || hash_file(&p))
                            .await
                            .map_err(|e| SbError::Filesystem {
                                message: format!("spawn_blocking panicked: {e}"),
                                path: local_path.display().to_string(),
                                source: None,
                            })??
                    } else {
                        String::new() // file doesn't exist locally, treat as unmodified
                    };

                    let stored_local_hash = row.local_hash.as_deref().unwrap_or("");
                    if local_hash == stored_local_hash || stored_local_hash.is_empty() {
                        // Local unmodified — safe to download
                        actions.push(FileAction::Download { meta: meta.clone() });
                    } else {
                        // Both sides changed — conflict
                        actions.push(FileAction::Conflict {
                            path: meta.name.clone(),
                            meta: meta.clone(),
                        });
                    }
                }
                // If meta.last_modified == row.remote_mtime: unchanged, skip
            }
        }
    }

    // Phase 2: Detect remote deletions
    for (path, row) in &state_map {
        if server_set.contains(path) {
            continue; // Still on server
        }
        if row.status == SyncStatus::Conflict {
            continue; // Already conflicted, skip
        }

        let local_path = space_root.join(path);
        if !local_path.exists() {
            // File gone both locally and remotely — just delete the state row
            actions.push(FileAction::DeleteLocalState { path: path.clone() });
            continue;
        }

        // Check if local is modified
        let p = local_path.clone();
        let local_hash = tokio::task::spawn_blocking(move || hash_file(&p))
            .await
            .map_err(|e| SbError::Filesystem {
                message: format!("spawn_blocking panicked: {e}"),
                path: local_path.display().to_string(),
                source: None,
            })??;

        let stored_local_hash = row.local_hash.as_deref().unwrap_or("");
        if local_hash == stored_local_hash {
            // Local unmodified — safe to delete locally
            actions.push(FileAction::DeleteLocal { path: path.clone() });
        } else {
            // Local modified but remote deleted — conflict
            actions.push(FileAction::Conflict {
                path: path.clone(),
                meta: FileMeta {
                    name: path.clone(),
                    last_modified: 0,
                    created: 0,
                    content_type: "text/markdown".to_string(),
                    size: 0,
                    perm: None,
                },
            });
        }
    }

    // Phase 3: Execute actions concurrently
    let action_count = actions.len();
    let semaphore = Arc::new(Semaphore::new(workers as usize));
    let mut join_set: JoinSet<SbResult<ActionOutcome>> = JoinSet::new();

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
            execute_action(action, &cl, &space, &sb).await
        });
    }

    // Create progress bar for TTY display
    let progress = SyncProgress::new(action_count as u64, show_progress);
    progress.set_message("Pulling");

    // Collect results
    let mut result = PullResult::default();

    while let Some(outcome) = join_set.join_next().await {
        let outcome = outcome.map_err(|e| SbError::Filesystem {
            message: format!("task panicked: {e}"),
            path: String::new(),
            source: None,
        })??;

        match outcome {
            ActionOutcome::Downloaded(sync_result) => {
                result.downloaded += 1;
                result.results.push(sync_result);
            }
            ActionOutcome::Conflict(sync_result) => {
                result.conflicts += 1;
                result.results.push(sync_result);
            }
            ActionOutcome::Deleted(sync_result) => {
                result.deleted += 1;
                result.results.push(sync_result);
            }
            ActionOutcome::DeletedState(sync_result) => {
                result.deleted += 1;
                result.results.push(sync_result);
            }
            ActionOutcome::Skipped => {
                result.skipped += 1;
            }
        }
        progress.inc();
    }

    progress.finish();
    Ok(result)
}

/// Plan which files need to be pulled without executing any I/O.
///
/// Returns a list of planned actions for display in dry-run mode. Does NOT
/// download files, write to disk, or update state.db.
pub async fn plan_pull(
    client: &SbClient,
    space_root: &Path,
    _sb_dir: &Path,
    db_path: &Path,
    filter: &FileFilter,
) -> SbResult<Vec<crate::sync::SyncAction>> {
    use crate::sync::SyncAction;

    // 1. Fetch server file listing
    let server_files = client.list_files().await?;

    // 2. Load all state.db rows via spawn_blocking
    let db_path_owned = db_path.to_path_buf();
    let rows = tokio::task::spawn_blocking(move || {
        let db = StateDb::open(&db_path_owned)?;
        db.get_all_rows()
    })
    .await
    .map_err(|e| SbError::Filesystem {
        message: format!("spawn_blocking panicked: {e}"),
        path: db_path.display().to_string(),
        source: None,
    })??;

    // 3. Build HashMaps for O(1) lookup
    let state_map: HashMap<String, SyncStateRow> =
        rows.into_iter().map(|r| (r.path.clone(), r)).collect();

    // 4. Build HashSet of server file names (for deletion detection)
    let server_set: HashSet<String> = server_files.iter().map(|m| m.name.clone()).collect();

    let mut actions: Vec<SyncAction> = Vec::new();

    // Phase 1a: Process server files — decisions that don't require hashing, and
    // collect files that need a local hash comparison.
    //
    // Each entry: (meta, stored_local_hash) — stored_local_hash is "" when the row
    // has no local_hash recorded, meaning we treat the local file as unmodified.
    let mut p1_needs_hash: Vec<(FileMeta, String)> = Vec::new();

    for meta in &server_files {
        // Skip .sb/ files
        if meta.name.starts_with(".sb/") {
            continue;
        }
        // Apply glob filter
        if !filter.should_sync(&meta.name) {
            continue;
        }
        // Validate path for traversal attacks
        if !is_safe_path(&meta.name) {
            continue;
        }
        // Validate .md paths
        if meta.name.ends_with(".md") {
            let name_without_ext = meta.name.trim_end_matches(".md");
            if validate_page_path(space_root, name_without_ext).is_err() {
                continue;
            }
        }

        match state_map.get(&meta.name) {
            None => {
                // New remote file: plan download — no hash needed
                actions.push(SyncAction::Download {
                    path: meta.name.clone(),
                    remote_mtime: meta.last_modified,
                    reason: "new remote file".into(),
                });
            }
            Some(row) => {
                // Files already conflicted in state.db (Pitfall 6)
                if row.status == SyncStatus::Conflict {
                    actions.push(SyncAction::Conflict {
                        path: meta.name.clone(),
                        reason: "already conflicted -- run sb sync resolve".into(),
                    });
                    continue;
                }

                if meta.last_modified > row.remote_mtime {
                    // Remote changed: need local hash to decide Download vs Conflict.
                    // If the local file doesn't exist we don't need to hash it — treat
                    // as unmodified (empty hash == stored "" → Download).
                    let local_path = space_root.join(&meta.name);
                    if local_path.exists() {
                        let stored = row.local_hash.clone().unwrap_or_default();
                        p1_needs_hash.push((meta.clone(), stored));
                    } else {
                        // File doesn't exist locally → unmodified, safe to download
                        actions.push(SyncAction::Download {
                            path: meta.name.clone(),
                            remote_mtime: meta.last_modified,
                            reason: "remote newer".into(),
                        });
                    }
                }
                // If meta.last_modified == row.remote_mtime: unchanged, no action
            }
        }
    }

    // Phase 1b: Hash all Phase-1 candidates in parallel using JoinSet.
    {
        let mut join_set: tokio::task::JoinSet<Result<(String, i64, String, String), SbError>> =
            tokio::task::JoinSet::new();

        for (meta, stored_local_hash) in p1_needs_hash {
            let path = space_root.join(&meta.name);
            let name = meta.name.clone();
            let remote_mtime = meta.last_modified;
            join_set.spawn_blocking(move || {
                let hash = hash_file(&path).map_err(|e| SbError::Filesystem {
                    message: format!("hash_file failed: {e}"),
                    path: path.display().to_string(),
                    source: None,
                })?;
                Ok((name, remote_mtime, hash, stored_local_hash))
            });
        }

        while let Some(result) = join_set.join_next().await {
            let (name, remote_mtime, local_hash, stored_local_hash) =
                result.map_err(|e| SbError::Internal {
                    message: format!("hash task panicked: {e}"),
                })??;

            if local_hash == stored_local_hash || stored_local_hash.is_empty() {
                // Local unmodified — safe to download
                actions.push(SyncAction::Download {
                    path: name,
                    remote_mtime,
                    reason: "remote newer".into(),
                });
            } else {
                // Both sides changed — conflict
                actions.push(SyncAction::Conflict {
                    path: name,
                    reason: "both local and remote modified".into(),
                });
            }
        }
    }

    // Phase 2a: Detect remote deletions — decisions that don't require hashing, and
    // collect files that need a local hash to decide delete vs conflict.
    //
    // Each entry: (path, stored_local_hash)
    let mut p2_needs_hash: Vec<(String, String)> = Vec::new();

    for (path, row) in &state_map {
        if server_set.contains(path) {
            continue; // Still on server
        }
        if row.status == SyncStatus::Conflict {
            continue; // Already conflicted, skip
        }

        let local_path = space_root.join(path);
        if !local_path.exists() {
            // File gone both locally and remotely — cleanup, no hash needed
            actions.push(SyncAction::DeleteLocal {
                path: path.clone(),
                reason: "deleted on server".into(),
            });
            continue;
        }

        // Local file exists but was deleted on server — need to hash to decide
        let stored = row.local_hash.clone().unwrap_or_default();
        p2_needs_hash.push((path.clone(), stored));
    }

    // Phase 2b: Hash all Phase-2 candidates in parallel using JoinSet.
    {
        let mut join_set: tokio::task::JoinSet<Result<(String, String, String), SbError>> =
            tokio::task::JoinSet::new();

        for (path, stored_local_hash) in p2_needs_hash {
            let local_path = space_root.join(&path);
            join_set.spawn_blocking(move || {
                let hash = hash_file(&local_path).map_err(|e| SbError::Filesystem {
                    message: format!("hash_file failed: {e}"),
                    path: local_path.display().to_string(),
                    source: None,
                })?;
                Ok((path, hash, stored_local_hash))
            });
        }

        while let Some(result) = join_set.join_next().await {
            let (path, local_hash, stored_local_hash) =
                result.map_err(|e| SbError::Internal {
                    message: format!("hash task panicked: {e}"),
                })??;

            if local_hash == stored_local_hash {
                // Local unmodified — safe to delete locally
                actions.push(SyncAction::DeleteLocal {
                    path,
                    reason: "deleted on server".into(),
                });
            } else {
                // Local modified but remote deleted — conflict
                actions.push(SyncAction::Conflict {
                    path,
                    reason: "deleted on server but modified locally".into(),
                });
            }
        }
    }

    Ok(actions)
}

/// Internal actions computed during phase 1 and 2.
enum FileAction {
    Download { meta: FileMeta },
    Conflict { path: String, meta: FileMeta },
    DeleteLocal { path: String },
    DeleteLocalState { path: String },
}

/// Internal outcomes from executing actions.
enum ActionOutcome {
    Downloaded(SyncResult),
    Conflict(SyncResult),
    Deleted(SyncResult),
    DeletedState(SyncResult),
    #[allow(dead_code)]
    Skipped, // reserved for future use
}

/// Execute a single file action.
async fn execute_action(
    action: FileAction,
    client: &SbClient,
    space_root: &Path,
    sb_dir: &Path,
) -> SbResult<ActionOutcome> {
    match action {
        FileAction::Download { meta } => {
            let content = client.get_file(&meta.name).await?;
            let local_path = space_root.join(&meta.name);

            // Create parent dirs
            if let Some(parent) = local_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| SbError::Filesystem {
                        message: "failed to create parent directories".into(),
                        path: parent.display().to_string(),
                        source: Some(e),
                    })?;
            }

            tokio::fs::write(&local_path, &content)
                .await
                .map_err(|e| SbError::Filesystem {
                    message: "failed to write downloaded file".into(),
                    path: local_path.display().to_string(),
                    source: Some(e),
                })?;

            // Compute hash and mtime of downloaded content
            let local_hash = {
                let bytes = content.to_vec();
                let mut hasher = blake3::Hasher::new();
                hasher.update(&bytes);
                hasher.finalize().to_hex().to_string()
            };
            let remote_hash = local_hash.clone(); // just downloaded, they match

            let local_mtime = crate::sync::scanner::mtime_ms_from_path(&local_path).await;

            Ok(ActionOutcome::Downloaded(SyncResult::Synced {
                path: meta.name,
                local_hash,
                remote_hash,
                remote_mtime: meta.last_modified,
                local_mtime,
            }))
        }

        FileAction::Conflict { path, meta } => {
            // Download remote version and stash it
            if meta.last_modified > 0 {
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
                            "conflict: {} (remote version stashed to {})",
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
            } else {
                // Remote deletion conflict: no file to download, just warn
                tracing::warn!(
                    "conflict: {} (deleted on server but modified locally)",
                    path
                );
            }

            Ok(ActionOutcome::Conflict(SyncResult::Conflict {
                path,
                conflict_at: jiff::Zoned::now().timestamp().as_millisecond(),
            }))
        }

        FileAction::DeleteLocal { path } => {
            let local_path = space_root.join(&path);
            tokio::fs::remove_file(&local_path)
                .await
                .map_err(|e| SbError::Filesystem {
                    message: "failed to remove local file after remote deletion".into(),
                    path: local_path.display().to_string(),
                    source: Some(e),
                })?;
            Ok(ActionOutcome::Deleted(SyncResult::Deleted { path }))
        }

        FileAction::DeleteLocalState { path } => {
            // File gone both locally and remotely — just remove state entry
            Ok(ActionOutcome::DeletedState(SyncResult::Deleted { path }))
        }
    }
}

/// Validate that a path is safe (no ".." components, not absolute).
///
/// This protects against path traversal attacks from server-returned names.
fn is_safe_path(path: &str) -> bool {
    // Must not be absolute
    if path.starts_with('/') {
        return false;
    }
    // Must not contain ".." components
    for component in Path::new(path).components() {
        match component {
            std::path::Component::ParentDir => return false,
            std::path::Component::RootDir => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::sync::db::StateDb;

    fn make_filter() -> FileFilter {
        FileFilter::new(&[], &[], false).expect("create filter")
    }

    fn make_client(base_url: &str) -> SbClient {
        SbClient::new(base_url, "testtoken").expect("SbClient::new")
    }

    fn make_file_meta(name: &str, last_modified: i64) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "lastModified": last_modified,
            "created": 1000000000000i64,
            "contentType": "text/markdown",
            "size": 100,
        })
    }

    fn setup_db(dir: &Path) -> PathBuf {
        let sb_dir = dir.join(".sb");
        fs::create_dir_all(&sb_dir).expect("create .sb dir");
        sb_dir.join("state.db")
    }

    // Test: pull downloads a new remote file (in listing, not in state.db) and writes to disk
    #[tokio::test]
    async fn pull_downloads_new_remote_file_to_disk() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());
        // Initialize empty state.db
        StateDb::open(&db_path).expect("open db");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                make_file_meta("notes/new-page.md", 1700000000000i64)
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/notes/new-page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# New Page\n"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.downloaded, 1, "should download 1 file");
        assert_eq!(result.conflicts, 0);

        let file_path = dir.path().join("notes/new-page.md");
        assert!(file_path.exists(), "downloaded file should exist on disk");
        let content = fs::read_to_string(&file_path).expect("read file");
        assert!(
            content.contains("New Page"),
            "content should match server response"
        );
    }

    // Test: pull downloads a changed remote file (lastModified > remote_mtime) when local is unmodified
    #[tokio::test]
    async fn pull_downloads_changed_remote_file_when_local_unmodified() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());

        // Write initial file with known content
        let file_path = dir.path().join("page.md");
        fs::write(&file_path, b"original content").expect("write file");
        let original_hash = hash_file(&file_path).expect("hash");

        // Pre-populate state.db with synced row
        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "page.md".to_string(),
                local_hash: original_hash.clone(),
                remote_hash: original_hash.clone(),
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!([make_file_meta("page.md", 1700000001000i64)]), // newer mtime
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("updated content"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.downloaded, 1, "should download 1 updated file");
        assert_eq!(result.conflicts, 0);

        let content = fs::read_to_string(&file_path).expect("read file");
        assert_eq!(content, "updated content", "file should be updated");
    }

    // Test: pull skips download and marks conflict when remote changed AND local also modified
    #[tokio::test]
    async fn pull_marks_conflict_when_both_local_and_remote_modified() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";

        // Write locally-modified content (different from stored hash)
        let file_path = dir.path().join("page.md");
        fs::write(&file_path, b"local modification").expect("write file");

        // Pre-populate state.db with stored hash matching original (not local modified)
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
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!([make_file_meta("page.md", 1700000001000i64)]), // remote newer
            ))
            .mount(&server)
            .await;
        // Conflict: also mock get_file for stashing
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("remote updated content"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.conflicts, 1, "should detect 1 conflict");
        assert_eq!(result.downloaded, 0, "should not download on conflict");

        // Local file should be preserved (not overwritten)
        let content = fs::read_to_string(&file_path).expect("read file");
        assert_eq!(
            content, "local modification",
            "local file should be preserved"
        );
    }

    // Test: pull on conflict: local file preserved, remote version stashed to .sb/conflicts/
    #[tokio::test]
    async fn pull_conflict_stashes_remote_version_to_sb_conflicts() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());

        let original_hash = "aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233";
        let file_path = dir.path().join("page.md");
        fs::write(&file_path, b"local modification").expect("write file");

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
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                make_file_meta("page.md", 1700000001000i64)
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("remote content for stash"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.conflicts, 1);

        // Verify conflict stash was created under .sb/conflicts/
        let conflicts_dir = dir.path().join(".sb/conflicts");
        assert!(conflicts_dir.exists(), "conflicts dir should be created");
        let stash_files: Vec<_> = fs::read_dir(&conflicts_dir)
            .expect("read conflicts dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(stash_files.len(), 1, "should have 1 stash file");

        let stash_path = stash_files[0].path();
        let stash_name = stash_path.file_name().unwrap().to_string_lossy();
        assert!(
            stash_name.starts_with("page."),
            "stash file name should start with 'page.'"
        );
        assert!(
            stash_name.ends_with(".md"),
            "stash file should have .md extension"
        );

        let stash_content = fs::read_to_string(&stash_path).expect("read stash");
        assert_eq!(
            stash_content, "remote content for stash",
            "stash should contain remote content"
        );
    }

    // Test: pull removes local file when file is absent from server AND local is unmodified
    #[tokio::test]
    async fn pull_removes_local_file_when_remote_deleted_and_local_unmodified() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());

        let file_path = dir.path().join("old-page.md");
        fs::write(&file_path, b"original content").expect("write file");
        let original_hash = hash_file(&file_path).expect("hash");

        // Pre-populate state.db
        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "old-page.md".to_string(),
                local_hash: original_hash.clone(),
                remote_hash: original_hash.clone(),
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        // Server returns empty listing (file deleted)
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.deleted, 1, "should delete 1 local file");
        assert!(!file_path.exists(), "local file should be removed");
    }

    // Test: pull marks conflict when file is absent from server AND local is modified
    #[tokio::test]
    async fn pull_marks_conflict_when_remote_deleted_but_local_modified() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());

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

        // Server returns empty listing (file deleted)
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.conflicts, 1, "should detect 1 conflict");
        // Local file should be preserved
        assert!(
            file_path.exists(),
            "local file should be preserved on conflict"
        );
    }

    // Test: pull does nothing for files whose server lastModified equals stored remote_mtime
    #[tokio::test]
    async fn pull_skips_unchanged_files() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());

        let file_path = dir.path().join("page.md");
        fs::write(&file_path, b"current content").expect("write file");
        let current_hash = hash_file(&file_path).expect("hash");

        {
            let mut db = StateDb::open(&db_path).expect("open db");
            db.commit_batch(&[SyncResult::Synced {
                path: "page.md".to_string(),
                local_hash: current_hash.clone(),
                remote_hash: current_hash.clone(),
                remote_mtime: 1700000000000,
                local_mtime: 0,
            }])
            .expect("commit");
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!([make_file_meta("page.md", 1700000000000i64)]), // same mtime
            ))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.downloaded, 0, "should not download unchanged file");
        assert_eq!(result.conflicts, 0);
        assert_eq!(result.deleted, 0);
    }

    // Test: pull returns Vec of SyncResult entries for batch commit
    #[tokio::test]
    async fn pull_returns_sync_results_for_batch_commit() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());
        StateDb::open(&db_path).expect("open db");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                make_file_meta("page1.md", 1700000000000i64)
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/page1.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("content"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.results.len(), 1, "should have 1 SyncResult");
        match &result.results[0] {
            SyncResult::Synced { path, .. } => assert_eq!(path, "page1.md"),
            other => panic!("expected Synced result, got: {other:?}"),
        }
    }

    // Test: pull validates server-returned file names (rejects path traversal)
    #[tokio::test]
    async fn pull_rejects_path_traversal_from_server() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());
        StateDb::open(&db_path).expect("open db");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                // These dangerous paths should be rejected
                make_file_meta("../etc/passwd", 1700000000000i64),
                make_file_meta("/etc/shadow", 1700000000000i64),
                // This safe path should be processed
                make_file_meta("safe-page.md", 1700000000000i64),
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/safe-page.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("safe content"))
            .mount(&server)
            .await;

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            4,
            false,
        )
        .await
        .expect("pull should succeed");

        // Only safe-page.md should be downloaded
        assert_eq!(result.downloaded, 1, "only safe file should be downloaded");

        // Dangerous paths should not be created
        assert!(
            !dir.path().join("../etc/passwd").exists(),
            "path traversal should be rejected"
        );
        assert!(
            !dir.path().join("etc/passwd").exists(),
            "path traversal should not create file"
        );
    }

    // Test: pull respects FileFilter (skips excluded files from server listing)
    #[tokio::test]
    async fn pull_respects_file_filter() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());
        StateDb::open(&db_path).expect("open db");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                make_file_meta("_plug/core.js", 1700000000000i64),
                make_file_meta("allowed.md", 1700000000000i64),
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs/allowed.md"))
            .respond_with(ResponseTemplate::new(200).set_body_string("allowed content"))
            .mount(&server)
            .await;

        // Filter that excludes _plug/*
        let filter = FileFilter::new(&["_plug/*".to_string()], &[], false).expect("filter");
        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        let result = pull(&client, dir.path(), &sb_dir, &db_path, &filter, 4, false)
            .await
            .expect("pull should succeed");

        assert_eq!(
            result.downloaded, 1,
            "only non-filtered file should be downloaded"
        );
        assert!(
            !dir.path().join("_plug/core.js").exists(),
            "_plug file should not be downloaded"
        );
        assert!(
            dir.path().join("allowed.md").exists(),
            "allowed file should be downloaded"
        );
    }

    // Test: pull uses concurrent workers (Semaphore limits simultaneous downloads)
    #[tokio::test]
    async fn pull_uses_concurrent_workers_with_semaphore() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = setup_db(dir.path());
        StateDb::open(&db_path).expect("open db");

        // Create 3 files to download
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/.fs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                make_file_meta("page1.md", 1700000001000i64),
                make_file_meta("page2.md", 1700000002000i64),
                make_file_meta("page3.md", 1700000003000i64),
            ])))
            .mount(&server)
            .await;
        for page in &["page1.md", "page2.md", "page3.md"] {
            Mock::given(method("GET"))
                .and(wm_path(&format!("/.fs/{page}")))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(format!("content of {page}")),
                )
                .mount(&server)
                .await;
        }

        let client = make_client(&server.uri());
        let sb_dir = dir.path().join(".sb");
        // Use workers=2 to limit concurrency
        let result = pull(
            &client,
            dir.path(),
            &sb_dir,
            &db_path,
            &make_filter(),
            2,
            false,
        )
        .await
        .expect("pull should succeed");

        assert_eq!(result.downloaded, 3, "all 3 files should be downloaded");
        for page in &["page1.md", "page2.md", "page3.md"] {
            assert!(
                dir.path().join(page).exists(),
                "{page} should exist after pull"
            );
        }
    }
}
