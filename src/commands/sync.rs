use std::collections::HashMap;

use crate::cli::OutputFormat;
use crate::commands::page::find_space_root;
use crate::commands::server::build_client;
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};
use crate::sync::db::StateDb;
use crate::sync::scanner::{FileFilter, LocalScanner};
use crate::sync::{puller, pusher, SyncAction, SyncStatus};

/// Shared setup state for sync commands.
struct SyncContext {
    #[allow(dead_code)]
    space_root: std::path::PathBuf,
    sb_dir: std::path::PathBuf,
    db_path: std::path::PathBuf,
    content_dir: std::path::PathBuf,
    config: ResolvedConfig,
    filter: FileFilter,
    client: Option<crate::client::SbClient>,
}

impl SyncContext {
    /// Build a full context including an HTTP client.
    fn new(cli_token: Option<&str>) -> SbResult<Self> {
        let space_root = find_space_root()?;
        let config = ResolvedConfig::load_from(&space_root)?;
        let client = build_client(cli_token)?;
        let sb_dir = space_root.join(".sb");
        let db_path = sb_dir.join("state.db");
        let content_dir = space_root.join(&config.sync_dir.value);
        std::fs::create_dir_all(&content_dir).map_err(|e| SbError::Filesystem {
            message: format!(
                "failed to create sync directory '{}'",
                config.sync_dir.value
            ),
            path: content_dir.display().to_string(),
            source: Some(e),
        })?;
        let filter = FileFilter::new(
            &config.sync_exclude.value,
            &config.sync_include.value,
            config.sync_attachments.value,
        )?;
        Ok(Self {
            space_root,
            sb_dir,
            db_path,
            content_dir,
            config,
            filter,
            client: Some(client),
        })
    }

    /// Build a context without an HTTP client (for commands that only read local state).
    fn new_no_client() -> SbResult<Self> {
        let space_root = find_space_root()?;
        let config = ResolvedConfig::load_from(&space_root)?;
        let sb_dir = space_root.join(".sb");
        let db_path = sb_dir.join("state.db");
        let content_dir = space_root.join(&config.sync_dir.value);
        std::fs::create_dir_all(&content_dir).map_err(|e| SbError::Filesystem {
            message: format!(
                "failed to create sync directory '{}'",
                config.sync_dir.value
            ),
            path: content_dir.display().to_string(),
            source: Some(e),
        })?;
        let filter = FileFilter::new(
            &config.sync_exclude.value,
            &config.sync_include.value,
            config.sync_attachments.value,
        )?;
        Ok(Self {
            space_root,
            sb_dir,
            db_path,
            content_dir,
            config,
            filter,
            client: None,
        })
    }

    /// Unwrap the inner client, panicking if this context was built without one.
    fn client(&self) -> &crate::client::SbClient {
        self.client
            .as_ref()
            .expect("SyncContext::client() called on a no-client context")
    }
}

/// Commit a batch of sync results to state.db and update the last_sync timestamp.
async fn commit_sync_results(
    db_path: &std::path::Path,
    results: Vec<crate::sync::SyncResult>,
) -> SbResult<()> {
    let db_path = db_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> SbResult<()> {
        let mut db = StateDb::open(&db_path)?;
        db.commit_batch(&results)?;
        // Update last_sync timestamp
        let now = jiff::Zoned::now().to_string();
        db.set_meta("last_sync", &now)?;
        Ok(())
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("state.db commit task panicked: {e}"),
    })?
}

/// Pull changes from the server into the local space.
///
/// When `dry_run` is true, calls `plan_pull` to compute actions and prints
/// them without executing any file I/O or updating state.db.
pub async fn execute_pull(
    cli_token: Option<&str>,
    quiet: bool,
    format: &OutputFormat,
    dry_run: bool,
) -> SbResult<()> {
    let ctx = SyncContext::new(cli_token)?;

    if dry_run {
        let actions = puller::plan_pull(
            ctx.client(),
            &ctx.content_dir,
            &ctx.sb_dir,
            &ctx.db_path,
            &ctx.filter,
        )
        .await?;
        return format_dry_run_output(&actions, format, quiet);
    }

    let workers = ctx.config.sync_workers.value;
    let show_progress = !quiet && crate::output::is_tty();

    let result = puller::pull(
        ctx.client(),
        &ctx.content_dir,
        &ctx.sb_dir,
        &ctx.db_path,
        &ctx.filter,
        workers,
        show_progress,
    )
    .await?;

    // Commit results to state.db atomically
    commit_sync_results(&ctx.db_path, result.results).await?;

    if !quiet {
        eprintln!(
            "Pull complete: {} downloaded, {} conflicts, {} removed",
            result.downloaded, result.conflicts, result.deleted
        );
        if result.conflicts > 0 {
            eprintln!("Run `sb sync conflicts` to see conflicting files");
        }
    }

    Ok(())
}

