use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};

use crate::cli::{OutputFormat, SortField};
use crate::commands::server::build_client;
use crate::config;
use crate::error::{SbError, SbResult};
use crate::output;

/// Convert user-visible page name to filesystem-relative .md path.
///
/// Example: "Journal/2026-04-05" -> PathBuf("Journal/2026-04-05.md")
pub fn page_name_to_path(name: &str) -> PathBuf {
    PathBuf::from(format!("{}.md", name))
}

/// Strip .md extension to get display name.
///
/// Example: Path("Journal/2026-04-05.md") -> "Journal/2026-04-05"
pub fn path_to_page_name(path: &Path) -> String {
    path.with_extension("").to_string_lossy().into_owned()
}

/// Resolve full filesystem path for a page given space root.
pub fn resolve_page_path(space_root: &Path, name: &str) -> PathBuf {
    space_root.join(page_name_to_path(name))
}

/// Validate that a page name does not contain path traversal components.
///
/// Rejects any name where a component is `..` (Component::ParentDir).
/// Returns the resolved absolute path on success.
///
/// Security: This check runs BEFORE any filesystem operation.
pub fn validate_page_path(space_root: &Path, name: &str) -> SbResult<PathBuf> {
    for component in Path::new(name).components() {
        if let Component::ParentDir = component {
            return Err(SbError::Usage(format!(
                "invalid page name '{}': path traversal not allowed",
                name
            )));
        }
    }
    Ok(resolve_page_path(space_root, name))
}

/// Find the space root using a layered resolver:
/// 1. `SB_SPACE` env var (absolute or `~/`-relative)
/// 2. Walk up from cwd looking for `.sb/config.toml`
/// 3. `space` field in `$XDG_CONFIG_HOME/sb/config.toml`
pub fn find_space_root() -> SbResult<PathBuf> {
    // 1. SB_SPACE env override
    if let Ok(val) = std::env::var("SB_SPACE") {
        let expanded = crate::config::expand_tilde(&val)?;
        if !expanded.join(".sb").join("config.toml").is_file() {
            return Err(SbError::SpaceNotFound {
                configured_path: expanded.display().to_string(),
                via: "SB_SPACE environment variable".to_string(),
            });
        }
        return Ok(expanded);
    }
    // 2. Walk up from cwd
    let cwd = std::env::current_dir().map_err(|e| SbError::Config {
        message: format!("cannot determine current directory: {e}"),
    })?;
    if let Ok(root) = find_space_root_from(&cwd) {
        return Ok(root);
    }
    // 3. XDG config
    let user_config = crate::config::load_user_config()?;
    if let Some(ref path_str) = user_config.space {
        let expanded = crate::config::expand_tilde(path_str)?;
        if !expanded.join(".sb").join("config.toml").is_file() {
            let xdg_path = crate::config::xdg_config_dir()
                .map(|d| d.join("config.toml").display().to_string())
                .unwrap_or_else(|_| "~/.config/sb/config.toml".to_string());
            return Err(SbError::SpaceNotFound {
                configured_path: expanded.display().to_string(),
                via: xdg_path,
            });
        }
        return Ok(expanded);
    }
    Err(SbError::NotInitialized)
}

/// Find space root starting from a specific directory (testable variant).
pub fn find_space_root_from(start: &Path) -> SbResult<PathBuf> {
    let config_path = config::find_config_file(start).ok_or(SbError::NotInitialized)?;
    // config_path is .sb/config.toml -> parent() is .sb/ -> parent() is space root
    let space_root = config_path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| SbError::Config {
            message: "unexpected config path structure".to_string(),
        })?;
    Ok(space_root.to_path_buf())
}

/// A single page entry for listing.
#[derive(Debug, serde::Serialize)]
struct PageEntry {
    name: String,
    modified: String,
    #[serde(skip)]
    modified_time: std::time::SystemTime,
    #[serde(skip)]
    created_time: Option<std::time::SystemTime>,
}

