use std::collections::HashMap;

use crate::cli::OutputFormat;
use crate::commands::page::find_space_root;
use crate::commands::server::build_client;
use crate::config::ResolvedConfig;
use crate::error::{SbError, SbResult};
use crate::sync::db::StateDb;
use crate::sync::scanner::{scan_marker_conflicts, FileFilter, LocalScanner, MarkerConflict};
use crate::sync::{puller, pusher, SyncAction, SyncStatus};

/// Resolve the sync content directory, refusing to point outside the space.
///
/// `Path::join` silently discards its base when the argument is absolute, so an
/// absolute `[sync] dir` (easy to end up with, since a global `~/.config/sb`
/// config applies to every space that does not override it) makes `content_dir`
/// a completely unrelated directory. That is how `sb sync resolve --all` run in
/// one space can read *that* space's state.db and overwrite files in another.
fn resolve_content_dir(
    space_root: &std::path::Path,
    config: &ResolvedConfig,
) -> SbResult<std::path::PathBuf> {
    let content_dir = space_root.join(&config.sync_dir.value);
    if !content_dir.starts_with(space_root) {
        return Err(SbError::Config {
            message: format!(
                "sync dir '{}' resolves to {}, outside the space at {}.\n\
                 An absolute `[sync] dir` overrides the space it is used from; \
                 set a relative dir, or run from the space it belongs to.",
                config.sync_dir.value,
                content_dir.display(),
                space_root.display()
            ),
        });
    }
    Ok(content_dir)
}

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
        let content_dir = resolve_content_dir(&space_root, &config)?;
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
        let content_dir = resolve_content_dir(&space_root, &config)?;
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

/// Pure predicate: should the marker-conflict hint be printed?
///
/// Factored out so the quiet-suppression rule is unit-testable directly.
fn should_print_marker_hint(marker_conflict_count: usize, quiet: bool) -> bool {
    marker_conflict_count > 0 && !quiet
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
    workers_override: Option<u32>,
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

    let workers = workers_override.unwrap_or(ctx.config.sync_workers.value);
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
    workers_override: Option<u32>,
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

    let workers = workers_override.unwrap_or(ctx.config.sync_workers.value);
    let show_progress = !quiet && crate::output::is_tty();

    let mut result = pusher::push(
        ctx.client(),
        &ctx.content_dir,
        &ctx.sb_dir,
        &ctx.db_path,
        &ctx.filter,
        workers,
        show_progress,
    )
    .await?;

    let mut failures = std::mem::take(&mut result.failures);
    let failed = failures.len();
    let succeeded = result.uploaded + result.conflicts + result.deleted + result.readonly;

    // Commit what succeeded FIRST. One file's failure must not cost us the work
    // of the files that did upload.
    commit_sync_results(&ctx.db_path, result.results).await?;

    for (path, err) in &failures {
        eprintln!("Failed to push '{path}': {err}");
    }

    if !quiet {
        eprintln!(
            "Push complete: {} uploaded, {} conflicts, {} deleted, {} read-only, {} failed",
            result.uploaded, result.conflicts, result.deleted, result.readonly, failed
        );
        if result.conflicts > 0 {
            eprintln!("Run `sb sync conflicts` to see conflicting files");
        }
        if result.readonly > 0 {
            eprintln!("Read-only paths were left as they are; edit one again to retry it");
        }
    }

    // Hand back a single reason whenever every failure shares one category, so
    // its exit code survives (an auth failure still exits 3 with its hint) even
    // when other files pushed fine. Gating that on "nothing succeeded" let one
    // unrelated sibling mask a 401 as a generic exit 1. Mixed causes stay a
    // general error, since no single code describes them; each was printed above.
    //
    // Sorted by path first: `JoinSet` completes in nondeterministic order, so
    // picking `failures[0]` off the raw vec made the reported reason a coin
    // flip between runs on identical input.
    failures.sort_by(|a, b| a.0.cmp(&b.0));
    match failed {
        0 => Ok(()),
        _ if all_same_category(&failures) => Err(failures.swap_remove(0).1),
        _ => Err(SbError::Config {
            message: format!("{failed} of {} files failed to push", failed + succeeded),
        }),
    }
}

/// True when every failure carries the same error category, so returning any
/// one of them reports the whole run's exit code honestly.
fn all_same_category(failures: &[(String, SbError)]) -> bool {
    let mut codes = failures.iter().map(|(_, e)| e.code_str());
    match codes.next() {
        Some(first) => codes.all(|c| c == first),
        None => false,
    }
}

/// Run pull then push sequentially.
pub async fn execute_sync(
    cli_token: Option<&str>,
    quiet: bool,
    format: &OutputFormat,
    workers_override: Option<u32>,
) -> SbResult<()> {
    execute_pull(cli_token, quiet, format, false, workers_override).await?;
    execute_push(cli_token, quiet, format, false, workers_override).await?;
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
///
/// The global `--quiet` flag suppresses the
/// human-readable hint printed when marker conflicts are found; the counts
/// themselves are always printed regardless.
pub async fn execute_status(format: &OutputFormat, quiet: bool) -> SbResult<()> {
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

    // Scan for server/editor-written conflict markers -- a DISTINCT kind of
    // conflict from `state.db`'s stash-based one above. This is a plain
    // content/filename scan, so it's counted separately and never touches
    // state.db or the .sb/conflicts/ stash.
    let content_dir_for_markers = ctx.content_dir.clone();
    let marker_conflicts = tokio::task::spawn_blocking(move || -> SbResult<Vec<MarkerConflict>> {
        scan_marker_conflicts(&content_dir_for_markers)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("marker-conflict scan task panicked: {e}"),
    })??;
    let marker_conflict_count = marker_conflicts.len();

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
    // A read-only row's local file matches its stored hash, so it is none of
    // modified/new/deleted and would otherwise be invisible here: the space
    // would look perfectly synced while a file the server refused exists only
    // on disk. The one stderr note during push is not enough on its own.
    let readonly_count = rows
        .iter()
        .filter(|r| r.status == SyncStatus::ReadOnly)
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
                "marker_conflicts": marker_conflict_count,
                "readonly": readonly_count,
                "last_sync": last_sync_display,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        OutputFormat::Human => {
            // Widths are explicit so every count lands in one column; the
            // longest label ("Marker conflicts") sets it.
            println!("{:<17}Count", "Status");
            println!("----------------------");
            println!("{:<17}{modified_count}", "Modified");
            println!("{:<17}{new_count}", "New");
            println!("{:<17}{deleted_count}", "Deleted");
            println!("{:<17}{conflict_count}", "Conflicts");
            println!("{:<17}{marker_conflict_count}", "Marker conflicts");
            println!("{:<17}{readonly_count}", "Read-only");
            println!("----------------------");
            println!("Last sync: {}", humanize_last_sync(last_sync_display));
        }
    }

    if readonly_count > 0 && !quiet {
        eprintln!(
            "{readonly_count} file(s) hold edits the server refused as read-only -- they exist only on disk; edit one again to retry it"
        );
    }

    if should_print_marker_hint(marker_conflict_count, quiet) {
        eprintln!(
            "{marker_conflict_count} file(s) contain unresolved conflict markers written by the server/editor merge -- run `sb sync conflicts` to see them"
        );
    }

    Ok(())
}

