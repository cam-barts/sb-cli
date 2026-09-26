use globset::{Glob, GlobSet, GlobSetBuilder};
use std::io::Read;
use std::path::Path;
use walkdir::WalkDir;

use crate::error::{SbError, SbResult};

/// Returns the modification time of the given metadata as milliseconds since Unix epoch.
/// Returns 0 if the mtime is unavailable. Uses `i64::try_from` to avoid silent truncation.
pub fn mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Returns the modification time of the file at `path` as milliseconds since Unix epoch.
/// Returns 0 if the file metadata is unavailable or mtime cannot be represented.
pub async fn mtime_ms_from_path(path: &std::path::Path) -> i64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| mtime_ms(&m))
        .unwrap_or(0)
}

/// Information about a local file discovered by scanning
#[derive(Debug, Clone)]
pub struct LocalFileInfo {
    /// Relative path from space root, forward slashes, e.g. "Journal/2026-04-05.md"
    pub rel_path: String,
    /// blake3 hex digest of file contents
    pub hash: String,
    /// File modification time as Unix milliseconds
    pub mtime_ms: i64,
    /// File size in bytes
    pub size: u64,
}

/// Glob-based file filter for sync include/exclude
#[derive(Clone)]
pub struct FileFilter {
    exclude_set: GlobSet,
    include_set: GlobSet,
    attachments_enabled: bool,
}

impl FileFilter {
    /// Create a new FileFilter from exclude and include glob patterns.
    ///
    /// `attachments` controls whether non-.md files are allowed through the filter.
    /// When false (default), only .md files pass. When true, all files pass (minus .sb/ and excludes).
    ///
    /// Default exclude should be ["_plug/*"] if not overridden by caller.
    pub fn new(excludes: &[String], includes: &[String], attachments: bool) -> SbResult<Self> {
        let mut ex_builder = GlobSetBuilder::new();
        for pattern in excludes {
            ex_builder.add(Glob::new(pattern).map_err(|e| SbError::Config {
                message: format!("invalid exclude glob '{pattern}': {e}"),
            })?);
        }
        let mut in_builder = GlobSetBuilder::new();
        for pattern in includes {
            in_builder.add(Glob::new(pattern).map_err(|e| SbError::Config {
                message: format!("invalid include glob '{pattern}': {e}"),
            })?);
        }
        Ok(FileFilter {
            exclude_set: ex_builder.build().map_err(|e| SbError::Config {
                message: format!("failed to build exclude globset: {e}"),
            })?,
            include_set: in_builder.build().map_err(|e| SbError::Config {
                message: format!("failed to build include globset: {e}"),
            })?,
            attachments_enabled: attachments,
        })
    }

    /// Returns true if the file should be synced.
    ///
    /// .sb/ paths are ALWAYS excluded — enforced by the scanner before
    /// this method is called via filter_entry.
    ///
    /// Include overrides exclude: if the path matches an include pattern
    /// it is accepted even if it also matches an exclude pattern.
    ///
    /// When attachments_enabled=false, only .md files pass the filter.
    pub fn should_sync(&self, rel_path: &str) -> bool {
        // .sb/ is always excluded — defend in depth even if caller misses it
        if rel_path.starts_with(".sb/") || rel_path == ".sb" {
            return false;
        }
        // include overrides exclude
        if self.include_set.is_match(rel_path) {
            return true;
        }
        if self.exclude_set.is_match(rel_path) {
            return false;
        }
        // when attachments disabled, only .md files pass
        if !self.attachments_enabled && !rel_path.ends_with(".md") {
            return false;
        }
        true
    }
}

/// Kind of server/editor-written conflict artifact found on disk.
///
/// Distinct from the metadata-driven conflict tracked in state.db
/// (`SyncStatus::Conflict`, stashed under `.sb/conflicts/` and resolved via
/// `sb sync resolve`): this is a pure content/filename heuristic over the
/// *current* working file, recomputed fresh on every scan and never
/// persisted. Nothing in this module writes to state.db or touches the
/// stash directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerConflictKind {
    /// Git-style markers (`<<<<<<< ...` / `=======` / `>>>>>>> ...`) written
    /// directly into the file's content by SilverBullet's server-side merge
    /// (or a plain git merge) when it couldn't auto-resolve a line.
    InlineMarkers,
    /// A `name.conflicted-<hash>.ext` sibling file: SilverBullet's fallback
    /// for binaries and text over its merge size limit, where it can't
    /// splice markers into the content at all.
    ConflictedSibling,
}

impl MarkerConflictKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MarkerConflictKind::InlineMarkers => "inline_markers",
            MarkerConflictKind::ConflictedSibling => "conflicted_sibling",
        }
    }
}