/// Collect all `.md` files from `space_root`, excluding the `.sb/` subtree.
fn collect_pages(space_root: &Path) -> SbResult<Vec<PageEntry>> {
    use walkdir::WalkDir;

    let sb_dir = space_root.join(".sb");
    let mut entries = Vec::new();

    for entry in WalkDir::new(space_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !e.path().starts_with(&sb_dir))
    {
        let entry = entry.map_err(|e| SbError::Filesystem {
            message: "error walking directory".to_string(),
            path: space_root.display().to_string(),
            source: e.into_io_error(),
        })?;

        // Only process files with .md extension
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let metadata = path.metadata().map_err(|e| SbError::Filesystem {
            message: "cannot read file metadata".to_string(),
            path: path.display().to_string(),
            source: Some(e),
        })?;

        let modified_time = metadata.modified().map_err(|e| SbError::Filesystem {
            message: "cannot read modification time".to_string(),
            path: path.display().to_string(),
            source: Some(e),
        })?;

        // created_time is platform-dependent; use None on platforms that don't support it
        let created_time = metadata.created().ok();

        // Compute relative path from space root, use forward slashes
        let rel_path = path
            .strip_prefix(space_root)
            .map_err(|_| SbError::Filesystem {
                message: "path is not under space root".to_string(),
                path: path.display().to_string(),
                source: None,
            })?;

        // Strip .md extension and normalise to forward slashes
        let name = path_to_page_name(rel_path).replace('\\', "/");

        // Format modified time for human display: "2026-04-05 14:30"
        let modified = format_system_time_human(modified_time);

        entries.push(PageEntry {
            name,
            modified,
            modified_time,
            created_time,
        });
    }

    Ok(entries)
}