/// Every conflict stash for `original_path`, most recent first.
///
/// The caller resolves against `[0]` and then deletes the whole vec: the older
/// entries are superseded copies of the same server-side content, and leaving
/// them behind is what let one path accumulate 86 of them.
fn find_stash_files(
    sb_dir: &std::path::Path,
    original_path: &str,
    quiet: bool,
) -> SbResult<Vec<std::path::PathBuf>> {
    let conflicts_subdir = crate::sync::stash_dir_for(sb_dir, original_path);
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

    let mut matches = crate::sync::stash_files_for(sb_dir, original_path)?;
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

    matches.reverse(); // stash_files_for is oldest-first; resolve wants newest
    if matches.len() > 1 && !quiet {
        eprintln!(
            "Warning: found {} stash files for '{}'; resolving against the most recent and removing the rest",
            matches.len(),
            original_path
        );
    }
    Ok(matches)
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

/// What happened to one conflicted file during a resolve run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    KeptLocal,
    KeptRemote,
    Skipped,
    Diffed,
    Failed,
    /// User asked to stop; the caller abandons the rest of the walk.
    Quit,
}

impl Outcome {
    /// Stable machine-readable name for `--format json`.
    fn as_str(self) -> &'static str {
        match self {
            Outcome::KeptLocal => "kept_local",
            Outcome::KeptRemote => "kept_remote",
            Outcome::Skipped => "skipped",
            Outcome::Diffed => "diffed",
            Outcome::Failed => "failed",
            Outcome::Quit => "quit",
        }
    }
}

/// One answer to the interactive conflict prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Local,
    Remote,
    Diff,
    Skip,
    Quit,
}

/// Parse a reply to the `[l/r/d/s/q]` prompt. `None` means "reprompt".
fn parse_choice(input: &str) -> Option<Choice> {
    match input.trim().to_lowercase().as_str() {
        "l" => Some(Choice::Local),
        "r" => Some(Choice::Remote),
        "d" => Some(Choice::Diff),
        "s" => Some(Choice::Skip),
        "q" => Some(Choice::Quit),
        _ => None,
    }
}

/// Flags that apply uniformly to every file in a resolve run.
#[derive(Debug, Clone, Copy)]
struct ResolveOpts {
    keep_local: bool,
    keep_remote: bool,
    show_diff: bool,
    force: bool,
    quiet: bool,
}

/// Reject `..` and absolute paths before they are joined onto the space root.
///
/// Applies to every target, whichever way it arrived (positional arg, `--all`,
/// or the picker), so a poisoned `state.db` row is rejected too.
///
/// This is a lexical check, not a containment guarantee: a symlink inside the
/// space still resolves outside it, and `tokio::fs::copy` follows symlinks.
/// Sandboxing that would mean canonicalizing against `content_dir`, which would
/// also break spaces that legitimately symlink subdirectories in.
fn validate_relative_path(path: &str) -> SbResult<()> {
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
    Ok(())
}

/// Run `$DIFF_TOOL` (default `diff -u`) on the local file and its stash.
///
/// `diff` exits 0 when identical and 1 when the files differ; only 2 means the
/// tool itself failed, so that is the only status treated as an error.
fn spawn_diff(local: &std::path::Path, stash: &std::path::Path) -> SbResult<()> {
    let diff_tool = std::env::var("DIFF_TOOL").unwrap_or_else(|_| "diff".to_string());
    let mut cmd = std::process::Command::new(&diff_tool);
    if diff_tool == "diff" {
        // Only pass -u for system diff; $DIFF_TOOL may use different flags
        cmd.arg("-u");
    }
    cmd.arg(local).arg(stash);
    if crate::output::no_input() {
        // $DIFF_TOOL inherits stdin by default. vimdiff and friends block
        // forever reading a non-terminal stdin, which would hang the whole
        // walk; there is nobody to type into it in this mode anyway.
        cmd.stdin(std::process::Stdio::null());
    }
    let status = cmd.status().map_err(|e| SbError::Filesystem {
        message: format!("failed to spawn diff tool '{diff_tool}'"),
        path: diff_tool.clone(),
        source: Some(e),
    })?;
    if status.code() == Some(2) {
        return Err(SbError::Filesystem {
            message: format!("diff tool '{diff_tool}' reported an error"),
            path: diff_tool,
            source: None,
        });
    }
    Ok(())
}

/// Build the HTTP client on first use, so `--keep-remote` still works with no
/// auth token configured (it never touches the server).
fn client_for<'a>(
    slot: &'a mut Option<crate::client::SbClient>,
    cli_token: Option<&str>,
) -> SbResult<&'a crate::client::SbClient> {
    if slot.is_none() {
        *slot = Some(build_client(cli_token)?);
    }
    Ok(slot.as_ref().expect("client just built"))
}

