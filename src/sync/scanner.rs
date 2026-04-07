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
}