/// Format a `SystemTime` as `"YYYY-MM-DD HH:MM"` in local time.
fn format_system_time_human(t: std::time::SystemTime) -> String {
    match jiff::Timestamp::try_from(t) {
        Ok(ts) => {
            let zdt = ts.to_zoned(jiff::tz::TimeZone::system());
            zdt.strftime("%Y-%m-%d %H:%M").to_string()
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Format a `SystemTime` as ISO 8601 for JSON output.
fn format_system_time_iso(t: std::time::SystemTime) -> String {
    use jiff::Timestamp;
    match Timestamp::try_from(t) {
        Ok(ts) => {
            let zdt = ts.to_zoned(jiff::tz::TimeZone::UTC);
            zdt.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
        }
        Err(_) => "unknown".to_string(),
    }
}

/// List all pages in the local space with sort, limit, and format options.
pub async fn execute_list(
    sort: &SortField,
    limit: Option<usize>,
    format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    let _ = (quiet, color); // reserved for future use (e.g., colored output)

    let space_root = find_space_root()?;
    let mut entries = collect_pages(&space_root)?;

    // Sort
    match sort {
        SortField::Name => {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
        }
        SortField::Modified => {
            entries.sort_by(|a, b| b.modified_time.cmp(&a.modified_time));
        }
        SortField::Created => {
            if entries.iter().any(|e| e.created_time.is_none()) {
                tracing::warn!(
                    "created time not available on this platform; falling back to modified time"
                );
            }
            entries.sort_by(|a, b| {
                let at = a.created_time.unwrap_or(a.modified_time);
                let bt = b.created_time.unwrap_or(b.modified_time);
                bt.cmp(&at)
            });
        }
    }

    // Apply limit
    if let Some(n) = limit {
        entries.truncate(n);
    }

    match format {
        OutputFormat::Json => {
            // For JSON, emit ISO 8601 modified timestamps
            #[derive(serde::Serialize)]
            struct JsonEntry<'a> {
                name: &'a str,
                modified: String,
            }
            let json_entries: Vec<JsonEntry> = entries
                .iter()
                .map(|e| JsonEntry {
                    name: &e.name,
                    modified: format_system_time_iso(e.modified_time),
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json_entries).map_err(|e| {
                    SbError::Config {
                        message: format!("failed to serialize page list: {e}"),
                    }
                })?
            );
        }
        OutputFormat::Human => {
            if entries.is_empty() {
                return Ok(());
            }
            let name_width = entries
                .iter()
                .map(|e| e.name.len())
                .max()
                .unwrap_or(4)
                .max(4);
            for entry in &entries {
                println!(
                    "{:<width$}  {}",
                    entry.name,
                    entry.modified,
                    width = name_width
                );
            }
        }
    }

    Ok(())
}

/// Read a page's content and print it to stdout.
///
/// - Local mode: reads the file from the local space directory.
/// - Remote mode (`--remote`): fetches the page from the SilverBullet server via SbClient.
pub async fn execute_read(
    cli_token: Option<&str>,
    name: &str,
    remote: bool,
    _format: &OutputFormat,
    _quiet: bool,
    _color: bool,
) -> SbResult<()> {
    let space_root = find_space_root()?;
    validate_page_path(&space_root, name)?;

    if remote {
        let client = build_client(cli_token)?;
        let content = client.get_page(name).await?;
        print!("{}", content);
    } else {
        let page_path = resolve_page_path(&space_root, name);
        if !page_path.exists() {
            return Err(SbError::PageNotFound {
                name: name.to_string(),
            });
        }
        let content = std::fs::read_to_string(&page_path).map_err(|e| SbError::Filesystem {
            message: "failed to read page".to_string(),
            path: page_path.display().to_string(),
            source: Some(e),
        })?;
        print!("{}", content);
    }

    Ok(())
}

/// Create a new page with content from --content flag, stdin pipe, --template, or editor.
///
/// Content priority:
///   1. `--content` flag
///   2. stdin pipe (when stdin is not a TTY)
///   3. `--template` (local file first, then remote)
///   4. Open editor (empty file)
#[allow(clippy::too_many_arguments)]
pub async fn execute_create(
    cli_token: Option<&str>,
    name: &str,
    content: Option<&str>,
    edit: bool,
    template: Option<&str>,
    _format: &OutputFormat,
    quiet: bool,
    color: bool,
) -> SbResult<()> {
    use std::io::Read;

    let space_root = find_space_root()?;
    let page_path = validate_page_path(&space_root, name)?;

    // Duplicate check
    if page_path.exists() {
        return Err(SbError::PageAlreadyExists {
            name: name.to_string(),
        });
    }

    // Create parent directories
    if let Some(parent) = page_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
            message: "failed to create parent directories".to_string(),
            path: parent.display().to_string(),
            source: Some(e),
        })?;
    }

    // Determine content body (priority order)
    let body: String;
    let open_editor: bool;

    if let Some(c) = content {
        // 1. --content flag
        body = c.to_string();
        open_editor = edit;
    } else if !std::io::stdin().is_terminal() {
        // 2. stdin pipe
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| SbError::Filesystem {
                message: "failed to read stdin".to_string(),
                path: "<stdin>".to_string(),
                source: Some(e),
            })?;
        body = buf;
        open_editor = edit;
    } else if let Some(tmpl_name) = template {
        // 3. --template: try local first, then remote
        let local_tmpl = resolve_page_path(&space_root, tmpl_name);
        if local_tmpl.exists() {
            body = std::fs::read_to_string(&local_tmpl).map_err(|e| SbError::Filesystem {
                message: "failed to read template".to_string(),
                path: local_tmpl.display().to_string(),
                source: Some(e),
            })?;
        } else {
            let client = build_client(cli_token)?;
            body = client.get_page(tmpl_name).await?;
        }
        open_editor = edit;
    } else {
        // 4. No content source — write empty file and open editor
        body = String::new();
        open_editor = true;
    }

    // Write the file
    std::fs::write(&page_path, &body).map_err(|e| SbError::Filesystem {
        message: "failed to write page".to_string(),
        path: page_path.display().to_string(),
        source: Some(e),
    })?;

    if open_editor {
        open_in_editor(&page_path).await?;
    }

    crate::output::print_success(&format!("created page '{name}'"), color, quiet);

    Ok(())
}

/// Edit an existing page in `$EDITOR`.
pub async fn execute_edit(name: &str, _quiet: bool, _color: bool) -> SbResult<()> {
    let space_root = find_space_root()?;
    let page_path = validate_page_path(&space_root, name)?;
    if !page_path.exists() {
        return Err(SbError::PageNotFound {
            name: name.to_string(),
        });
    }
    open_in_editor(&page_path).await?;
    Ok(())
}

/// Prompt user for delete confirmation on a TTY.
///
/// Non-TTY stdin without --force is a fail-safe: refuse deletion to prevent
/// scripted deletion without explicit opt-in.
async fn confirm_delete(name: &str) -> SbResult<bool> {
    if !std::io::stdin().is_terminal() {
        return Err(SbError::Usage(
            "cannot confirm deletion in non-interactive mode; use --force".into(),
        ));
    }
    let name = name.to_string();
    let confirmed = tokio::task::spawn_blocking(move || -> bool {
        use std::io::Write;
        eprint!("Delete {}? [y/N] ", name);
        std::io::stderr().flush().ok();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("delete prompt task failed: {e}"),
    })?;
    Ok(confirmed)
}