/// Resolve a single conflicted file.
///
/// Everything here is per-file: the stash path, the `state.db` row, and the
/// hashes are all looked up fresh, so nothing can be hoisted and go stale across
/// a multi-file walk.
async fn resolve_one(
    ctx: &SyncContext,
    client_slot: &mut Option<crate::client::SbClient>,
    cli_token: Option<&str>,
    path: &str,
    opts: ResolveOpts,
    index: usize,
    total: usize,
) -> SbResult<Outcome> {
    validate_relative_path(path)?;

    let local_file = ctx.content_dir.join(path);
    let stash_files = find_stash_files(&ctx.sb_dir, path, opts.quiet)?;
    let stash_file = stash_files[0].clone();

    // Verify the file is actually in conflict status in state.db
    let db_path_check = ctx.db_path.clone();
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

    // Handle --diff: show diff and return without modifying anything
    if opts.show_diff {
        let local_abs = local_file
            .canonicalize()
            .unwrap_or_else(|_| local_file.clone());
        let stash_abs = stash_file
            .canonicalize()
            .unwrap_or_else(|_| stash_file.clone());
        spawn_diff(&local_abs, &stash_abs)?;
        return Ok(Outcome::Diffed);
    }

    // Decide local vs remote: explicit flag, --force default, else prompt.
    let resolved_keep_local = if opts.keep_local {
        true
    } else if opts.keep_remote {
        false
    } else if opts.force {
        // --force without --keep defaults to keep local
        true
    } else {
        // Interactive only. Without a terminal there is nobody to answer, so
        // fail with a usage error instead of blocking on stdin forever.
        if crate::output::no_input() {
            return Err(SbError::Usage(format!(
                "cannot prompt for '{path}' in non-interactive mode; pass --keep-local or --keep-remote"
            )));
        }
        let local_size = std::fs::metadata(&local_file).map(|m| m.len()).unwrap_or(0);
        let stash_size = std::fs::metadata(&stash_file).map(|m| m.len()).unwrap_or(0);
        if total > 1 {
            eprintln!("Conflict {index}/{total}: {path}");
        } else {
            eprintln!("Conflict: {path}");
        }
        eprintln!("  Local:  {} bytes", local_size);
        eprintln!("  Remote: {} bytes (stashed)", stash_size);
        eprintln!();
        eprintln!("Options:");
        eprintln!("  l = keep local (upload to server)");
        eprintln!("  r = keep remote (overwrite local)");
        eprintln!("  d = show diff");
        eprintln!("  s = skip this file");
        eprintln!("  q = quit without resolving");

        loop {
            eprint!("Choice [l/r/d/s/q]: ");
            use std::io::Write;
            std::io::stderr().flush().ok();
            let mut input = String::new();
            let n = std::io::stdin()
                .read_line(&mut input)
                .map_err(|e| SbError::Filesystem {
                    message: "failed to read input".into(),
                    path: String::new(),
                    source: Some(e),
                })?;
            if n == 0 {
                // stdin closed mid-walk: treat as quit rather than spinning.
                return Ok(Outcome::Quit);
            }
            match parse_choice(&input) {
                Some(Choice::Local) => break true,
                Some(Choice::Remote) => break false,
                Some(Choice::Diff) => {
                    let _ = spawn_diff(&local_file, &stash_file);
                    // Continue the loop — let them choose after viewing diff
                }
                Some(Choice::Skip) => {
                    if !opts.quiet {
                        eprintln!("Skipped '{path}'");
                    }
                    return Ok(Outcome::Skipped);
                }
                Some(Choice::Quit) => {
                    if !opts.quiet {
                        eprintln!("Conflict not resolved");
                    }
                    return Ok(Outcome::Quit);
                }
                None => {
                    eprintln!("Invalid choice. Enter l, r, d, s, or q.");
                }
            }
        }
    };

    if resolved_keep_local {
        // Keep local — upload local file to server, remove stash
        let client = client_for(client_slot, cli_token)?;
        let content = tokio::fs::read(&local_file)
            .await
            .map_err(|e| SbError::Filesystem {
                message: "failed to read local file for upload".into(),
                path: local_file.display().to_string(),
                source: Some(e),
            })?;

        // Unconditional: the whole point of --keep-local is to overwrite the
        // server copy we already stashed.
        let new_etag = client
            .put_file(path, bytes::Bytes::from(content), None)
            .await?;

        // Get new remote_mtime after upload
        let new_remote_mtime = client.get_file_meta(path).await.unwrap_or(0);

        let (local_hash, local_mtime) = compute_hash_and_mtime(local_file.clone()).await?;

        // Update state.db: mark_resolved inside spawn_blocking
        let db_path_owned = ctx.db_path.clone();
        let path_owned = path.to_string();
        let lh = local_hash.clone();
        let rh = local_hash.clone(); // after upload, remote content matches local
        tokio::task::spawn_blocking(move || -> SbResult<()> {
            let mut db = StateDb::open(&db_path_owned)?;
            db.mark_resolved(
                &path_owned,
                &lh,
                &rh,
                new_etag.as_deref(),
                new_remote_mtime,
                local_mtime,
            )?;
            Ok(())
        })
        .await
        .map_err(|e| SbError::Internal {
            message: format!("state.db update task panicked: {e}"),
        })??;

        // Delete stash files (Pitfall 4: outside the DB transaction)
        remove_stashes(&stash_files, opts.quiet).await;

        if !opts.quiet {
            eprintln!("Resolved '{path}': kept local version (uploaded to server)");
        }
        Ok(Outcome::KeptLocal)
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
        let db_path_owned = ctx.db_path.clone();
        let path_owned = path.to_string();
        let lh = local_hash.clone();
        let rh = local_hash.clone(); // local now matches what was the remote (stash content)
        tokio::task::spawn_blocking(move || -> SbResult<()> {
            let mut db = StateDb::open(&db_path_owned)?;
            // No ETag: we never fetched the server's current copy here, and a
            // stale one would make every later push 412.
            db.mark_resolved(
                &path_owned,
                &lh,
                &rh,
                None,
                existing_remote_mtime,
                local_mtime,
            )?;
            Ok(())
        })
        .await
        .map_err(|e| SbError::Internal {
            message: format!("state.db update task panicked: {e}"),
        })??;

        remove_stashes(&stash_files, opts.quiet).await;

        if !opts.quiet {
            eprintln!("Resolved '{path}': kept remote version (local overwritten)");
        }
        Ok(Outcome::KeptRemote)
    }
}

/// Delete every stash the resolution consumed, pruning the parent dir if that
/// leaves it empty.
///
/// The whole set goes, not just the one we diffed against: the conflict is over,
/// and the older stashes are superseded copies of the same server-side content.
///
/// Deliberately non-fatal: the DB already says "resolved", and a leftover stash
/// is recoverable noise, not lost data.
async fn remove_stashes(stash_files: &[std::path::PathBuf], quiet: bool) {
    for stash_file in stash_files {
        if let Err(e) = tokio::fs::remove_file(stash_file).await {
            if !quiet {
                eprintln!(
                    "Warning: failed to remove stash file {}: {e}",
                    stash_file.display()
                );
            }
        }
    }
    if let Some(parent) = stash_files.first().and_then(|p| p.parent()) {
        prune_empty_stash_dirs(parent);
    }
}

/// Delete `dir` and its now-childless ancestors, stopping at `.sb/conflicts/`.
///
/// `remove_dir` only succeeds on an empty directory, so this is self-limiting:
/// the first non-empty ancestor ends the walk. Without it a nested page leaves
/// behind the skeleton of its folders (`conflicts/Work/Career/` empties but
/// `conflicts/Work/` stays).
fn prune_empty_stash_dirs(dir: &std::path::Path) {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if !d.ends_with("conflicts") && std::fs::remove_dir(d).is_ok() {
            cur = d.parent();
        } else {
            break;
        }
    }
}

/// Read every path currently marked `conflict` in `state.db`.
async fn conflict_paths(db_path: &std::path::Path) -> SbResult<Vec<String>> {
    let db_path_owned = db_path.to_path_buf();
    let rows = tokio::task::spawn_blocking(move || -> SbResult<_> {
        let db = StateDb::open(&db_path_owned)?;
        db.get_rows_by_status(&SyncStatus::Conflict)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("state.db read task panicked: {e}"),
    })??;
    Ok(rows.into_iter().map(|r| r.path).collect())
}