/// A local file flagged by the marker-conflict scan (see `scan_marker_conflicts`).
#[derive(Debug, Clone)]
pub struct MarkerConflict {
    /// Relative path from space root, forward slashes.
    pub rel_path: String,
    pub kind: MarkerConflictKind,
}

/// Length of the run of `ch` at the start of `trimmed`.
fn fence_run_len(trimmed: &str, ch: char) -> usize {
    trimmed.chars().take_while(|&c| c == ch).count()
}

/// If `trimmed` opens a markdown fence (a run of 3+ backticks or tildes),
/// returns the fence character and run length.
fn fence_open_marker(trimmed: &str) -> Option<(char, usize)> {
    for ch in ['`', '~'] {
        let run = fence_run_len(trimmed, ch);
        if run >= 3 {
            return Some((ch, run));
        }
    }
    None
}

/// Marks which lines of `lines` fall inside a *properly closed* markdown
/// fence. Per CommonMark, a fence is closed only by a line of the SAME
/// character, whose run is at least as long as the opener's, and which
/// (ignoring trailing whitespace) contains nothing else. Backtick and tilde
/// fences are tracked separately -- a stray `~~~` inside a ``` block, or
/// vice versa, is just content, not a fence boundary.
///
/// An opener with no matching closer by EOF (a truncated paste, say) is left
/// unmarked: we'd rather risk scanning a few lines of an abandoned code
/// block than go blind for the rest of the file, which is the wrong failure
/// direction for a "did you miss a conflict" check.
fn fenced_line_mask(lines: &[&str]) -> Vec<bool> {
    let mut fenced = vec![false; lines.len()];
    let mut open: Option<(char, usize, usize)> = None; // (char, len, start_index)

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if let Some((ch, len, start)) = open {
            let run = fence_run_len(trimmed, ch);
            if run >= len && run == trimmed.trim_end().chars().count() {
                for f in fenced.iter_mut().take(i + 1).skip(start) {
                    *f = true;
                }
                open = None;
            }
        } else if let Some((ch, len)) = fence_open_marker(trimmed) {
            open = Some((ch, len, i));
        }
    }

    fenced
}

/// Returns true if `content` contains a well-formed conflict-marker triple:
/// an opening `<<<<<<<` line, then a `=======` line, then a `>>>>>>>` line,
/// each starting at column 0 (no leading whitespace), in that order, with no
/// blank line between them. Matches both `<<<<<<< SB sha256:...`
/// (SilverBullet's server-side merge) and plain git markers
/// (`<<<<<<< HEAD`) since both share the `<<<<<<<` prefix.
///
/// Tradeoff: Cam's space contains git documentation, pasted diffs, and
/// fenced code blocks that *mention* conflict markers without being one.
/// Three defenses, aimed at the real shape of the two failure modes:
///
///   1. Markdown code fences (``` or ~~~) suspend scanning for their
///      extent -- a documented example inside a fenced block never
///      completes (or even starts) the triple. See `fenced_line_mask` for
///      how fence boundaries are matched (same character, closer >= opener).
///   2. Markers must sit at column 0. Real markers are written by tools
///      that never indent them; an indented markdown code example (4-space
///      indented, like this feature's own spec) fails the check too.
///   3. No blank line is tolerated between the three tokens. A real conflict
///      hunk is a single contiguous splice -- the server never writes a
///      paragraph break between the marker and its content. A doc page that
///      mentions each marker in its own section is, definitionally, broken
///      up by blank lines (headers, paragraph breaks) between them, so a
///      blank line resets the match instead of letting SeenOpen/
///      SeenSeparator survive indefinitely across unrelated prose.
///
/// Tradeoff for rule 3: a real conflict whose hunk spans a blank line (e.g.
/// SB merged two paragraphs and the differing region includes a paragraph
/// break) would be missed. That's judged rarer and cheaper than flagging
/// every "here's how conflict markers work" page in the space as unresolved.
/// A real conflict marker triple typed out *inside* a fence (e.g. someone
/// pastes an already-conflicted file into a note) is also still missed.
pub fn has_conflict_markers(content: &str) -> bool {
    #[derive(PartialEq, Eq)]
    enum State {
        Searching,
        SeenOpen,
        SeenSeparator,
    }

    let lines: Vec<&str> = content.lines().collect();
    let fenced = fenced_line_mask(&lines);

    let mut state = State::Searching;
    for (i, &line) in lines.iter().enumerate() {
        if fenced[i] || line.trim().is_empty() {
            state = State::Searching;
            continue;
        }

        if line.starts_with("<<<<<<<") {
            state = State::SeenOpen;
        } else if line == "=======" {
            state = if state == State::SeenOpen {
                State::SeenSeparator
            } else {
                State::Searching
            };
        } else if line.starts_with(">>>>>>>") {
            if state == State::SeenSeparator {
                return true;
            }
            state = State::Searching;
        }
    }
    false
}