/// Push local changes to the server.
///
/// When `dry_run` is true, calls `plan_push` to compute actions and prints
/// them without executing any file I/O or updating state.db.
pub async fn execute_push(
    cli_token: Option<&str>,
    quiet: bool,
    format: &OutputFormat,
    dry_run: bool,
) -> SbResult<()> {
    let ctx = SyncContext::new(cli_token)?;

    if dry_run {
        let actions = pusher::plan_push(
            ctx.client(),
            &ctx.content_dir,
            &ctx.sb_dir,
            &ctx.db_path,
            &ctx.filter,
        )
        .await?;
        return format_dry_run_output(&actions, format, quiet);
    }

    let workers = ctx.config.sync_workers.value;
    let show_progress = !quiet && crate::output::is_tty();

    let result = pusher::push(
        ctx.client(),
        &ctx.content_dir,
        &ctx.sb_dir,
        &ctx.db_path,
        &ctx.filter,
        workers,
        show_progress,
    )
    .await?;

    // Commit results to state.db atomically
    commit_sync_results(&ctx.db_path, result.results).await?;

    if !quiet {
        eprintln!(
            "Push complete: {} uploaded, {} conflicts, {} deleted",
            result.uploaded, result.conflicts, result.deleted
        );
        if result.conflicts > 0 {
            eprintln!("Run `sb sync conflicts` to see conflicting files");
        }
    }

    Ok(())
}

/// Run pull then push sequentially.
pub async fn execute_sync(
    cli_token: Option<&str>,
    quiet: bool,
    format: &OutputFormat,
) -> SbResult<()> {
    execute_pull(cli_token, quiet, format, false).await?;
    execute_push(cli_token, quiet, format, false).await?;
    Ok(())
}

/// Run dry-run for both pull and push, combining results.
pub async fn execute_sync_dry_run(
    cli_token: Option<&str>,
    quiet: bool,
    format: &OutputFormat,
) -> SbResult<()> {
    let ctx = SyncContext::new(cli_token)?;

    let mut actions = puller::plan_pull(
        ctx.client(),
        &ctx.content_dir,
        &ctx.sb_dir,
        &ctx.db_path,
        &ctx.filter,
    )
    .await?;
    let push_actions = pusher::plan_push(
        ctx.client(),
        &ctx.content_dir,
        &ctx.sb_dir,
        &ctx.db_path,
        &ctx.filter,
    )
    .await?;
    actions.extend(push_actions);

    format_dry_run_output(&actions, format, quiet)
}

/// Format and print dry-run actions to stdout.
///
/// Human format: table with Action | Path | Reason columns.
/// JSON format: array of {action, path, reason} objects.
fn format_dry_run_output(
    actions: &[SyncAction],
    format: &OutputFormat,
    quiet: bool,
) -> SbResult<()> {
    if actions.is_empty() {
        if !quiet {
            match format {
                OutputFormat::Json => println!("[]"),
                OutputFormat::Human => println!("Nothing to sync"),
            }
        }
        return Ok(());
    }

    match format {
        OutputFormat::Json => {
            let entries: Vec<serde_json::Value> = actions
                .iter()
                .map(|a| {
                    let (action, path, reason) = sync_action_parts(a);
                    serde_json::json!({ "action": action, "path": path, "reason": reason })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&entries).unwrap());
        }
        OutputFormat::Human => {
            println!("{:<14} {:<50} Reason", "Action", "Path");
            println!("{}", "-".repeat(80));
            for a in actions {
                let (action, path, reason) = sync_action_parts(a);
                println!("{:<14} {:<50} {}", action, path, reason);
            }
            if !quiet {
                let total = actions.len();
                eprintln!("\n{total} action(s) would be performed");
            }
        }
    }
    Ok(())
}