/// Resolve one, some, or all sync conflicts.
///
/// With a `path`, resolves exactly that file (the original behaviour). With
/// `--all`, walks every conflicted file. With neither, opens a multi-select
/// picker over the live conflict list and walks whatever was chosen.
#[allow(clippy::too_many_arguments)]
pub async fn execute_resolve(
    cli_token: Option<&str>,
    path: Option<&str>,
    all: bool,
    keep_local: bool,
    keep_remote: bool,
    show_diff: bool,
    force: bool,
    quiet: bool,
    format: &OutputFormat,
) -> SbResult<()> {
    // No path and no --all means we must ask. Check before touching state.db so
    // a space with zero conflicts still reports the usage problem rather than
    // silently succeeding.
    if path.is_none() && !all && crate::output::no_input() {
        return Err(SbError::Usage(
            "no path given: pass a path, or --all with --keep-local/--keep-remote".to_string(),
        ));
    }

    let ctx = SyncContext::new_no_client()?;

    let targets: Vec<String> = match path {
        Some(p) => vec![p.to_string()],
        None => {
            let conflicts = conflict_paths(&ctx.db_path).await?;
            if conflicts.is_empty() {
                match format {
                    OutputFormat::Json => println!("[]"),
                    OutputFormat::Human => println!("No conflicts"),
                }
                return Ok(());
            }
            if all {
                conflicts
            } else {
                let picked = crate::commands::picker::pick_multi(&conflicts, "conflict").await?;
                if picked.is_empty() {
                    if !quiet {
                        eprintln!("Cancelled.");
                    }
                    return Ok(());
                }
                picked
            }
        }
    };

    // An explicit path is validated up front so a bad argument is reported on
    // its own terms rather than as whatever the walk hits first. Targets that
    // came from state.db are validated per-file inside resolve_one instead, so
    // one poisoned row is skipped and counted rather than killing the run.
    let explicit = path.is_some();
    if explicit {
        for target in &targets {
            validate_relative_path(target)?;
        }
    }

    let opts = ResolveOpts {
        keep_local,
        keep_remote,
        show_diff,
        force,
        quiet,
    };

    // Nothing here decides local-vs-remote for us, so every target would reach
    // the prompt. Fail once, up front, with a usage error rather than N times
    // per file (or, before this guard existed, blocking on stdin forever).
    if !(keep_local || keep_remote || force || show_diff) && crate::output::no_input() {
        return Err(SbError::Usage(
            "cannot prompt for a resolution in non-interactive mode; \
             pass --keep-local or --keep-remote"
                .to_string(),
        ));
    }

    let total = targets.len();
    let mut client_slot: Option<crate::client::SbClient> = None;
    let mut results: Vec<(String, Outcome)> = Vec::with_capacity(total);
    let mut first_error: Option<SbError> = None;
    let mut failures = 0usize;

    for (i, target) in targets.iter().enumerate() {
        match resolve_one(
            &ctx,
            &mut client_slot,
            cli_token,
            target,
            opts,
            i + 1,
            total,
        )
        .await
        {
            Ok(Outcome::Quit) => {
                results.push((target.clone(), Outcome::Quit));
                break;
            }
            Ok(outcome) => results.push((target.clone(), outcome)),
            Err(e) => {
                // An explicitly named target keeps the original contract: its
                // error is the command's error, category and exit code intact.
                // A walk must not let one bad file abandon the rest, so the
                // error is reported and counted instead. Keyed on how the
                // target was chosen, not on how many there happen to be, so
                // the contract does not change with the size of the conflict
                // list.
                if explicit {
                    return Err(e);
                }
                failures += 1;
                eprintln!("Failed to resolve '{target}': {e}");
                if first_error.is_none() {
                    first_error = Some(e);
                }
                results.push((target.clone(), Outcome::Failed));
            }
        }
    }

    // --diff writes the diff itself to stdout; appending JSON would corrupt it.
    if matches!(format, OutputFormat::Json) && !show_diff {
        let entries: Vec<serde_json::Value> = results
            .iter()
            .map(|(p, o)| serde_json::json!({ "path": p, "resolution": o.as_str() }))
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries).unwrap());
    }

    if total > 1 && !quiet {
        let resolved = results
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::KeptLocal | Outcome::KeptRemote))
            .count();
        let skipped = results
            .iter()
            .filter(|(_, o)| *o == Outcome::Skipped)
            .count();
        let mut parts = vec![format!("{resolved} resolved")];
        if skipped > 0 {
            parts.push(format!("{skipped} skipped"));
        }
        if failures > 0 {
            parts.push(format!("{failures} failed"));
        }
        eprintln!("{}", parts.join(", "));
    }

    match first_error {
        // Everything failed, and for one reason: hand back that reason intact
        // so its category survives (an auth failure still exits 3, with its
        // remediation hint) rather than being flattened to a generic error.
        Some(e) if failures == total => Err(e),
        Some(_) => Err(SbError::Config {
            // Reused for its bare-message rendering and general (exit 1)
            // category; the per-file causes were already printed above.
            message: format!("{failures} of {total} conflicts failed to resolve"),
        }),
        None => Ok(()),
    }
}
/// One stash file the prune pass decided to delete, and why.
struct Doomed {
    stash: std::path::PathBuf,
    origin: String,
    bytes: u64,
    reason: &'static str,
}

/// Every original path that has at least one stash under `.sb/conflicts/`.
fn stashed_paths(conflicts_root: &std::path::Path) -> Vec<String> {
    let mut paths: Vec<String> = walkdir::WalkDir::new(conflicts_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let rel = e.path().strip_prefix(conflicts_root).ok()?;
            crate::sync::stash_origin(&rel.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Decide which of `origin`'s stashes carry no information.
///
/// A stash byte-identical to the live local file says nothing the file does not
/// already say -- those are the spurious ones. Of whatever is left, byte-identical
/// stashes collapse to the newest, since the older copies hold the same server
/// content under an earlier timestamp.
///
/// A path still marked `conflict` keeps its newest stash regardless, so
/// `sb sync resolve` always has something to diff against; `--all` waives that
/// and clears the path out entirely.
fn plan_prune(
    sb_dir: &std::path::Path,
    content_dir: &std::path::Path,
    origin: &str,
    in_conflict: bool,
    all: bool,
) -> SbResult<Vec<Doomed>> {
    use crate::sync::scanner::hash_file;

    let stashes = crate::sync::stash_files_for(sb_dir, origin)?; // oldest first
    let local_hash = hash_file(&content_dir.join(origin)).ok();

    if all && !in_conflict {
        return Ok(stashes
            .into_iter()
            .map(|stash| Doomed {
                bytes: std::fs::metadata(&stash).map(|m| m.len()).unwrap_or(0),
                stash,
                origin: origin.to_string(),
                reason: "path is not in conflict",
            })
            .collect());
    }

    let mut doomed = Vec::new();
    let mut kept_hashes: Vec<String> = Vec::new();
    // Newest first, so the copy we keep for each distinct content is the newest.
    for stash in stashes.into_iter().rev() {
        let Ok(hash) = hash_file(&stash) else {
            continue;
        };
        let last_resort = in_conflict && !all && doomed.is_empty() && kept_hashes.is_empty();

        let reason = if local_hash.as_deref() == Some(hash.as_str()) && !last_resort {
            "identical to the local file"
        } else if kept_hashes.contains(&hash) {
            "duplicate of a newer stash"
        } else {
            kept_hashes.push(hash);
            continue;
        };

        doomed.push(Doomed {
            bytes: std::fs::metadata(&stash).map(|m| m.len()).unwrap_or(0),
            stash,
            origin: origin.to_string(),
            reason,
        });
    }
    Ok(doomed)
}

/// Remove conflict stashes that carry no information.
///
/// Exists because an unresolved conflict row re-conflicts on every sync, and
/// before the dedup in `write_conflict_stash` that meant a fresh byte-identical
/// stash per run -- 485 files and 33 MB, cleared by hand, in the incident that
/// prompted this command.
pub async fn execute_prune_stashes(
    path: Option<&str>,
    all: bool,
    dry_run: bool,
    format: &OutputFormat,
    quiet: bool,
) -> SbResult<()> {
    let ctx = SyncContext::new_no_client()?;
    let conflicts_root = ctx.sb_dir.join("conflicts");

    let origins = match path {
        Some(p) => {
            validate_relative_path(p)?;
            vec![p.to_string()]
        }
        None if conflicts_root.is_dir() => stashed_paths(&conflicts_root),
        None => Vec::new(),
    };

    let conflicted: std::collections::HashSet<String> =
        conflict_paths(&ctx.db_path).await?.into_iter().collect();

    let sb_dir = ctx.sb_dir.clone();
    let content_dir = ctx.content_dir.clone();
    let doomed = tokio::task::spawn_blocking(move || -> SbResult<Vec<Doomed>> {
        let mut doomed = Vec::new();
        for origin in &origins {
            doomed.extend(plan_prune(
                &sb_dir,
                &content_dir,
                origin,
                conflicted.contains(origin),
                all,
            )?);
        }
        Ok(doomed)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("prune planning task panicked: {e}"),
    })??;

    let mut removed = 0usize;
    let mut bytes = 0u64;
    let mut failures = 0usize;
    for d in &doomed {
        if dry_run {
            removed += 1;
            bytes += d.bytes;
            continue;
        }
        match tokio::fs::remove_file(&d.stash).await {
            Ok(()) => {
                removed += 1;
                bytes += d.bytes;
            }
            Err(e) => {
                failures += 1;
                if !quiet {
                    eprintln!("Warning: failed to remove stash {}: {e}", d.stash.display());
                }
            }
        }
    }
    if !dry_run {
        for d in &doomed {
            if let Some(parent) = d.stash.parent() {
                prune_empty_stash_dirs(parent);
            }
        }
    }

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": dry_run,
                    "pruned": removed,
                    "bytes": bytes,
                    "failed": failures,
                    "stashes": doomed.iter().map(|d| serde_json::json!({
                        "stash": d.stash.display().to_string(),
                        "path": d.origin,
                        "reason": d.reason,
                    })).collect::<Vec<_>>(),
                }))
                .unwrap()
            );
        }
        OutputFormat::Human => {
            if doomed.is_empty() {
                println!("No redundant conflict stashes");
            } else {
                for d in &doomed {
                    println!("{}  [{}]", d.stash.display(), d.reason);
                }
                let verb = if dry_run { "Would prune" } else { "Pruned" };
                println!("{verb} {removed} stash file(s), {bytes} bytes");
            }
            if !quiet && !all && !doomed.is_empty() {
                eprintln!("Pass --all to also drop stashes left over from paths that are no longer in conflict.");
            }
        }
    }

    Ok(())
}