/// Delete a page.
///
/// Without --force: prompts on TTY, refuses on non-TTY (fail-safe).
/// With --force: deletes immediately without prompting.
pub async fn execute_delete(name: &str, force: bool, quiet: bool, color: bool) -> SbResult<()> {
    let space_root = find_space_root()?;
    let page_path = validate_page_path(&space_root, name)?;
    if !page_path.exists() {
        return Err(SbError::PageNotFound {
            name: name.to_string(),
        });
    }
    if !force {
        let confirmed = confirm_delete(name).await?;
        if !confirmed {
            output::print_success("Cancelled", color, quiet);
            return Ok(());
        }
    }
    std::fs::remove_file(&page_path).map_err(|e| SbError::Filesystem {
        message: "failed to delete page".into(),
        path: page_path.display().to_string(),
        source: Some(e),
    })?;
    output::print_success(&format!("Deleted {}", name), color, quiet);
    Ok(())
}

/// Append content to a page, creating it if it doesn't exist.
pub async fn execute_append(name: &str, content: &str, quiet: bool, color: bool) -> SbResult<()> {
    let space_root = find_space_root()?;
    let page_path = validate_page_path(&space_root, name)?;
    if page_path.exists() {
        // Append with newline separator using OpenOptions::append
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&page_path)
            .map_err(|e| SbError::Filesystem {
                message: "failed to open page for append".into(),
                path: page_path.display().to_string(),
                source: Some(e),
            })?;
        file.write_all(format!("\n{}", content).as_bytes())
            .map_err(|e| SbError::Filesystem {
                message: "failed to append to page".into(),
                path: page_path.display().to_string(),
                source: Some(e),
            })?;
    } else {
        // Auto-create page if it doesn't exist
        if let Some(parent) = page_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
                message: "failed to create directory".into(),
                path: parent.display().to_string(),
                source: Some(e),
            })?;
        }
        std::fs::write(&page_path, content).map_err(|e| SbError::Filesystem {
            message: "failed to create page".into(),
            path: page_path.display().to_string(),
            source: Some(e),
        })?;
    }
    output::print_success(&format!("Appended to {}", name), color, quiet);
    Ok(())
}

/// Move/rename a page, creating intermediate directories for the target.
pub async fn execute_move(name: &str, new_name: &str, quiet: bool, color: bool) -> SbResult<()> {
    let space_root = find_space_root()?;
    let src_path = validate_page_path(&space_root, name)?;
    let dst_path = validate_page_path(&space_root, new_name)?;
    if !src_path.exists() {
        return Err(SbError::PageNotFound {
            name: name.to_string(),
        });
    }
    if dst_path.exists() {
        return Err(SbError::PageAlreadyExists {
            name: new_name.to_string(),
        });
    }
    // Create intermediate directories for destination
    if let Some(parent) = dst_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
            message: "failed to create directory".into(),
            path: parent.display().to_string(),
            source: Some(e),
        })?;
    }
    std::fs::rename(&src_path, &dst_path).map_err(|e| SbError::Filesystem {
        message: "failed to move page".into(),
        path: src_path.display().to_string(),
        source: Some(e),
    })?;

    // Update state.db so sync engine detects the move correctly.
    // Old path -> deleted (remove row), New path -> new (insert with status='new').
    // No-op if state.db doesn't exist yet (space initialized but never synced).
    let db_path = space_root.join(".sb").join("state.db");
    if db_path.exists() {
        let old_rel = format!("{}.md", name);
        let new_rel = format!("{}.md", new_name);
        let db_path_owned = db_path.clone();
        tokio::task::spawn_blocking(move || -> SbResult<()> {
            let db = crate::sync::db::StateDb::open(&db_path_owned)?;
            // delete_row is a no-op if path not present (handles pre-sync case)
            db.delete_row(&old_rel)?;
            // Insert new path as status='new' so next push uploads it
            db.upsert_row(&crate::sync::SyncStateRow {
                path: new_rel,
                local_hash: None,
                remote_hash: None,
                remote_mtime: 0,
                local_mtime: 0,
                status: crate::sync::SyncStatus::New,
                conflict_at: 0,
            })?;
            Ok(())
        })
        .await
        .map_err(|e| SbError::Filesystem {
            message: format!("state.db update task panicked: {e}"),
            path: db_path.display().to_string(),
            source: None,
        })??;
    }

    output::print_success(&format!("Moved {} -> {}", name, new_name), color, quiet);
    Ok(())
}