/// Extract (action_name, path, reason) string parts from a SyncAction for display.
fn sync_action_parts(action: &SyncAction) -> (&'static str, &str, &str) {
    match action {
        SyncAction::Download { path, reason, .. } => ("download", path.as_str(), reason.as_str()),
        SyncAction::Upload { path, reason } => ("upload", path.as_str(), reason.as_str()),
        SyncAction::DeleteLocal { path, reason } => {
            ("delete_local", path.as_str(), reason.as_str())
        }
        SyncAction::DeleteRemote { path, reason } => {
            ("delete_remote", path.as_str(), reason.as_str())
        }
        SyncAction::Conflict { path, reason } => ("conflict", path.as_str(), reason.as_str()),
        SyncAction::Skip { path, reason } => ("skip", path.as_str(), reason.as_str()),
    }
}

/// Show sync status: counts of modified, new, deleted, conflict files.
pub async fn execute_status(format: &OutputFormat) -> SbResult<()> {
    let ctx = SyncContext::new_no_client()?;

    let excludes = ctx.config.sync_exclude.value.clone();
    let includes = ctx.config.sync_include.value.clone();

    // Open state.db and scan local files concurrently via spawn_blocking
    let db_path_owned = ctx.db_path.clone();
    let content_dir_owned = ctx.content_dir.clone();

    let (rows, last_sync) = tokio::task::spawn_blocking(move || -> SbResult<_> {
        let db = StateDb::open(&db_path_owned)?;
        let rows = db.get_all_rows()?;
        let last_sync = db.get_meta("last_sync")?;
        Ok((rows, last_sync))
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("state.db read task panicked: {e}"),
    })??;

    // Scan local files
    let (ex, inc) = (excludes, includes);
    let attachments = ctx.config.sync_attachments.value;
    let local_files = tokio::task::spawn_blocking(move || -> SbResult<_> {
        let filter = FileFilter::new(&ex, &inc, attachments)?;
        let scanner = LocalScanner::new(filter);
        scanner.scan(&content_dir_owned)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("local scan task panicked: {e}"),
    })??;

    // Build local file map for comparison
    let local_map: HashMap<String, &crate::sync::scanner::LocalFileInfo> = local_files
        .iter()
        .map(|f| (f.rel_path.clone(), f))
        .collect();

    // Build state.db map
    let state_map: HashMap<String, &crate::sync::SyncStateRow> =
        rows.iter().map(|r| (r.path.clone(), r)).collect();

    // Compute counts
    let mut modified_count = 0usize;
    let mut new_count = 0usize;
    let mut deleted_count = 0usize;
    let conflict_count = rows
        .iter()
        .filter(|r| r.status == SyncStatus::Conflict)
        .count();

    for (path, local_file) in &local_map {
        match state_map.get(path) {
            None => {
                new_count += 1;
            }
            Some(row) => {
                // Modified if local hash differs from tracked local_hash
                if row.local_hash.as_deref() != Some(local_file.hash.as_str()) {
                    modified_count += 1;
                }
            }
        }
    }

    // Deleted: synced rows with no corresponding local file
    for (path, row) in &state_map {
        if row.status == SyncStatus::Synced && !local_map.contains_key(path) {
            deleted_count += 1;
        }
    }

    let last_sync_display = last_sync.as_deref().unwrap_or("never");

    match format {
        OutputFormat::Json => {
            let json = serde_json::json!({
                "modified": modified_count,
                "new": new_count,
                "deleted": deleted_count,
                "conflicts": conflict_count,
                "last_sync": last_sync_display,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        OutputFormat::Human => {
            println!("Status          Count");
            println!("---------------------");
            println!("Modified        {modified_count}");
            println!("New             {new_count}");
            println!("Deleted         {deleted_count}");
            println!("Conflicts       {conflict_count}");
            println!("---------------------");
            println!("Last sync: {last_sync_display}");
        }
    }

    Ok(())
}

/// Find the most recent conflict stash file for a given path.
///
/// Scans .sb/conflicts/ for files matching the path's stem + timestamp pattern.
/// Returns the most recent by filesystem mtime if multiple exist.
fn find_stash_file(sb_dir: &std::path::Path, original_path: &str) -> SbResult<std::path::PathBuf> {
    let p = std::path::Path::new(original_path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let ext = p.extension().and_then(|s| s.to_str());
    let parent = p.parent().map(|pp| pp.to_str().unwrap_or("")).unwrap_or("");

    let conflicts_subdir = if parent.is_empty() {
        sb_dir.join("conflicts")
    } else {
        sb_dir.join("conflicts").join(parent)
    };

    let prefix = format!("{stem}.");
    let suffix = ext.map(|e| format!(".{e}"));

    if !conflicts_subdir.exists() {
        return Err(SbError::Filesystem {
            message: format!(
                "no stash file found for '{}': conflicts directory does not exist",
                original_path
            ),
            path: conflicts_subdir.display().to_string(),
            source: None,
        });
    }

    let mut matches: Vec<std::path::PathBuf> = std::fs::read_dir(&conflicts_subdir)
        .map_err(|e| SbError::Filesystem {
            message: "failed to read conflicts directory".to_string(),
            path: conflicts_subdir.display().to_string(),
            source: Some(e),
        })?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let matches_prefix = name.starts_with(&prefix);
            let matches_suffix = suffix.as_ref().is_none_or(|s| name.ends_with(s.as_str()));
            // Exclude the original filename itself (stem.ext without timestamp)
            let original_name = ext
                .map(|e| format!("{stem}.{e}"))
                .unwrap_or_else(|| stem.to_string());
            matches_prefix && matches_suffix && name != original_name
        })
        .collect();

    if matches.is_empty() {
        return Err(SbError::Filesystem {
            message: format!(
                "no stash file found for '{}' in {}",
                original_path,
                conflicts_subdir.display()
            ),
            path: conflicts_subdir.display().to_string(),
            source: None,
        });
    }

    // Sort by mtime, pick most recent
    matches.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });

    if matches.len() > 1 {
        eprintln!(
            "Warning: found {} stash files for '{}'; resolving the most recent",
            matches.len(),
            original_path
        );
    }

    Ok(matches.pop().unwrap())
}