/// List files currently in conflict.
///
/// Two DISTINCT kinds are reported, never conflated:
/// - "stash" conflicts: metadata/hash-driven, tracked in state.db and
///   resolved via `sb sync resolve` (stash lives under `.sb/conflicts/`).
/// - "marker" conflicts: a plain content/filename scan for conflict markers
///   the server or editor wrote directly into a file (or a
///   `.conflicted-<hash>.` sibling it dropped instead). REPORT ONLY -- sb
///   does not merge or resolve these; the editor already does that well.
///
/// `quiet` suppresses the human-readable hints (both the existing stash
/// "resolve interactively" hint and the new marker-conflict hint); the
/// listing itself (the data) is always printed regardless.
pub async fn execute_conflicts(format: &OutputFormat, quiet: bool) -> SbResult<()> {
    let ctx = SyncContext::new_no_client()?;

    let db_path_owned = ctx.db_path.clone();
    let conflict_rows = tokio::task::spawn_blocking(move || -> SbResult<_> {
        let db = StateDb::open(&db_path_owned)?;
        db.get_rows_by_status(&SyncStatus::Conflict)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("state.db read task panicked: {e}"),
    })??;

    let content_dir_owned = ctx.content_dir.clone();
    let marker_conflicts = tokio::task::spawn_blocking(move || -> SbResult<Vec<MarkerConflict>> {
        scan_marker_conflicts(&content_dir_owned)
    })
    .await
    .map_err(|e| SbError::Internal {
        message: format!("marker-conflict scan task panicked: {e}"),
    })??;

    if conflict_rows.is_empty() && marker_conflicts.is_empty() {
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
            let mut entries: Vec<serde_json::Value> = conflict_rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "path": r.path,
                        "kind": "stash_conflict",
                        "status": r.status.as_str(),
                        "conflict_at": r.conflict_at,
                    })
                })
                .collect();
            entries.extend(marker_conflicts.iter().map(|m| {
                serde_json::json!({
                    "path": m.rel_path,
                    "kind": "marker_conflict",
                    "marker_kind": m.kind.as_str(),
                })
            }));
            println!("{}", serde_json::to_string_pretty(&entries).unwrap());
        }
        OutputFormat::Human => {
            if !conflict_rows.is_empty() {
                println!("Stash conflicts (resolve with `sb sync resolve <path>`):");
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
            if !marker_conflicts.is_empty() {
                if !conflict_rows.is_empty() {
                    println!();
                }
                println!("Files with unresolved conflict markers (edit directly -- sb does not resolve these):");
                for m in &marker_conflicts {
                    println!("{}  [{}]", m.rel_path, m.kind.as_str());
                }
            }
            // This hint is specific to stash conflicts (`sb sync resolve`
            // doesn't know about marker conflicts at all), so only show it
            // when there's actually a stash conflict to resolve.
            if !conflict_rows.is_empty() && !quiet {
                eprintln!("Run `sb sync resolve` to resolve them interactively.");
            }
        }
    }

    if should_print_marker_hint(marker_conflicts.len(), quiet) {
        eprintln!(
            "{} file(s) contain conflict markers or unmergeable-conflict siblings that sb will not resolve for you -- open them in the editor",
            marker_conflicts.len()
        );
    }

    Ok(())
}