/// Open a file in `$EDITOR`.
///
/// Splits `$EDITOR` on whitespace so multi-word values like `"code --wait"` work:
/// first token is the binary, the rest are prepended arguments.
pub async fn open_in_editor(path: &Path) -> SbResult<()> {
    let editor_str = std::env::var("EDITOR").map_err(|_| SbError::EditorNotSet)?;
    let parts: Vec<&str> = editor_str.split_whitespace().collect();
    if parts.is_empty() {
        return Err(SbError::EditorNotSet);
    }
    let editor_bin = parts[0].to_string();
    let editor_args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&editor_bin);
        for arg in &editor_args {
            cmd.arg(arg);
        }
        cmd.arg(&path);
        let status = cmd.status().map_err(|e| SbError::Config {
            message: format!("failed to launch editor '{}': {}", editor_bin, e),
        })?;
        if !status.success() {
            return Err(SbError::ProcessFailed {
                code: status.code().unwrap_or(1),
                stderr: String::new(),
            });
        }
        Ok(())
    })
    .await
    .map_err(|e| SbError::Config {
        message: format!("editor task failed: {e}"),
    })??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn setup_space_root() -> TempDir {
        let dir = tempfile::tempdir().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        std::fs::create_dir_all(&sb_dir).expect("create .sb dir");
        let mut f = std::fs::File::create(sb_dir.join("config.toml")).expect("create config.toml");
        f.write_all(b"server_url = \"https://sb.example.com\"\n")
            .expect("write config.toml");
        dir
    }

    // --- page_name_to_path tests ---

    #[test]
    fn page_name_to_path_simple_name() {
        let result = page_name_to_path("my-notes");
        assert_eq!(result, PathBuf::from("my-notes.md"));
    }

    #[test]
    fn page_name_to_path_nested_name() {
        let result = page_name_to_path("Journal/2026-04-05");
        assert_eq!(result, PathBuf::from("Journal/2026-04-05.md"));
    }

    // --- path_to_page_name tests ---

    #[test]
    fn path_to_page_name_strips_md_extension() {
        let result = path_to_page_name(Path::new("Journal/2026-04-05.md"));
        assert_eq!(result, "Journal/2026-04-05");
    }

    #[test]
    fn path_to_page_name_simple_file() {
        let result = path_to_page_name(Path::new("my-notes.md"));
        assert_eq!(result, "my-notes");
    }

    // --- resolve_page_path tests ---

    #[test]
    fn resolve_page_path_joins_space_root_and_name() {
        let space_root = Path::new("/space");
        let result = resolve_page_path(space_root, "my-notes");
        assert_eq!(result, PathBuf::from("/space/my-notes.md"));
    }

    // --- find_space_root_from tests ---

    #[test]
    fn find_space_root_from_dir_with_sb_config() {
        let dir = setup_space_root();
        let result = find_space_root_from(dir.path()).expect("should find space root");
        // The space root should be the dir containing .sb/
        assert_eq!(result, dir.path());
    }

    #[test]
    fn find_space_root_returns_not_initialized_when_no_config() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let result = find_space_root_from(dir.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            SbError::NotInitialized => {}
            other => panic!("expected NotInitialized, got: {other:?}"),
        }
    }

    // --- validate_page_path tests ---

    #[test]
    fn validate_page_path_accepts_normal_nested_path() {
        let space_root = Path::new("/space");
        let result = validate_page_path(space_root, "Projects/new-idea");
        assert!(result.is_ok(), "should accept normal nested path");
        let path = result.unwrap();
        assert_eq!(path, PathBuf::from("/space/Projects/new-idea.md"));
    }

    #[test]
    fn validate_page_path_rejects_parent_dir_traversal() {
        let space_root = Path::new("/space");
        let result = validate_page_path(space_root, "../etc/passwd");
        assert!(result.is_err(), "should reject path traversal");
        match result.unwrap_err() {
            SbError::Usage(msg) => {
                assert!(
                    msg.contains("path traversal"),
                    "error message should mention path traversal: {msg}"
                );
            }
            other => panic!("expected Usage error, got: {other:?}"),
        }
    }

    #[test]
    fn validate_page_path_rejects_double_dot_in_middle() {
        let space_root = Path::new("/space");
        let result = validate_page_path(space_root, "Projects/../../../etc/shadow");
        assert!(result.is_err(), "should reject embedded path traversal");
    }
}