/// Compute file hash and mtime in a blocking task.
async fn compute_hash_and_mtime(path: std::path::PathBuf) -> SbResult<(String, i64)> {
    use crate::sync::scanner::{hash_file, mtime_ms};
    let path_display = path.display().to_string();
    tokio::task::spawn_blocking(move || -> SbResult<_> {
        let hash = hash_file(&path)?;
        let mtime = std::fs::metadata(&path).map(|m| mtime_ms(&m)).unwrap_or(0);
        Ok((hash, mtime))
    })
    .await
    .map_err(|e| SbError::Filesystem {
        message: format!("hash task panicked: {e}"),
        path: path_display,
        source: None,
    })?
}

/// Resolve a sync conflict.
#[allow(clippy::too_many_arguments)]
pub async fn execute_resolve(
    cli_token: Option<&str>,
    path: &str,
    keep_local: bool,
    keep_remote: bool,
    show_diff: bool,
    force: bool,
    quiet: bool,
    format: &OutputFormat,
) -> SbResult<()> {
    let _ = format; // format not used for resolve output (messages go to stderr)

    let space_root = find_space_root()?;
    let config = ResolvedConfig::load_from(&space_root)?;
    let sb_dir = space_root.join(".sb");
    let db_path = sb_dir.join("state.db");
    let content_dir = space_root.join(&config.sync_dir.value);

    // Validate path — reject path traversal using component-based check
    for component in std::path::Path::new(path).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(SbError::Usage(format!(
                "invalid path: '{path}' -- must not contain '..' components"
            )));
        }
        if matches!(
            component,
            std::path::Component::RootDir | std::path::Component::Prefix(_)
        ) {
            return Err(SbError::Usage(format!(
                "invalid path: '{path}' -- must be a relative path"
            )));
        }
    }

    let local_file = content_dir.join(path);
    let stash_file = find_stash_file(&sb_dir, path)?;

    // Verify the file is actually in conflict status in state.db
    let db_path_check = db_path.clone();
    let path_owned = path.to_string();
    let row = tokio::task::spawn_blocking(move || -> SbResult<_> {
        let db = StateDb::open(&db_path_check)?;
        db.get_row(&path_owned)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("state.db task panicked: {e}"),
    })??;

    match &row {
        None => {
            return Err(SbError::Filesystem {
                message: format!("'{}' is not tracked in state.db", path),
                path: "state.db".to_string(),
                source: None,
            });
        }
        Some(r) if r.status != SyncStatus::Conflict => {
            return Err(SbError::Filesystem {
                message: format!(
                    "'{}' is not in conflict (status: {})",
                    path,
                    r.status.as_str()
                ),
                path: "state.db".to_string(),
                source: None,
            });
        }
        _ => {}
    }

    // Handle --diff: show diff and exit without modifying anything
    if show_diff {
        let diff_tool = std::env::var("DIFF_TOOL").unwrap_or_else(|_| "diff".to_string());
        let local_abs = local_file
            .canonicalize()
            .unwrap_or_else(|_| local_file.clone());
        let stash_abs = stash_file
            .canonicalize()
            .unwrap_or_else(|_| stash_file.clone());

        let mut cmd = std::process::Command::new(&diff_tool);
        if diff_tool == "diff" {
            // Only pass -u for system diff; $DIFF_TOOL may use different flags
            cmd.arg("-u");
        }
        cmd.arg(&local_abs).arg(&stash_abs);

        let status = cmd.status().map_err(|e| SbError::Filesystem {
            message: format!("failed to spawn diff tool '{diff_tool}'"),
            path: diff_tool.clone(),
            source: Some(e),
        })?;
        // diff exits 0 = identical, 1 = different (normal), 2 = error
        if status.code() == Some(2) {
            return Err(SbError::Filesystem {
                message: format!("diff tool '{diff_tool}' reported an error"),
                path: diff_tool,
                source: None,
            });
        }
        return Ok(());
    }

    // Handle interactive mode: no --keep-local, no --keep-remote
    let resolved_keep_local = if keep_local {
        true
    } else if keep_remote {
        false
    } else if force {
        // --force without --keep defaults to keep local
        true
    } else {
        // Interactive: show conflict info and prompt
        let local_size = std::fs::metadata(&local_file).map(|m| m.len()).unwrap_or(0);
        let stash_size = std::fs::metadata(&stash_file).map(|m| m.len()).unwrap_or(0);
        eprintln!("Conflict: {path}");
        eprintln!("  Local:  {} bytes", local_size);
        eprintln!("  Remote: {} bytes (stashed)", stash_size);
        eprintln!();
        eprintln!("Options:");
        eprintln!("  l = keep local (upload to server)");
        eprintln!("  r = keep remote (overwrite local)");
        eprintln!("  d = show diff");
        eprintln!("  q = quit without resolving");

        loop {
            eprint!("Choice [l/r/d/q]: ");
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .map_err(|e| SbError::Filesystem {
                    message: "failed to read input".into(),
                    path: String::new(),
                    source: Some(e),
                })?;
            match input.trim().to_lowercase().as_str() {
                "l" => break true,
                "r" => break false,
                "d" => {
                    // Show diff inline
                    let diff_tool =
                        std::env::var("DIFF_TOOL").unwrap_or_else(|_| "diff".to_string());
                    let mut cmd = std::process::Command::new(&diff_tool);
                    if diff_tool == "diff" {
                        cmd.arg("-u");
                    }
                    cmd.arg(&local_file).arg(&stash_file);
                    let _ = cmd.status();
                    // Continue the loop — let them choose after viewing diff
                }
                "q" => {
                    if !quiet {
                        eprintln!("Conflict not resolved");
                    }
                    return Ok(());
                }
                _ => {
                    eprintln!("Invalid choice. Enter l, r, d, or q.");
                }
            }
        }
    };

    if resolved_keep_local {
        // Keep local — upload local file to server, remove stash
        let client = build_client(cli_token)?;
        let content = tokio::fs::read(&local_file)
            .await
            .map_err(|e| SbError::Filesystem {
                message: "failed to read local file for upload".into(),
                path: local_file.display().to_string(),
                source: Some(e),
            })?;

        client.put_file(path, bytes::Bytes::from(content)).await?;

        // Get new remote_mtime after upload
        let new_remote_mtime = client.get_file_meta(path).await.unwrap_or(0);

        let (local_hash, local_mtime) = compute_hash_and_mtime(local_file.clone()).await?;

        // Update state.db: mark_resolved inside spawn_blocking
        let db_path_owned = db_path.clone();
        let path_owned = path.to_string();
        let lh = local_hash.clone();
        let rh = local_hash.clone(); // after upload, remote content matches local
        tokio::task::spawn_blocking(move || -> SbResult<()> {
            let mut db = StateDb::open(&db_path_owned)?;
            db.mark_resolved(&path_owned, &lh, &rh, new_remote_mtime, local_mtime)?;
            Ok(())
        })
        .await
        .map_err(|e| SbError::Internal {
            message: format!("state.db update task panicked: {e}"),
        })??;

        // Delete stash file (Pitfall 4: outside the DB transaction)
        if let Err(e) = tokio::fs::remove_file(&stash_file).await {
            eprintln!(
                "Warning: failed to remove stash file {}: {e}",
                stash_file.display()
            );
        }

        // Clean up empty parent dirs in .sb/conflicts/
        if let Some(parent) = stash_file.parent() {
            let _ = std::fs::remove_dir(parent); // only succeeds if empty
        }

        if !quiet {
            eprintln!(
                "Resolved '{}': kept local version (uploaded to server)",
                path
            );
        }
    } else {
        // Keep remote — overwrite local with stash, remove stash
        tokio::fs::copy(&stash_file, &local_file)
            .await
            .map_err(|e| SbError::Filesystem {
                message: "failed to overwrite local file with stash".into(),
                path: local_file.display().to_string(),
                source: Some(e),
            })?;

        let (local_hash, local_mtime) = compute_hash_and_mtime(local_file.clone()).await?;

        // For keep-remote, the remote_mtime in state.db should be the existing row's remote_mtime
        // (since we didn't change the server). Use the row we already loaded.
        let existing_remote_mtime = row.as_ref().map(|r| r.remote_mtime).unwrap_or(0);

        // Update state.db
        let db_path_owned = db_path.clone();
        let path_owned = path.to_string();
        let lh = local_hash.clone();
        let rh = local_hash.clone(); // local now matches what was the remote (stash content)
        tokio::task::spawn_blocking(move || -> SbResult<()> {
            let mut db = StateDb::open(&db_path_owned)?;
            db.mark_resolved(&path_owned, &lh, &rh, existing_remote_mtime, local_mtime)?;
            Ok(())
        })
        .await
        .map_err(|e| SbError::Internal {
            message: format!("state.db update task panicked: {e}"),
        })??;

        // Delete stash file
        if let Err(e) = tokio::fs::remove_file(&stash_file).await {
            eprintln!(
                "Warning: failed to remove stash file {}: {e}",
                stash_file.display()
            );
        }

        if let Some(parent) = stash_file.parent() {
            let _ = std::fs::remove_dir(parent);
        }

        if !quiet {
            eprintln!(
                "Resolved '{}': kept remote version (local overwritten)",
                path
            );
        }
    }

    Ok(())
}