/// Render the stored last-sync stamp (a jiff `Zoned` string such as
/// `2026-09-26T01:42:10.4259-04:00[America/New_York]`) as
/// `2026-09-26 01:42 (3 min ago)` for humans. Anything unparseable,
/// including `never`, is shown as-is.
fn humanize_last_sync(raw: &str) -> String {
    let Ok(zoned) = raw.parse::<jiff::Zoned>() else {
        return raw.to_string();
    };
    let secs = jiff::Timestamp::now().as_second() - zoned.timestamp().as_second();
    let ago = match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3_600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3_600),
        s => format!("{} days ago", s / 86_400),
    };
    format!("{} ({ago})", zoned.strftime("%Y-%m-%d %H:%M"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::scanner::{hash_file, mtime_ms};
    use crate::test_util::{make_space, SbSpaceGuard};

    // --- humanize_last_sync ---

    #[test]
    fn humanize_last_sync_formats_a_zoned_stamp() {
        let s = humanize_last_sync("2020-01-02T03:04:05.123-05:00[America/New_York]");
        assert!(s.starts_with("2020-01-02 03:04 ("), "{s}");
        assert!(s.ends_with("days ago)"), "{s}");
    }

    #[test]
    fn humanize_last_sync_passes_through_never_and_garbage() {
        assert_eq!(humanize_last_sync("never"), "never");
        assert_eq!(humanize_last_sync("not a date"), "not a date");
    }

    /// A push where one file 401s and another is merely read-only must still
    /// exit 3. Counting the read-only file as a success used to take the
    /// "partial failure" branch and flatten the auth error to exit 1, so an
    /// unrelated sibling masked the one problem the user could actually act on.
    #[test]
    fn one_category_of_failure_survives_an_unrelated_sibling() {
        let failures = vec![(
            "a.md".to_string(),
            SbError::AuthFailed {
                url: "u".into(),
                status: 401,
            },
        )];
        assert!(all_same_category(&failures));
        assert_eq!(failures[0].1.exit_code(), 3);
    }

    #[test]
    fn mixed_failure_categories_are_not_collapsed_into_one() {
        let failures = vec![
            (
                "a.md".to_string(),
                SbError::AuthFailed {
                    url: "u".into(),
                    status: 401,
                },
            ),
            (
                "b.md".to_string(),
                SbError::HttpStatus {
                    status: 500,
                    url: "u".into(),
                    body: String::new(),
                },
            ),
        ];
        assert!(
            !all_same_category(&failures),
            "auth and server faults must not report as one code"
        );
    }

    /// Two failures of the same category report that category regardless of
    /// which task finished first.
    #[test]
    fn same_category_holds_across_several_failures() {
        let failures = vec![
            (
                "a.md".to_string(),
                SbError::AuthFailed {
                    url: "u".into(),
                    status: 401,
                },
            ),
            (
                "b.md".to_string(),
                SbError::AuthFailed {
                    url: "u".into(),
                    status: 403,
                },
            ),
        ];
        assert!(all_same_category(&failures));
    }

    #[test]
    fn no_failures_is_not_a_category() {
        assert!(!all_same_category(&[]));
    }

    // --- sync_action_parts ---

    #[test]
    fn sync_action_parts_maps_each_variant_to_action_name() {
        let cases: Vec<(SyncAction, &str)> = vec![
            (
                SyncAction::Download {
                    path: "a".into(),
                    reason: "r".into(),
                    remote_mtime: 0,
                },
                "download",
            ),
            (
                SyncAction::Upload {
                    path: "b".into(),
                    reason: "r".into(),
                },
                "upload",
            ),
            (
                SyncAction::DeleteLocal {
                    path: "c".into(),
                    reason: "r".into(),
                },
                "delete_local",
            ),
            (
                SyncAction::DeleteRemote {
                    path: "d".into(),
                    reason: "r".into(),
                },
                "delete_remote",
            ),
            (
                SyncAction::Conflict {
                    path: "e".into(),
                    reason: "r".into(),
                },
                "conflict",
            ),
            (
                SyncAction::Skip {
                    path: "f".into(),
                    reason: "r".into(),
                },
                "skip",
            ),
        ];
        for (action, expected) in cases {
            let (name, _path, _reason) = sync_action_parts(&action);
            assert_eq!(name, expected, "wrong name for {action:?}");
        }
    }

    // --- format_dry_run_output ---

    #[test]
    fn format_dry_run_empty_actions_human_renders_nothing_to_sync() {
        // Function writes to stdout — we can't capture it here, just ensure no error.
        format_dry_run_output(&[], &OutputFormat::Human, false).unwrap();
    }

    #[test]
    fn format_dry_run_empty_actions_json_renders_empty_array() {
        format_dry_run_output(&[], &OutputFormat::Json, false).unwrap();
    }

    #[test]
    fn format_dry_run_with_actions_succeeds_in_both_formats() {
        let actions = vec![
            SyncAction::Upload {
                path: "a.md".into(),
                reason: "new".into(),
            },
            SyncAction::Download {
                path: "b.md".into(),
                reason: "remote-new".into(),
                remote_mtime: 1,
            },
        ];
        format_dry_run_output(&actions, &OutputFormat::Human, false).unwrap();
        format_dry_run_output(&actions, &OutputFormat::Json, false).unwrap();
    }

    // --- find_stash_files ---

    /// The stash `resolve` resolves against. `find_stash_files` returns the whole
    /// set newest-first because the caller also deletes it, but which one is
    /// authoritative is still `[0]`.
    fn find_stash_file(
        sb_dir: &std::path::Path,
        path: &str,
        quiet: bool,
    ) -> SbResult<std::path::PathBuf> {
        find_stash_files(sb_dir, path, quiet).map(|mut v| v.remove(0))
    }

    #[test]
    fn find_stash_files_returns_every_stash_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let conflicts = tmp.path().join("conflicts");
        std::fs::create_dir_all(&conflicts).unwrap();
        for (name, body) in [
            ("Doc.20260101T000000.md", "oldest"),
            ("Doc.20260102T000000.md", "middle"),
            ("Doc.20260103T000000.md", "newest"),
        ] {
            std::fs::write(conflicts.join(name), body).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let got = find_stash_files(tmp.path(), "Doc.md", true).unwrap();
        let bodies: Vec<String> = got
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        assert_eq!(bodies, vec!["newest", "middle", "oldest"]);
    }

    #[tokio::test]
    async fn remove_stashes_clears_the_whole_set_and_its_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let conflicts = tmp.path().join("conflicts").join("Work");
        std::fs::create_dir_all(&conflicts).unwrap();
        for i in 0..3 {
            std::fs::write(conflicts.join(format!("Doc.2026010{i}T000000.md")), "same").unwrap();
        }
        let stashes = find_stash_files(tmp.path(), "Work/Doc.md", true).unwrap();
        assert_eq!(stashes.len(), 3);
        remove_stashes(&stashes, true).await;
        assert!(
            find_stash_files(tmp.path(), "Work/Doc.md", true).is_err(),
            "no stash should survive a resolution"
        );
        assert!(
            !conflicts.exists(),
            "the emptied directory should be pruned"
        );
    }

    #[test]
    fn prune_empty_stash_dirs_walks_up_but_keeps_the_conflicts_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("conflicts");
        let leaf = root.join("Work").join("Career");
        std::fs::create_dir_all(&leaf).unwrap();
        prune_empty_stash_dirs(&leaf);
        assert!(!leaf.exists());
        assert!(!root.join("Work").exists(), "empty ancestors should go too");
        assert!(root.exists(), "the conflicts root itself must survive");
    }

    #[test]
    fn prune_empty_stash_dirs_stops_at_a_directory_that_still_has_stashes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("conflicts");
        let sibling = root.join("Work").join("Other.20260101T000000.md");
        let leaf = root.join("Work").join("Career");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(&sibling, "keep me").unwrap();
        prune_empty_stash_dirs(&leaf);
        assert!(!leaf.exists());
        assert!(sibling.exists(), "a sibling's stash must not be disturbed");
    }

    #[test]
    fn find_stash_file_returns_error_when_conflicts_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_stash_file(tmp.path(), "Some.md", true).unwrap_err();
        match err {
            SbError::Filesystem { message, .. } => {
                assert!(
                    message.contains("conflicts directory does not exist"),
                    "{message}"
                )
            }
            other => panic!("expected Filesystem, got: {other:?}"),
        }
    }

    #[test]
    fn find_stash_file_returns_error_when_no_match_in_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("conflicts")).unwrap();
        let err = find_stash_file(tmp.path(), "Missing.md", true).unwrap_err();
        match err {
            SbError::Filesystem { message, .. } => assert!(message.contains("no stash file")),
            other => panic!("expected Filesystem, got: {other:?}"),
        }
    }

    #[test]
    fn find_stash_file_returns_only_match_when_one_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("conflicts")).unwrap();
        let stash = tmp.path().join("conflicts").join("Doc.20260101T000000.md");
        std::fs::write(&stash, "stash body").unwrap();
        let got = find_stash_file(tmp.path(), "Doc.md", true).unwrap();
        assert_eq!(got, stash);
    }

    #[test]
    fn find_stash_file_picks_most_recent_when_multiple_match() {
        let tmp = tempfile::tempdir().unwrap();
        let conflicts = tmp.path().join("conflicts");
        std::fs::create_dir_all(&conflicts).unwrap();
        let older = conflicts.join("Doc.20260101T000000.md");
        let newer = conflicts.join("Doc.20260102T000000.md");
        std::fs::write(&older, "older").unwrap();
        // Sleep ensures mtime differs on filesystems with second precision.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newer, "newer").unwrap();
        let got = find_stash_file(tmp.path(), "Doc.md", true).unwrap();
        // most-recent by mtime should win
        let body = std::fs::read_to_string(&got).unwrap();
        assert_eq!(body, "newer");
    }

    #[test]
    fn find_stash_file_searches_nested_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("conflicts").join("Journal");
        std::fs::create_dir_all(&nested).unwrap();
        let stash = nested.join("2026-01-01.20260102T000000.md");
        std::fs::write(&stash, "nested stash").unwrap();
        let got = find_stash_file(tmp.path(), "Journal/2026-01-01.md", true).unwrap();
        assert_eq!(got, stash);
    }

    #[test]
    fn find_stash_file_excludes_original_filename() {
        // A file with the same name as the original (no timestamp suffix) must not be returned.
        let tmp = tempfile::tempdir().unwrap();
        let conflicts = tmp.path().join("conflicts");
        std::fs::create_dir_all(&conflicts).unwrap();
        std::fs::write(conflicts.join("Doc.md"), "not a stash").unwrap();
        let err = find_stash_file(tmp.path(), "Doc.md", true).unwrap_err();
        assert!(matches!(err, SbError::Filesystem { .. }));
    }

    // --- plan_prune ---

    /// `n` stashes for `Doc.md`, all holding `body`, plus a local file holding
    /// `local`. Returns (sb_dir, content_dir).
    fn prune_fixture(
        tmp: &std::path::Path,
        n: usize,
        body: &str,
        local: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let sb_dir = tmp.join(".sb");
        let content_dir = tmp.join("space");
        std::fs::create_dir_all(sb_dir.join("conflicts")).unwrap();
        std::fs::create_dir_all(&content_dir).unwrap();
        std::fs::write(content_dir.join("Doc.md"), local).unwrap();
        for i in 0..n {
            std::fs::write(
                sb_dir
                    .join("conflicts")
                    .join(format!("Doc.202601{:02}T000000.md", i + 1)),
                body,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        (sb_dir, content_dir)
    }

    #[test]
    fn plan_prune_drops_every_stash_identical_to_the_local_file() {
        // The Okta case: 86 stashes byte-identical to each other and to the live
        // file, so none of them records a real conflict.
        let tmp = tempfile::tempdir().unwrap();
        let (sb, content) = prune_fixture(tmp.path(), 5, "same bytes", "same bytes");
        let doomed = plan_prune(&sb, &content, "Doc.md", false, false).unwrap();
        assert_eq!(doomed.len(), 5);
        assert!(doomed
            .iter()
            .all(|d| d.reason == "identical to the local file"));
    }

    #[test]
    fn plan_prune_keeps_the_newest_stash_while_the_path_is_still_in_conflict() {
        // Dropping the last one would leave `sb sync resolve` with nothing to
        // diff against.
        let tmp = tempfile::tempdir().unwrap();
        let (sb, content) = prune_fixture(tmp.path(), 5, "same bytes", "same bytes");
        let doomed = plan_prune(&sb, &content, "Doc.md", true, false).unwrap();
        assert_eq!(doomed.len(), 4);
        let survivor = &crate::sync::stash_files_for(&sb, "Doc.md").unwrap()[4];
        assert!(!doomed.iter().any(|d| &d.stash == survivor));
    }

    #[test]
    fn plan_prune_with_all_clears_a_conflicted_path_too() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, content) = prune_fixture(tmp.path(), 3, "same bytes", "same bytes");
        assert_eq!(
            plan_prune(&sb, &content, "Doc.md", true, true)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn plan_prune_collapses_duplicates_but_keeps_a_real_remote_version() {
        // Stash content differs from local, so it is a genuine conflict record:
        // one copy survives, the redundant timestamps go.
        let tmp = tempfile::tempdir().unwrap();
        let (sb, content) = prune_fixture(tmp.path(), 4, "server side", "local side");
        let doomed = plan_prune(&sb, &content, "Doc.md", true, false).unwrap();
        assert_eq!(doomed.len(), 3);
        assert!(doomed
            .iter()
            .all(|d| d.reason == "duplicate of a newer stash"));
    }

    #[test]
    fn plan_prune_keeps_stashes_with_distinct_content() {
        let tmp = tempfile::tempdir().unwrap();
        let (sb, content) = prune_fixture(tmp.path(), 0, "", "local side");
        for (i, body) in ["rev one", "rev two", "rev three"].iter().enumerate() {
            std::fs::write(
                sb.join("conflicts")
                    .join(format!("Doc.202602{:02}T000000.md", i + 1)),
                body,
            )
            .unwrap();
        }
        assert!(plan_prune(&sb, &content, "Doc.md", true, false)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn stashed_paths_recovers_originals_including_nested_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let conflicts = tmp.path().join("conflicts");
        std::fs::create_dir_all(conflicts.join("Work/Career")).unwrap();
        std::fs::write(conflicts.join("Doc.20260101T000000.md"), "a").unwrap();
        std::fs::write(conflicts.join("Doc.20260102T000000.md"), "a").unwrap();
        std::fs::write(conflicts.join("Work/Career/Okta.20260101T000000.md"), "a").unwrap();
        std::fs::write(conflicts.join("not-a-stash.md"), "a").unwrap();
        assert_eq!(
            stashed_paths(&conflicts),
            vec!["Doc.md".to_string(), "Work/Career/Okta.md".to_string()]
        );
    }

    // --- compute_hash_and_mtime ---

    #[tokio::test]
    async fn compute_hash_and_mtime_returns_hash_matching_scanner() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("x.md");
        std::fs::write(&file, b"hello world").unwrap();
        let expected = hash_file(&file).unwrap();
        let expected_mtime = std::fs::metadata(&file).map(|m| mtime_ms(&m)).unwrap();
        let (h, m) = compute_hash_and_mtime(file).await.unwrap();
        assert_eq!(h, expected);
        assert_eq!(m, expected_mtime);
    }

    // --- should_print_marker_hint ---

    #[test]
    fn should_print_marker_hint_true_when_markers_present_and_not_quiet() {
        assert!(should_print_marker_hint(3, false));
    }

    #[test]
    fn should_print_marker_hint_false_when_quiet() {
        assert!(!should_print_marker_hint(3, true));
    }

    #[test]
    fn should_print_marker_hint_false_when_no_markers() {
        assert!(!should_print_marker_hint(0, false));
        assert!(!should_print_marker_hint(0, true));
    }

    // --- execute_status (no-client path) ---

    #[tokio::test]
    async fn execute_status_with_empty_space_reports_zero_counts() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        execute_status(&OutputFormat::Json, false)
            .await
            .expect("status");
    }

    #[tokio::test]
    async fn execute_status_counts_new_files() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        std::fs::create_dir_all(tmp.path().join("space")).unwrap();
        std::fs::write(tmp.path().join("space").join("a.md"), "x").unwrap();
        std::fs::write(tmp.path().join("space").join("b.md"), "y").unwrap();
        execute_status(&OutputFormat::Human, false)
            .await
            .expect("status");
    }

    #[tokio::test]
    async fn execute_status_json_reports_marker_conflicts_without_panicking() {
        // sync.dir = "." in make_space, so content lives at the space root.
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        std::fs::write(
            tmp.path().join("Marked.md"),
            "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Clean.md"), "fine").unwrap();

        // Exercises the marker-scan + JSON-shape code path added by this
        // feature; end-to-end tests (tests/cli_sync_test.rs) assert on the
        // actual "marker_conflicts" field value and --quiet hint text.
        execute_status(&OutputFormat::Json, false)
            .await
            .expect("status");
    }

    // --- execute_conflicts (no-client path) ---

    #[tokio::test]
    async fn execute_conflicts_with_empty_db_renders_no_conflicts_human() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        execute_conflicts(&OutputFormat::Human, true)
            .await
            .expect("conflicts");
    }

    #[tokio::test]
    async fn execute_conflicts_with_empty_db_renders_empty_array_json() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        execute_conflicts(&OutputFormat::Json, true)
            .await
            .expect("conflicts json");
    }

    #[tokio::test]
    async fn execute_conflicts_lists_conflict_rows_from_state_db() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        // Seed state.db with a conflict row.
        let db_path = tmp.path().join(".sb").join("state.db");
        let db = StateDb::open(&db_path).unwrap();
        db.upsert_row(&crate::sync::SyncStateRow {
            path: "Conflict.md".into(),
            local_hash: Some("lh".into()),
            remote_hash: Some("rh".into()),
            remote_etag: None,
            remote_mtime: 1000,
            local_mtime: 2000,
            status: SyncStatus::Conflict,
            conflict_at: 1700000000000,
        })
        .unwrap();
        drop(db);
        execute_conflicts(&OutputFormat::Human, true)
            .await
            .expect("conflicts");
        execute_conflicts(&OutputFormat::Json, true)
            .await
            .expect("conflicts json");
    }

    #[tokio::test]
    async fn execute_conflicts_reports_marker_conflicts_distinct_from_stash() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());

        // A stash (metadata-driven) conflict.
        let db_path = tmp.path().join(".sb").join("state.db");
        let db = StateDb::open(&db_path).unwrap();
        db.upsert_row(&crate::sync::SyncStateRow {
            path: "Stashed.md".into(),
            local_hash: Some("lh".into()),
            remote_hash: Some("rh".into()),
            remote_etag: None,
            remote_mtime: 1000,
            local_mtime: 2000,
            status: SyncStatus::Conflict,
            conflict_at: 1700000000000,
        })
        .unwrap();
        drop(db);

        // A marker conflict (content-driven) on a different, untracked file.
        std::fs::write(
            tmp.path().join("Marked.md"),
            "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n",
        )
        .unwrap();

        // Both formats should succeed and not conflate the two kinds; the
        // exact text/kind-tagging is asserted end-to-end in
        // tests/cli_sync_test.rs where stdout can actually be captured.
        execute_conflicts(&OutputFormat::Human, true)
            .await
            .expect("conflicts human");
        execute_conflicts(&OutputFormat::Json, true)
            .await
            .expect("conflicts json");
    }

    // --- parse_choice ---

    #[test]
    fn parse_choice_accepts_every_option() {
        assert_eq!(parse_choice("l"), Some(Choice::Local));
        assert_eq!(parse_choice("r"), Some(Choice::Remote));
        assert_eq!(parse_choice("d"), Some(Choice::Diff));
        assert_eq!(parse_choice("s"), Some(Choice::Skip));
        assert_eq!(parse_choice("q"), Some(Choice::Quit));
    }

    #[test]
    fn parse_choice_ignores_case_and_surrounding_whitespace() {
        // Real replies arrive from read_line with a trailing newline.
        assert_eq!(parse_choice("L\n"), Some(Choice::Local));
        assert_eq!(parse_choice("  R  \n"), Some(Choice::Remote));
        assert_eq!(parse_choice("S\r\n"), Some(Choice::Skip));
    }

    #[test]
    fn parse_choice_returns_none_for_garbage_so_caller_reprompts() {
        for bad in ["", "\n", "x", "lr", "quit", "1"] {
            assert_eq!(parse_choice(bad), None, "expected {bad:?} to reprompt");
        }
    }

    // --- execute_resolve ---

    #[tokio::test]
    async fn execute_resolve_rejects_path_traversal() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        let err = execute_resolve(
            None,
            Some("../etc/passwd"),
            false,
            false,
            false,
            false,
            false,
            true,
            &OutputFormat::Human,
        )
        .await
        .unwrap_err();
        match err {
            SbError::Usage(msg) => assert!(msg.contains("must not contain '..'")),
            other => panic!("expected Usage, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_resolve_rejects_absolute_path() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        let err = execute_resolve(
            None,
            Some("/etc/shadow"),
            false,
            false,
            false,
            false,
            false,
            true,
            &OutputFormat::Human,
        )
        .await
        .unwrap_err();
        match err {
            SbError::Usage(msg) => assert!(msg.contains("relative path")),
            other => panic!("expected Usage, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_resolve_errors_when_stash_dir_missing() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        let err = execute_resolve(
            None,
            Some("Doc.md"),
            false,
            true,
            false,
            false,
            true,
            true,
            &OutputFormat::Human,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SbError::Filesystem { .. }));
    }

    #[tokio::test]
    async fn execute_resolve_errors_when_path_not_in_state_db() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        // Create stash file but no state.db row.
        let conflicts = tmp.path().join(".sb").join("conflicts");
        std::fs::create_dir_all(&conflicts).unwrap();
        std::fs::write(conflicts.join("Doc.20260101T000000.md"), "stash").unwrap();
        // Touch state.db so it exists
        let _ = StateDb::open(&tmp.path().join(".sb").join("state.db")).unwrap();

        let err = execute_resolve(
            None,
            Some("Doc.md"),
            false,
            true,
            false,
            false,
            true,
            true,
            &OutputFormat::Human,
        )
        .await
        .unwrap_err();
        match err {
            SbError::Filesystem { message, .. } => {
                assert!(message.contains("not tracked"), "{message}")
            }
            other => panic!("expected Filesystem, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_resolve_errors_when_row_not_in_conflict_status() {
        let tmp = make_space(Some("https://example.com"));
        let _g = SbSpaceGuard::set(tmp.path());
        // Create stash file
        let conflicts = tmp.path().join(".sb").join("conflicts");
        std::fs::create_dir_all(&conflicts).unwrap();
        std::fs::write(conflicts.join("Doc.20260101T000000.md"), "stash").unwrap();
        // Seed row with Synced (not Conflict)
        let db_path = tmp.path().join(".sb").join("state.db");
        let db = StateDb::open(&db_path).unwrap();
        db.upsert_row(&crate::sync::SyncStateRow {
            path: "Doc.md".into(),
            local_hash: Some("lh".into()),
            remote_hash: Some("rh".into()),
            remote_etag: None,
            remote_mtime: 1000,
            local_mtime: 1000,
            status: SyncStatus::Synced,
            conflict_at: 0,
        })
        .unwrap();
        drop(db);
        let err = execute_resolve(
            None,
            Some("Doc.md"),
            false,
            true,
            false,
            false,
            true,
            true,
            &OutputFormat::Human,
        )
        .await
        .unwrap_err();
        match err {
            SbError::Filesystem { message, .. } => {
                assert!(message.contains("not in conflict"), "{message}")
            }
            other => panic!("expected Filesystem, got: {other:?}"),
        }
    }
}