/// Returns true if `filename` (basename only, no directory component)
/// matches SilverBullet's unmergeable-conflict sibling pattern:
/// `name.conflicted-<hash>.ext`, or `name.conflicted-<hash>` with no
/// extension, where `<hash>` is a short hex digest (6-64 hex chars, covering
/// short digests up through a full sha256 hex string).
///
/// Restricting to hex (not any alphanumeric token) is deliberate: an
/// ordinary page named e.g. `My.conflicted-notes.md` was previously matching
/// because "notes" is alphanumeric. Hex is the actual server format and
/// still accepts every real digest, so tightening it costs nothing.
pub fn is_conflicted_sibling_name(filename: &str) -> bool {
    match filename.split_once(".conflicted-") {
        Some((stem, rest)) if !stem.is_empty() => {
            let hash_part = rest.split('.').next().unwrap_or("");
            (6..=64).contains(&hash_part.len()) && hash_part.chars().all(|c| c.is_ascii_hexdigit())
        }
        _ => false,
    }
}

/// Scan `space_root` for server/editor-written conflict artifacts: inline
/// marker triples inside file content, and `.conflicted-<hash>.` sibling
/// files. Purely a filesystem/content read -- REPORT ONLY. Never touches
/// state.db, never modifies, merges, or deletes anything.
///
/// Unlike `LocalScanner::scan`, this ignores `FileFilter`: a conflicted
/// sibling can be a binary attachment that's normally excluded from sync,
/// but it still represents an unresolved conflict Cam needs to know about.
/// `.sb/` is still always skipped.
///
/// Content is only read for files that could plausibly hold server-written
/// markers. SilverBullet's own server-side merge refuses anything over 1
/// MiB, so a bigger file -- e.g. a 200+ MB `.zk/notebook.db` -- can never
/// contain a marker the server wrote, and is skipped without a read: no
/// stat-then-slurp of a multi-hundred-MB file just to throw it away. This is
/// also the cap on how much of a file is ever read, so a hand-pasted marker
/// triple in an oversized file is a known, accepted miss.
pub fn scan_marker_conflicts(space_root: &Path) -> SbResult<Vec<MarkerConflict>> {
    /// SilverBullet's server-side merge limit: files above this size are
    /// never merged, so they can never contain server-written markers.
    const MAX_MERGEABLE_BYTES: u64 = 1024 * 1024;

    let mut results = Vec::new();

    let walker = WalkDir::new(space_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                if name == ".sb" {
                    return false;
                }
            }
            true
        });

    for entry in walker {
        let entry = entry.map_err(|e| SbError::Filesystem {
            message: format!("error walking directory: {e}"),
            path: space_root.display().to_string(),
            source: None,
        })?;
        if !entry.file_type().is_file() {
            continue;
        }

        let abs_path = entry.path();
        let rel_path = abs_path
            .strip_prefix(space_root)
            .map_err(|_| SbError::Filesystem {
                message: "cannot compute relative path".into(),
                path: abs_path.display().to_string(),
                source: None,
            })?;
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");

        let file_name = abs_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if is_conflicted_sibling_name(file_name) {
            results.push(MarkerConflict {
                rel_path: rel_str,
                kind: MarkerConflictKind::ConflictedSibling,
            });
            continue;
        }

        // Skip files too big to ever have been merged (see MAX_MERGEABLE_BYTES
        // above) without reading them. `entry.metadata()` reuses the stat
        // walkdir already did for the directory walk on most platforms.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() == 0 || metadata.len() > MAX_MERGEABLE_BYTES {
            continue;
        }

        // Best-effort text read: non-UTF8/binary content can't hold a
        // meaningful marker triple by definition, so a read failure here is
        // not an error -- it just means "no inline markers to find".
        if let Ok(content) = std::fs::read_to_string(abs_path) {
            if has_conflict_markers(&content) {
                results.push(MarkerConflict {
                    rel_path: rel_str,
                    kind: MarkerConflictKind::InlineMarkers,
                });
            }
        }
    }

    Ok(results)
}