/// List files currently in conflict.
pub async fn execute_conflicts(format: &OutputFormat) -> SbResult<()> {
    let space_root = find_space_root()?;
    let sb_dir = space_root.join(".sb");
    let db_path = sb_dir.join("state.db");

    let db_path_owned = db_path.clone();
    let conflict_rows = tokio::task::spawn_blocking(move || -> SbResult<_> {
        let db = StateDb::open(&db_path_owned)?;
        db.get_rows_by_status(&SyncStatus::Conflict)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("state.db read task panicked: {e}"),
    })??;

    if conflict_rows.is_empty() {
        match format {
            OutputFormat::Json => {
                println!("[]");
            }
            OutputFormat::Human => {
                println!("No conflicts");
            }
        }
        return Ok(());
    }

    match format {
        OutputFormat::Json => {
            let entries: Vec<serde_json::Value> = conflict_rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "path": r.path,
                        "status": "conflict",
                        "conflict_at": r.conflict_at,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&entries).unwrap());
        }
        OutputFormat::Human => {
            for row in &conflict_rows {
                if row.conflict_at > 0 {
                    let ts = jiff::Timestamp::from_millisecond(row.conflict_at)
                        .map(|t| t.to_string())
                        .unwrap_or_else(|_| "unknown".to_string());
                    println!("{}  (detected: {})", row.path, ts);
                } else {
                    println!("{}", row.path);
                }
            }
        }
    }

    Ok(())
}