/// Compute blake3 hash of bytes already in memory, in the same hex form as
/// `hash_file`, so a downloaded body can be compared against a `local_hash`.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Compute blake3 hash of a file, returning hex string.
///
/// Runs synchronously — call from `spawn_blocking` if needed in async context.
pub fn hash_file(path: &Path) -> SbResult<String> {
    let mut file = std::fs::File::open(path).map_err(|e| SbError::Filesystem {
        message: "cannot open file for hashing".into(),
        path: path.display().to_string(),
        source: Some(e),
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 65536]; // 64 KiB buffer
    loop {
        let n = file.read(&mut buf).map_err(|e| SbError::Filesystem {
            message: "error reading file for hashing".into(),
            path: path.display().to_string(),
            source: Some(e),
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Scans local space directory, skipping .sb/ and respecting filter patterns.
pub struct LocalScanner {
    filter: FileFilter,
}

impl LocalScanner {
    pub fn new(filter: FileFilter) -> Self {
        LocalScanner { filter }
    }

    /// Scan the space content directory and return info for all matching files.
    ///
    /// Runs synchronously (filesystem + hashing are blocking I/O).
    ///
    /// The caller is responsible for pointing `space_root` at the dedicated
    /// content directory (e.g. `<project>/<sync.dir>`), so no repo-infrastructure
    /// filtering is needed here.
    ///
    /// Exclusion:
    /// 1. `.sb/` is always excluded.
    /// 2. Glob-based include/exclude patterns from `FileFilter` (include overrides exclude).
    pub fn scan(&self, space_root: &Path) -> SbResult<Vec<LocalFileInfo>> {
        let mut results = Vec::new();

        let walker = WalkDir::new(space_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // Always skip .sb/ at any depth
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    if name == ".sb" {
                        return false;
                    }
                }
                true
            });

        for result in walker {
            let entry = result.map_err(|e| SbError::Filesystem {
                message: format!("error walking directory: {e}"),
                path: space_root.display().to_string(),
                source: None,
            })?;

            // Skip non-files (directories, symlinks to directories, etc.)
            if !entry.file_type().is_file() {
                continue;
            }

            let abs_path = entry.path();
            let rel_path = abs_path
                .strip_prefix(space_root)
                .map_err(|_| SbError::Filesystem {
                    message: "cannot compute relative path".into(),
                    path: abs_path.display().to_string(),
                    source: None,
                })?;
            // Normalize to forward slashes for cross-platform consistency
            let rel_str = rel_path.to_string_lossy().replace('\\', "/");

            // Apply glob filter (include overrides exclude)
            if !self.filter.should_sync(&rel_str) {
                continue;
            }

            let hash = hash_file(abs_path)?;
            let metadata = std::fs::metadata(abs_path).map_err(|e| SbError::Filesystem {
                message: "cannot read file metadata".into(),
                path: abs_path.display().to_string(),
                source: Some(e),
            })?;
            let mtime_ms = mtime_ms(&metadata);
            let size = metadata.len();

            results.push(LocalFileInfo {
                rel_path: rel_str,
                hash,
                mtime_ms,
                size,
            });
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- FileFilter tests ---

    #[test]
    fn file_filter_empty_excludes_and_includes_accepts_all() {
        let filter = FileFilter::new(&[], &[], false).expect("create filter");
        assert!(filter.should_sync("Journal/note.md"));
        assert!(filter.should_sync("index.md"));
        assert!(filter.should_sync("deep/path/to/note.md"));
    }

    #[test]
    fn file_filter_exclude_plug_rejects_plug_files() {
        let excludes = vec!["_plug/*".to_string()];
        let filter = FileFilter::new(&excludes, &[], false).expect("create filter");
        assert!(
            !filter.should_sync("_plug/core.js"),
            "should reject _plug/core.js"
        );
        assert!(
            !filter.should_sync("_plug/search.js"),
            "should reject _plug/search.js"
        );
    }

    #[test]
    fn file_filter_exclude_plug_accepts_other_files() {
        let excludes = vec!["_plug/*".to_string()];
        let filter = FileFilter::new(&excludes, &[], false).expect("create filter");
        assert!(
            filter.should_sync("Journal/note.md"),
            "should accept Journal/note.md"
        );
        assert!(filter.should_sync("index.md"), "should accept index.md");
    }

    #[test]
    fn file_filter_include_overrides_exclude_sync23() {
        let excludes = vec!["*.tmp".to_string()];
        let includes = vec!["important.tmp".to_string()];
        let filter = FileFilter::new(&excludes, &includes, false).expect("create filter");
        // include overrides exclude
        assert!(
            filter.should_sync("important.tmp"),
            "include should override exclude"
        );
        // other .tmp files are still excluded
        assert!(
            !filter.should_sync("temp.tmp"),
            "non-included .tmp should be excluded"
        );
    }

    #[test]
    fn file_filter_always_rejects_sb_directory_sync21() {
        // .sb/ is rejected even with no exclude patterns
        let filter = FileFilter::new(&[], &[], false).expect("create filter");
        assert!(
            !filter.should_sync(".sb/config.toml"),
            "should always reject .sb/"
        );
        assert!(
            !filter.should_sync(".sb/state.db"),
            "should always reject .sb/"
        );
    }

    #[test]
    fn file_filter_sb_directory_rejected_even_with_include() {
        // include patterns cannot override .sb/ exclusion
        let includes = vec![".sb/*".to_string(), ".sb/config.toml".to_string()];
        let filter = FileFilter::new(&[], &includes, false).expect("create filter");
        assert!(
            !filter.should_sync(".sb/config.toml"),
            ".sb/ cannot be included"
        );
        assert!(
            !filter.should_sync(".sb/state.db"),
            ".sb/ cannot be included"
        );
    }

    // --- hash_file tests ---

    #[test]
    fn hash_file_returns_consistent_blake3_for_known_content() {
        let dir = TempDir::new().expect("create tempdir");
        let path = dir.path().join("test.md");
        fs::write(&path, b"hello world").expect("write file");

        let hash1 = hash_file(&path).expect("hash file");
        let hash2 = hash_file(&path).expect("hash file again");

        assert_eq!(hash1, hash2, "same content should produce same hash");
        // blake3 hex is 64 chars
        assert_eq!(hash1.len(), 64, "blake3 hex should be 64 chars");
    }

    #[test]
    fn hash_file_different_content_produces_different_hash() {
        let dir = TempDir::new().expect("create tempdir");
        let path1 = dir.path().join("a.md");
        let path2 = dir.path().join("b.md");
        fs::write(&path1, b"content A").expect("write a");
        fs::write(&path2, b"content B").expect("write b");

        let hash1 = hash_file(&path1).expect("hash a");
        let hash2 = hash_file(&path2).expect("hash b");

        assert_ne!(
            hash1, hash2,
            "different content should produce different hashes"
        );
    }

    // --- LocalScanner tests ---

    fn make_scanner(excludes: &[&str], includes: &[&str]) -> LocalScanner {
        let ex: Vec<String> = excludes.iter().map(|s| s.to_string()).collect();
        let inc: Vec<String> = includes.iter().map(|s| s.to_string()).collect();
        let filter = FileFilter::new(&ex, &inc, false).expect("create filter");
        LocalScanner::new(filter)
    }

    #[test]
    fn scanner_returns_correct_relative_paths() {
        let dir = TempDir::new().expect("create tempdir");
        fs::write(dir.path().join("note.md"), b"note content").expect("write note");
        fs::create_dir(dir.path().join("Journal")).expect("mkdir Journal");
        fs::write(dir.path().join("Journal/2026-04-05.md"), b"daily note").expect("write daily");

        let scanner = make_scanner(&[], &[]);
        let mut results = scanner.scan(dir.path()).expect("scan");
        results.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

        let paths: Vec<&str> = results.iter().map(|r| r.rel_path.as_str()).collect();
        assert!(
            paths.contains(&"note.md"),
            "should include note.md, got: {paths:?}"
        );
        assert!(
            paths.contains(&"Journal/2026-04-05.md"),
            "should include Journal/2026-04-05.md, got: {paths:?}"
        );
    }

    #[test]
    fn scanner_skips_sb_directory_unconditionally() {
        let dir = TempDir::new().expect("create tempdir");
        // Create a file outside .sb/
        fs::write(dir.path().join("note.md"), b"note").expect("write note");
        // Create .sb/ directory with files that should be skipped
        let sb_dir = dir.path().join(".sb");
        fs::create_dir(&sb_dir).expect("mkdir .sb");
        fs::write(sb_dir.join("config.toml"), b"[config]").expect("write config");
        fs::write(sb_dir.join("state.db"), b"sqlite").expect("write state.db");

        let scanner = make_scanner(&[], &[]);
        let results = scanner.scan(dir.path()).expect("scan");

        let paths: Vec<&str> = results.iter().map(|r| r.rel_path.as_str()).collect();
        assert!(
            !paths.iter().any(|p| p.starts_with(".sb/")),
            "no .sb/ paths, got: {paths:?}"
        );
        assert!(paths.contains(&"note.md"), "should include note.md");
    }

    #[test]
    fn scanner_skips_files_matching_exclude_patterns() {
        let dir = TempDir::new().expect("create tempdir");
        fs::create_dir(dir.path().join("_plug")).expect("mkdir _plug");
        fs::write(dir.path().join("_plug/core.js"), b"plugin code").expect("write plugin");
        fs::write(dir.path().join("note.md"), b"note content").expect("write note");

        let scanner = make_scanner(&["_plug/*"], &[]);
        let results = scanner.scan(dir.path()).expect("scan");

        let paths: Vec<&str> = results.iter().map(|r| r.rel_path.as_str()).collect();
        assert!(
            !paths.contains(&"_plug/core.js"),
            "_plug/core.js should be excluded"
        );
        assert!(paths.contains(&"note.md"), "note.md should be included");
    }

    #[test]
    fn scanner_includes_files_matching_include_even_if_excluded() {
        let dir = TempDir::new().expect("create tempdir");
        fs::write(dir.path().join("temp.tmp"), b"temp file").expect("write temp");
        fs::write(dir.path().join("important.tmp"), b"important temp").expect("write important");
        fs::write(dir.path().join("note.md"), b"note content").expect("write note");

        let scanner = make_scanner(&["*.tmp"], &["important.tmp"]);
        let results = scanner.scan(dir.path()).expect("scan");

        let paths: Vec<&str> = results.iter().map(|r| r.rel_path.as_str()).collect();
        assert!(
            paths.contains(&"important.tmp"),
            "important.tmp should be included via include"
        );
        assert!(!paths.contains(&"temp.tmp"), "temp.tmp should be excluded");
        assert!(paths.contains(&"note.md"), "note.md should be included");
    }

    #[test]
    fn scanner_computes_hash_and_captures_mtime() {
        let dir = TempDir::new().expect("create tempdir");
        let content = b"test content for hashing";
        fs::write(dir.path().join("note.md"), content).expect("write note");

        let scanner = make_scanner(&[], &[]);
        let results = scanner.scan(dir.path()).expect("scan");

        assert_eq!(results.len(), 1);
        let info = &results[0];

        // Verify hash is a valid blake3 hex (64 chars)
        assert_eq!(info.hash.len(), 64, "hash should be 64 hex chars");
        assert!(
            info.hash.chars().all(|c| c.is_ascii_hexdigit()),
            "hash should be hex"
        );

        // Verify hash matches direct hash_file call
        let expected_hash = hash_file(&dir.path().join("note.md")).expect("hash file");
        assert_eq!(
            info.hash, expected_hash,
            "scan hash should match direct hash_file"
        );

        // mtime_ms should be non-zero (file was just created)
        assert!(info.mtime_ms > 0, "mtime_ms should be positive");
    }

    #[test]
    fn scanner_empty_directory_returns_empty_vec() {
        let dir = TempDir::new().expect("create tempdir");

        let scanner = make_scanner(&[], &[]);
        let results = scanner.scan(dir.path()).expect("scan");

        assert!(
            results.is_empty(),
            "empty directory should return empty vec"
        );
    }

    // --- attachments_enabled tests ---

    #[test]
    fn file_filter_rejects_non_md_when_attachments_false() {
        let filter = FileFilter::new(&[], &[], false).expect("create filter");
        assert!(
            !filter.should_sync("image.png"),
            "should reject image.png when attachments=false"
        );
        assert!(
            !filter.should_sync("_attachments/photo.jpg"),
            "should reject _attachments/photo.jpg when attachments=false"
        );
        assert!(
            filter.should_sync("notes/page.md"),
            "should accept .md files when attachments=false"
        );
    }

    #[test]
    fn file_filter_accepts_non_md_when_attachments_true() {
        let filter = FileFilter::new(&[], &[], true).expect("create filter");
        assert!(
            filter.should_sync("image.png"),
            "should accept image.png when attachments=true"
        );
        assert!(
            filter.should_sync("_attachments/photo.jpg"),
            "should accept _attachments/photo.jpg when attachments=true"
        );
        assert!(
            filter.should_sync("notes/page.md"),
            "should accept .md files when attachments=true"
        );
    }

    #[test]
    fn file_filter_attachments_true_still_rejects_sb_dir() {
        let filter = FileFilter::new(&[], &[], true).expect("create filter");
        assert!(
            !filter.should_sync(".sb/config.toml"),
            ".sb/ must always be rejected even with attachments=true"
        );
        assert!(
            !filter.should_sync(".sb/state.db"),
            ".sb/ must always be rejected even with attachments=true"
        );
    }

    #[test]
    fn file_filter_attachments_true_still_respects_excludes() {
        let excludes = vec!["*.tmp".to_string()];
        let filter = FileFilter::new(&excludes, &[], true).expect("create filter");
        assert!(
            !filter.should_sync("temp.tmp"),
            "*.tmp should still be excluded when attachments=true"
        );
        assert!(
            filter.should_sync("image.png"),
            "image.png should be accepted when attachments=true and not excluded"
        );
    }

    // --- has_conflict_markers ---

    #[test]
    fn has_conflict_markers_detects_sb_style_marker_triple() {
        let content = "before\n<<<<<<< SB sha256:1a2b3c4d\nours\n=======\ntheirs\n>>>>>>> SB sha256:5e6f7a8b\nafter\n";
        assert!(has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_detects_plain_git_markers() {
        let content = "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch-name\n";
        assert!(has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_ignores_fenced_code_block_example() {
        // This is exactly the shape of the example in this feature's own spec:
        // a real-looking marker triple, but inside a fenced code block used to
        // document conflict markers -- must NOT be flagged.
        let content = "Here's what a conflict looks like:\n\n\
            ```\n\
            <<<<<<< SB sha256:1a2b3c4d\n\
            your version of the line\n\
            =======\n\
            their version of the line\n\
            >>>>>>> SB sha256:5e6f7a8b\n\
            ```\n\
            \nEdit the file to resolve it.\n";
        assert!(
            !has_conflict_markers(content),
            "fenced example must not be flagged as a real conflict"
        );
    }

    #[test]
    fn has_conflict_markers_ignores_tilde_fenced_block() {
        let content = "~~~\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n~~~\n";
        assert!(!has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_ignores_indented_example() {
        // 4-space indented markdown code block -- markers aren't at column 0.
        let content =
            "Example:\n\n    <<<<<<< SB sha256:aaa\n    ours\n    =======\n    theirs\n    >>>>>>> SB sha256:bbb\n";
        assert!(!has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_ignores_lone_marker_mention() {
        // A page that just talks about markers without a full triple.
        let content = "Conflict markers start with <<<<<<< and end with >>>>>>>.\n";
        assert!(!has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_ignores_open_without_close() {
        let content = "<<<<<<< HEAD\nours\n=======\ntheirs\nno closing marker here\n";
        assert!(!has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_ignores_out_of_order_markers() {
        // Separator appears before the opening marker -- not a valid triple.
        let content = "=======\n<<<<<<< HEAD\nours\n>>>>>>> branch\n";
        assert!(!has_conflict_markers(content));
    }

    #[test]
    fn has_conflict_markers_detects_triple_after_backtick_block_containing_tilde_line() {
        // A ``` block whose body contains a ~~~ line must NOT toggle the
        // backtick fence off early -- only a closing ``` closes it. A real
        // triple right after the block must still be detected.
        let content = "```\nbody\n~~~\nmore body\n```\n<<<<<<< SB sha256:aaa\nours\n=======\ntheirs\n>>>>>>> SB sha256:bbb\n";
        assert!(
            has_conflict_markers(content),
            "real triple after a fence containing a stray ~~~ must be detected"
        );
    }

    #[test]
    fn has_conflict_markers_detects_triple_after_unbalanced_fence() {
        // A single unclosed ``` line (e.g. a truncated paste) must not blind
        // the rest of the file -- an opener with no matching closer by EOF
        // is left unmarked, so the real triple after it is still scanned.
        let content =
            "```\ntruncated paste, no closing fence\n<<<<<<< SB sha256:aaa\nours\n=======\ntheirs\n>>>>>>> SB sha256:bbb\n";
        assert!(
            has_conflict_markers(content),
            "real triple after an unbalanced fence must be detected"
        );
    }

    #[test]
    fn has_conflict_markers_ignores_prose_mentioning_markers_in_separate_sections() {
        // A git how-to page: each marker appears at column 0, unfenced, but
        // in its own section separated by blank lines/headers -- not a real
        // contiguous triple.
        let content = "\
# Understanding conflict markers

SilverBullet writes conflict markers into a file it can't auto-merge.

## The opening marker

<<<<<<< SB sha256:1a2b3c4d

This marks the start of your version of the content.

## The separator

=======

This separates your version from the server's version.

## The closing marker

>>>>>>> SB sha256:5e6f7a8b

Resolve the conflict by editing the file to keep the parts you want.
";
        assert!(
            !has_conflict_markers(content),
            "markers scattered across separate prose sections must not be flagged"
        );
    }

    #[test]
    fn has_conflict_markers_clean_file_is_not_flagged() {
        assert!(!has_conflict_markers(
            "# Just a normal page\n\nSome text.\n"
        ));
    }

    // --- is_conflicted_sibling_name ---

    #[test]
    fn is_conflicted_sibling_name_matches_with_extension() {
        assert!(is_conflicted_sibling_name("photo.conflicted-a1b2c3.jpg"));
    }

    #[test]
    fn is_conflicted_sibling_name_matches_without_extension() {
        assert!(is_conflicted_sibling_name("data.conflicted-deadbeef"));
    }

    #[test]
    fn is_conflicted_sibling_name_rejects_plain_files() {
        assert!(!is_conflicted_sibling_name("note.md"));
        assert!(!is_conflicted_sibling_name("photo.jpg"));
    }

    #[test]
    fn is_conflicted_sibling_name_rejects_empty_stem_or_hash() {
        assert!(!is_conflicted_sibling_name(".conflicted-abc.jpg"));
        assert!(!is_conflicted_sibling_name("photo.conflicted-.jpg"));
    }

    #[test]
    fn is_conflicted_sibling_name_rejects_non_hex_token() {
        // An ordinary page named with ".conflicted-" followed by a word,
        // not a hash, must not be mistaken for a real conflict sibling.
        assert!(!is_conflicted_sibling_name("My.conflicted-notes.md"));
    }

    #[test]
    fn is_conflicted_sibling_name_rejects_short_hex_token() {
        // Too short to plausibly be a real digest.
        assert!(!is_conflicted_sibling_name("photo.conflicted-ab.jpg"));
    }

    // --- scan_marker_conflicts ---

    #[test]
    fn scan_marker_conflicts_flags_file_with_markers_not_clean_neighbour() {
        let dir = TempDir::new().expect("create tempdir");
        fs::write(
            dir.path().join("Conflicted.md"),
            "<<<<<<< SB sha256:aaa\nours\n=======\ntheirs\n>>>>>>> SB sha256:bbb\n",
        )
        .expect("write conflicted");
        fs::write(dir.path().join("Clean.md"), "# all good\n").expect("write clean");

        let results = scan_marker_conflicts(dir.path()).expect("scan");
        assert_eq!(
            results.len(),
            1,
            "only the conflicted file should be flagged"
        );
        assert_eq!(results[0].rel_path, "Conflicted.md");
        assert_eq!(results[0].kind, MarkerConflictKind::InlineMarkers);
    }

    #[test]
    fn scan_marker_conflicts_flags_plain_git_markers() {
        let dir = TempDir::new().expect("create tempdir");
        fs::write(
            dir.path().join("GitConflict.md"),
            "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> feature-branch\n",
        )
        .expect("write");

        let results = scan_marker_conflicts(dir.path()).expect("scan");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, MarkerConflictKind::InlineMarkers);
    }

    #[test]
    fn scan_marker_conflicts_ignores_fenced_documentation_example() {
        let dir = TempDir::new().expect("create tempdir");
        fs::write(
            dir.path().join("GitDocs.md"),
            "# Resolving conflicts\n\n```\n<<<<<<< SB sha256:1a2b3c4d\nyour version of the line\n=======\ntheir version of the line\n>>>>>>> SB sha256:5e6f7a8b\n```\n",
        )
        .expect("write docs page");

        let results = scan_marker_conflicts(dir.path()).expect("scan");
        assert!(
            results.is_empty(),
            "documentation page with fenced example must not be flagged, got: {results:?}"
        );
    }

    #[test]
    fn scan_marker_conflicts_lists_conflicted_sibling_files() {
        let dir = TempDir::new().expect("create tempdir");
        fs::write(dir.path().join("photo.jpg"), b"jpegbytes").expect("write photo");
        fs::write(
            dir.path().join("photo.conflicted-a1b2c3.jpg"),
            b"other jpeg bytes",
        )
        .expect("write sibling");

        let results = scan_marker_conflicts(dir.path()).expect("scan");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].rel_path, "photo.conflicted-a1b2c3.jpg");
        assert_eq!(results[0].kind, MarkerConflictKind::ConflictedSibling);
    }

    #[test]
    fn scan_marker_conflicts_skips_sb_directory() {
        let dir = TempDir::new().expect("create tempdir");
        let sb_dir = dir.path().join(".sb");
        fs::create_dir(&sb_dir).expect("mkdir .sb");
        fs::write(
            sb_dir.join("Fake.md"),
            "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> z\n",
        )
        .expect("write inside .sb");

        let results = scan_marker_conflicts(dir.path()).expect("scan");
        assert!(results.is_empty(), ".sb/ must never be scanned");
    }

    #[test]
    fn scan_marker_conflicts_empty_space_returns_empty_vec() {
        let dir = TempDir::new().expect("create tempdir");
        let results = scan_marker_conflicts(dir.path()).expect("scan");
        assert!(results.is_empty());
    }

    #[test]
    fn scan_marker_conflicts_does_not_scan_large_binary_file() {
        // A file well over SB's 1 MiB merge limit can never contain a
        // server-written marker triple. Bury a well-formed triple inside
        // several MB of filler bytes -- if the scanner were still slurping
        // and scanning whole files, this would be flagged; the size cap
        // must skip the read (and thus the detection) entirely.
        let dir = TempDir::new().expect("create tempdir");
        let mut content = vec![b'x'; 5 * 1024 * 1024];
        let marker = b"\n<<<<<<< SB sha256:aaa\nours\n=======\ntheirs\n>>>>>>> SB sha256:bbb\n";
        content.extend_from_slice(marker);
        fs::write(dir.path().join("huge.md"), &content).expect("write huge file");
        fs::write(dir.path().join("Clean.md"), "# fine\n").expect("write clean");

        let results = scan_marker_conflicts(dir.path()).expect("scan");
        assert!(
            results.is_empty(),
            "file over the merge-size cap must not be read or flagged, got: {results:?}"
        );
    }
}
