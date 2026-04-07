use std::path::Path;

use rusqlite::{params, Connection};

use crate::error::{SbError, SbResult};

use super::{SyncResult, SyncStateRow, SyncStatus};

/// SQLite-backed store for sync state tracking.
///
/// NOT Send — all operations must run inside `tokio::task::spawn_blocking`
/// when called from async code. Tests run on the test thread directly.
pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Open (or create) state.db at `path` and ensure schema is up to date.
    ///
    /// Enables WAL journal mode for crash safety.
    pub fn open(path: &Path) -> SbResult<Self> {
        let conn = Connection::open(path).map_err(|e| SbError::Database {
            message: format!("failed to open state.db: {e}"),
            source: Some(e),
        })?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(|e| SbError::Database {
                message: format!("failed to set WAL mode: {e}"),
                source: Some(e),
            })?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_state (
                path TEXT PRIMARY KEY NOT NULL,
                local_hash TEXT,
                remote_hash TEXT,
                remote_mtime INTEGER NOT NULL DEFAULT 0,
                local_mtime INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL DEFAULT 'synced',
                conflict_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS sync_meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );",
        )
        .map_err(|e| SbError::Database {
            message: format!("failed to create sync tables: {e}"),
            source: Some(e),
        })?;

        // Migration: add conflict_at column on existing databases.
        // ALTER TABLE ADD COLUMN errors on older SQLite if column already exists,
        // so we check via PRAGMA table_info first.
        let has_conflict_at: bool = conn
            .prepare("PRAGMA table_info(sync_state)")
            .map_err(|e| SbError::Database {
                message: format!("failed to check sync_state schema: {e}"),
                source: Some(e),
            })?
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| SbError::Database {
                message: format!("failed to read table_info: {e}"),
                source: Some(e),
            })?
            .any(|col| col.is_ok_and(|name| name == "conflict_at"));

        if !has_conflict_at {
            conn.execute_batch(
                "ALTER TABLE sync_state ADD COLUMN conflict_at INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(|e| SbError::Database {
                message: format!("failed to add conflict_at column: {e}"),
                source: Some(e),
            })?;
        }

        Ok(StateDb { conn })
    }

    /// Insert or replace a sync state row.
    ///
    /// All SQL uses parameterized queries — no string concatenation.
    pub fn upsert_row(&self, row: &SyncStateRow) -> SbResult<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO sync_state
                 (path, local_hash, remote_hash, remote_mtime, local_mtime, status, conflict_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.path,
                    row.local_hash,
                    row.remote_hash,
                    row.remote_mtime,
                    row.local_mtime,
                    row.status.as_str(),
                    row.conflict_at,
                ],
            )
            .map_err(|e| SbError::Database {
                message: format!("failed to upsert sync state row: {e}"),
                source: Some(e),
            })?;
        Ok(())
    }

    /// Retrieve a single row by path. Returns `None` if not tracked.
    pub fn get_row(&self, path: &str) -> SbResult<Option<SyncStateRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT path, local_hash, remote_hash, remote_mtime, local_mtime, status, conflict_at
                 FROM sync_state WHERE path = ?1",
            )
            .map_err(|e| SbError::Database {
                message: format!("failed to prepare get_row query: {e}"),
                source: Some(e),
            })?;

        let mut rows =
            stmt.query_map(params![path], row_mapper)
                .map_err(|e| SbError::Database {
                    message: format!("failed to query sync state row: {e}"),
                    source: Some(e),
                })?;

        match rows.next() {
            Some(row) => Ok(Some(row.map_err(|e| SbError::Database {
                message: format!("failed to read sync state row: {e}"),
                source: Some(e),
            })?)),
            None => Ok(None),
        }
    }

    /// Retrieve all rows from sync_state.
    pub fn get_all_rows(&self) -> SbResult<Vec<SyncStateRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT path, local_hash, remote_hash, remote_mtime, local_mtime, status, conflict_at
                 FROM sync_state",
            )
            .map_err(|e| SbError::Database {
                message: format!("failed to prepare get_all_rows query: {e}"),
                source: Some(e),
            })?;

        let rows = stmt
            .query_map([], row_mapper)
            .map_err(|e| SbError::Database {
                message: format!("failed to query all sync state rows: {e}"),
                source: Some(e),
            })?;

        rows.map(|r| {
            r.map_err(|e| SbError::Database {
                message: format!("failed to read sync state row: {e}"),
                source: Some(e),
            })
        })
        .collect()
    }

    /// Retrieve rows with a specific status (e.g., `SyncStatus::Conflict`).
    pub fn get_rows_by_status(&self, status: &SyncStatus) -> SbResult<Vec<SyncStateRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT path, local_hash, remote_hash, remote_mtime, local_mtime, status, conflict_at
                 FROM sync_state WHERE status = ?1",
            )
            .map_err(|e| SbError::Database {
                message: format!("failed to prepare get_rows_by_status query: {e}"),
                source: Some(e),
            })?;

        let rows = stmt
            .query_map(params![status.as_str()], row_mapper)
            .map_err(|e| SbError::Database {
                message: format!("failed to query sync state by status: {e}"),
                source: Some(e),
            })?;

        rows.map(|r| {
            r.map_err(|e| SbError::Database {
                message: format!("failed to read sync state row: {e}"),
                source: Some(e),
            })
        })
        .collect()
    }

    /// Delete a row by path. No-op if path not tracked.
    pub fn delete_row(&self, path: &str) -> SbResult<()> {
        self.conn
            .execute("DELETE FROM sync_state WHERE path = ?1", params![path])
            .map_err(|e| SbError::Database {
                message: format!("failed to delete sync state row: {e}"),
                source: Some(e),
            })?;
        Ok(())
    }

    /// Atomically commit a batch of sync results.
    ///
    /// All writes happen inside a single SQLite transaction.
    /// On crash or interrupt, the transaction rolls back leaving state.db consistent.
    pub fn commit_batch(&mut self, results: &[SyncResult]) -> SbResult<()> {
        let tx = self.conn.transaction().map_err(|e| SbError::Database {
            message: format!("failed to begin transaction: {e}"),
            source: Some(e),
        })?;

        for result in results {
            match result {
                SyncResult::Synced {
                    path,
                    local_hash,
                    remote_hash,
                    remote_mtime,
                    local_mtime,
                } => {
                    tx.execute(
                        "INSERT OR REPLACE INTO sync_state
                         (path, local_hash, remote_hash, remote_mtime, local_mtime, status, conflict_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'synced', 0)",
                        params![path, local_hash, remote_hash, remote_mtime, local_mtime],
                    )
                    .map_err(|e| SbError::Database {
                        message: format!("failed to write synced row in batch: {e}"),
                        source: Some(e),
                    })?;
                }
                SyncResult::Conflict { path, conflict_at } => {
                    tx.execute(
                        "UPDATE sync_state SET status = 'conflict', conflict_at = ?2 WHERE path = ?1",
                        params![path, conflict_at],
                    )
                    .map_err(|e| SbError::Database {
                        message: format!("failed to write conflict row in batch: {e}"),
                        source: Some(e),
                    })?;
                }
                SyncResult::Deleted { path } => {
                    tx.execute("DELETE FROM sync_state WHERE path = ?1", params![path])
                        .map_err(|e| SbError::Database {
                            message: format!("failed to delete row in batch: {e}"),
                            source: Some(e),
                        })?;
                }
            }
        }

        tx.commit().map_err(|e| SbError::Database {
            message: format!("failed to commit transaction: {e}"),
            source: Some(e),
        })?;

        Ok(())
    }

    /// Atomically resolve a conflict by setting status to synced with new hashes.
    ///
    /// Must be called from spawn_blocking. Filesystem stash cleanup is the caller's
    /// responsibility (cannot do async I/O inside spawn_blocking).
    pub fn mark_resolved(
        &mut self,
        path: &str,
        local_hash: &str,
        remote_hash: &str,
        remote_mtime: i64,
        local_mtime: i64,
    ) -> SbResult<()> {
        // Verify the row exists and is in conflict status
        let existing = self.get_row(path)?;
        match existing {
            None => {
                return Err(SbError::Database {
                    message: format!("cannot resolve: '{}' is not tracked in state.db", path),
                    source: None,
                });
            }
            Some(row) if row.status != SyncStatus::Conflict => {
                return Err(SbError::Database {
                    message: format!(
                        "cannot resolve: '{}' is in '{}' status, not 'conflict'",
                        path,
                        row.status.as_str()
                    ),
                    source: None,
                });
            }
            _ => {}
        }

        let tx = self.conn.transaction().map_err(|e| SbError::Database {
            message: format!("failed to begin transaction: {e}"),
            source: Some(e),
        })?;

        tx.execute(
            "UPDATE sync_state SET local_hash = ?2, remote_hash = ?3, remote_mtime = ?4,
             local_mtime = ?5, status = 'synced', conflict_at = 0 WHERE path = ?1",
            params![path, local_hash, remote_hash, remote_mtime, local_mtime],
        )
        .map_err(|e| SbError::Database {
            message: format!("failed to update resolved row: {e}"),
            source: Some(e),
        })?;

        tx.commit().map_err(|e| SbError::Database {
            message: format!("failed to commit resolve transaction: {e}"),
            source: Some(e),
        })?;

        Ok(())
    }

    /// Store a key-value pair in sync_meta (e.g., last_sync timestamp).
    pub fn set_meta(&self, key: &str, value: &str) -> SbResult<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO sync_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(|e| SbError::Database {
                message: format!("failed to set sync meta key '{key}': {e}"),
                source: Some(e),
            })?;
        Ok(())
    }

    /// Retrieve a value from sync_meta. Returns `None` if key not set.
    pub fn get_meta(&self, key: &str) -> SbResult<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT value FROM sync_meta WHERE key = ?1")
            .map_err(|e| SbError::Database {
                message: format!("failed to prepare get_meta query: {e}"),
                source: Some(e),
            })?;

        let mut rows = stmt
            .query_map(params![key], |row| row.get::<_, String>(0))
            .map_err(|e| SbError::Database {
                message: format!("failed to query sync meta: {e}"),
                source: Some(e),
            })?;

        match rows.next() {
            Some(val) => Ok(Some(val.map_err(|e| SbError::Database {
                message: format!("failed to read sync meta value: {e}"),
                source: Some(e),
            })?)),
            None => Ok(None),
        }
    }
}

/// Map a rusqlite Row to a SyncStateRow.
fn row_mapper(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncStateRow> {
    Ok(SyncStateRow {
        path: row.get(0)?,
        local_hash: row.get(1)?,
        remote_hash: row.get(2)?,
        remote_mtime: row.get(3)?,
        local_mtime: row.get(4)?,
        status: {
            let s: String = row.get(5)?;
            s.parse::<SyncStatus>().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.to_string(),
                    )),
                )
            })?
        },
        conflict_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn open_temp_db() -> (StateDb, NamedTempFile) {
        let tmp = NamedTempFile::new().expect("create temp file");
        let db = StateDb::open(tmp.path()).expect("open StateDb");
        (db, tmp)
    }

    fn make_row(path: &str) -> SyncStateRow {
        SyncStateRow {
            path: path.to_string(),
            local_hash: Some("abc123".to_string()),
            remote_hash: Some("def456".to_string()),
            remote_mtime: 1700000000000,
            local_mtime: 1700000001000,
            status: SyncStatus::Synced,
            conflict_at: 0,
        }
    }

    #[test]
    fn open_creates_sync_state_and_sync_meta_tables() {
        let (_db, tmp) = open_temp_db();
        // Reopen to verify tables persist
        let conn = rusqlite::Connection::open(tmp.path()).expect("reopen db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('sync_state', 'sync_meta')",
                [],
                |r| r.get(0),
            )
            .expect("query table count");
        assert_eq!(
            count, 2,
            "both sync_state and sync_meta tables should exist"
        );
    }

    #[test]
    fn sync_state_has_correct_columns() {
        let (_db, tmp) = open_temp_db();
        let conn = rusqlite::Connection::open(tmp.path()).expect("reopen db");
        // PRAGMA table_info returns: cid, name, type, notnull, dflt_value, pk
        let mut stmt = conn
            .prepare("PRAGMA table_info(sync_state)")
            .expect("prepare pragma");
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .expect("query cols")
            .map(|r| r.expect("col name"))
            .collect();
        assert!(cols.contains(&"path".to_string()));
        assert!(cols.contains(&"local_hash".to_string()));
        assert!(cols.contains(&"remote_hash".to_string()));
        assert!(cols.contains(&"remote_mtime".to_string()));
        assert!(cols.contains(&"local_mtime".to_string()));
        assert!(cols.contains(&"status".to_string()));
    }

    #[test]
    fn wal_journal_mode_enabled_after_open() {
        let (_db, tmp) = open_temp_db();
        let conn = rusqlite::Connection::open(tmp.path()).expect("reopen db");
        let mode: String = conn
            .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
            .expect("query journal_mode");
        assert_eq!(mode, "wal", "WAL journal mode should be enabled");
    }

    #[test]
    fn upsert_row_inserts_and_get_row_retrieves_it() {
        let (db, _tmp) = open_temp_db();
        let row = make_row("notes/page.md");
        db.upsert_row(&row).expect("upsert_row");

        let retrieved = db
            .get_row("notes/page.md")
            .expect("get_row")
            .expect("row should exist");
        assert_eq!(retrieved.path, "notes/page.md");
        assert_eq!(retrieved.local_hash, Some("abc123".to_string()));
        assert_eq!(retrieved.remote_hash, Some("def456".to_string()));
        assert_eq!(retrieved.remote_mtime, 1700000000000i64);
        assert_eq!(retrieved.local_mtime, 1700000001000i64);
        assert_eq!(retrieved.status, SyncStatus::Synced);
    }

    #[test]
    fn get_row_returns_none_for_nonexistent_path() {
        let (db, _tmp) = open_temp_db();
        let result = db
            .get_row("does/not/exist.md")
            .expect("get_row should not error");
        assert!(result.is_none(), "nonexistent path should return None");
    }

    #[test]
    fn get_all_rows_returns_all_rows() {
        let (db, _tmp) = open_temp_db();
        db.upsert_row(&make_row("page1.md")).expect("upsert 1");
        db.upsert_row(&make_row("page2.md")).expect("upsert 2");
        db.upsert_row(&make_row("page3.md")).expect("upsert 3");

        let rows = db.get_all_rows().expect("get_all_rows");
        assert_eq!(rows.len(), 3, "should return all 3 rows");
        let paths: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"page1.md"));
        assert!(paths.contains(&"page2.md"));
        assert!(paths.contains(&"page3.md"));
    }

    #[test]
    fn get_rows_by_status_filters_correctly() {
        let (db, _tmp) = open_temp_db();
        let mut row1 = make_row("page1.md");
        row1.status = SyncStatus::Conflict;
        let mut row2 = make_row("page2.md");
        row2.status = SyncStatus::Synced;
        let mut row3 = make_row("page3.md");
        row3.status = SyncStatus::Conflict;

        db.upsert_row(&row1).expect("upsert 1");
        db.upsert_row(&row2).expect("upsert 2");
        db.upsert_row(&row3).expect("upsert 3");

        let conflicts = db
            .get_rows_by_status(&SyncStatus::Conflict)
            .expect("get_rows_by_status");
        assert_eq!(conflicts.len(), 2, "should return 2 conflict rows");
        assert!(conflicts.iter().all(|r| r.status == SyncStatus::Conflict));

        let synced = db
            .get_rows_by_status(&SyncStatus::Synced)
            .expect("get_rows_by_status synced");
        assert_eq!(synced.len(), 1);
    }

    #[test]
    fn delete_row_removes_the_row() {
        let (db, _tmp) = open_temp_db();
        db.upsert_row(&make_row("notes/page.md")).expect("upsert");
        db.delete_row("notes/page.md").expect("delete_row");

        let result = db.get_row("notes/page.md").expect("get_row after delete");
        assert!(result.is_none(), "row should be deleted");
    }

    #[test]
    fn commit_batch_synced_inserts_with_status_synced() {
        let (mut db, _tmp) = open_temp_db();
        let results = vec![SyncResult::Synced {
            path: "notes/page.md".to_string(),
            local_hash: "abc".to_string(),
            remote_hash: "def".to_string(),
            remote_mtime: 1700000000000,
            local_mtime: 1700000001000,
        }];
        db.commit_batch(&results).expect("commit_batch");

        let row = db
            .get_row("notes/page.md")
            .expect("get_row")
            .expect("row should exist after commit");
        assert_eq!(row.status, SyncStatus::Synced);
        assert_eq!(row.local_hash, Some("abc".to_string()));
        assert_eq!(row.remote_hash, Some("def".to_string()));
    }

    #[test]
    fn commit_batch_conflict_updates_status_to_conflict() {
        let (mut db, _tmp) = open_temp_db();
        // Pre-insert the row as synced
        db.upsert_row(&make_row("notes/page.md")).expect("upsert");

        let results = vec![SyncResult::Conflict {
            path: "notes/page.md".to_string(),
            conflict_at: 0,
        }];
        db.commit_batch(&results).expect("commit_batch");

        let row = db
            .get_row("notes/page.md")
            .expect("get_row")
            .expect("row should still exist");
        assert_eq!(
            row.status,
            SyncStatus::Conflict,
            "status should be conflict"
        );
    }

    #[test]
    fn commit_batch_deleted_removes_row() {
        let (mut db, _tmp) = open_temp_db();
        db.upsert_row(&make_row("notes/page.md")).expect("upsert");

        let results = vec![SyncResult::Deleted {
            path: "notes/page.md".to_string(),
        }];
        db.commit_batch(&results).expect("commit_batch");

        let result = db.get_row("notes/page.md").expect("get_row after delete");
        assert!(result.is_none(), "row should be deleted after batch commit");
    }

    #[test]
    fn commit_batch_is_atomic_writes_multiple_entries() {
        let (mut db, _tmp) = open_temp_db();
        // Pre-insert two rows for conflict/delete
        db.upsert_row(&make_row("conflict.md"))
            .expect("upsert conflict");
        db.upsert_row(&make_row("deleted.md"))
            .expect("upsert deleted");

        let results = vec![
            SyncResult::Synced {
                path: "new.md".to_string(),
                local_hash: "h1".to_string(),
                remote_hash: "h2".to_string(),
                remote_mtime: 1700000000000,
                local_mtime: 1700000001000,
            },
            SyncResult::Conflict {
                path: "conflict.md".to_string(),
                conflict_at: 0,
            },
            SyncResult::Deleted {
                path: "deleted.md".to_string(),
            },
        ];
        db.commit_batch(&results).expect("commit_batch");

        // Verify all three outcomes applied atomically
        let new_row = db
            .get_row("new.md")
            .expect("get new.md")
            .expect("new.md should exist");
        assert_eq!(new_row.status, SyncStatus::Synced);

        let conflict_row = db
            .get_row("conflict.md")
            .expect("get conflict.md")
            .expect("conflict.md should still exist");
        assert_eq!(conflict_row.status, SyncStatus::Conflict);

        let deleted = db.get_row("deleted.md").expect("get deleted.md");
        assert!(deleted.is_none(), "deleted.md should be gone");
    }

    #[test]
    fn set_meta_and_get_meta_store_and_retrieve_values() {
        let (db, _tmp) = open_temp_db();
        db.set_meta("last_sync", "2026-04-06T00:00:00Z")
            .expect("set_meta");

        let val = db
            .get_meta("last_sync")
            .expect("get_meta")
            .expect("value should exist");
        assert_eq!(val, "2026-04-06T00:00:00Z");
    }

    #[test]
    fn get_meta_returns_none_for_missing_key() {
        let (db, _tmp) = open_temp_db();
        let result = db
            .get_meta("nonexistent_key")
            .expect("get_meta should not error");
        assert!(result.is_none(), "missing key should return None");
    }

    #[test]
    fn set_meta_overwrites_existing_value() {
        let (db, _tmp) = open_temp_db();
        db.set_meta("last_sync", "first_value")
            .expect("set_meta first");
        db.set_meta("last_sync", "second_value")
            .expect("set_meta second");

        let val = db
            .get_meta("last_sync")
            .expect("get_meta")
            .expect("value should exist");
        assert_eq!(val, "second_value", "second write should overwrite first");
    }

    #[test]
    fn sync_state_has_conflict_at_column() {
        let (_db, tmp) = open_temp_db();
        let conn = rusqlite::Connection::open(tmp.path()).expect("reopen db");
        let mut stmt = conn
            .prepare("PRAGMA table_info(sync_state)")
            .expect("prepare");
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .expect("query")
            .map(|r| r.expect("col"))
            .collect();
        assert!(
            cols.contains(&"conflict_at".to_string()),
            "sync_state should have conflict_at column; found: {:?}",
            cols
        );
    }

    #[test]
    fn commit_batch_conflict_stores_conflict_at_timestamp() {
        let (mut db, _tmp) = open_temp_db();
        // Pre-insert a synced row
        db.upsert_row(&make_row("notes/page.md")).expect("upsert");

        let conflict_time = 1700000050000i64;
        let results = vec![SyncResult::Conflict {
            path: "notes/page.md".to_string(),
            conflict_at: conflict_time,
        }];
        db.commit_batch(&results).expect("commit_batch");

        let row = db
            .get_row("notes/page.md")
            .expect("get_row")
            .expect("row should exist");
        assert_eq!(row.status, SyncStatus::Conflict);
        assert_eq!(
            row.conflict_at, conflict_time,
            "conflict_at should be stored"
        );
    }

    #[test]
    fn mark_resolved_updates_conflict_to_synced() {
        let (mut db, _tmp) = open_temp_db();
        // Insert a conflict row
        let mut row = make_row("page.md");
        row.status = SyncStatus::Conflict;
        row.conflict_at = 1700000050000;
        db.upsert_row(&row).expect("upsert");

        db.mark_resolved(
            "page.md",
            "newhash1",
            "newhash2",
            1700000060000,
            1700000061000,
        )
        .expect("mark_resolved");

        let resolved = db.get_row("page.md").expect("get").expect("exists");
        assert_eq!(resolved.status, SyncStatus::Synced);
        assert_eq!(resolved.local_hash, Some("newhash1".to_string()));
        assert_eq!(resolved.remote_hash, Some("newhash2".to_string()));
        assert_eq!(resolved.remote_mtime, 1700000060000);
        assert_eq!(resolved.local_mtime, 1700000061000);
        assert_eq!(resolved.conflict_at, 0);
    }

    #[test]
    fn mark_resolved_errors_on_nonexistent_path() {
        let (mut db, _tmp) = open_temp_db();
        let result = db.mark_resolved("missing.md", "h1", "h2", 0, 0);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not tracked"));
    }

    #[test]
    fn mark_resolved_errors_when_not_in_conflict_status() {
        let (mut db, _tmp) = open_temp_db();
        let row = make_row("page.md"); // status = Synced
        db.upsert_row(&row).expect("upsert");

        let result = db.mark_resolved("page.md", "h1", "h2", 0, 0);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not 'conflict'"));
    }

    #[test]
    fn migrate_adds_conflict_at_to_existing_db_without_it() {
        // Create a DB with the old schema (no conflict_at column)
        let tmp = NamedTempFile::new().expect("create temp file");
        {
            let conn = rusqlite::Connection::open(tmp.path()).expect("open");
            conn.execute_batch("PRAGMA journal_mode=WAL;").expect("wal");
            conn.execute_batch(
                "CREATE TABLE sync_state (
                    path TEXT PRIMARY KEY NOT NULL,
                    local_hash TEXT,
                    remote_hash TEXT,
                    remote_mtime INTEGER NOT NULL DEFAULT 0,
                    local_mtime INTEGER NOT NULL DEFAULT 0,
                    status TEXT NOT NULL DEFAULT 'synced'
                );
                CREATE TABLE sync_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );",
            )
            .expect("create old schema");
            // Insert a row with the old schema
            conn.execute(
                "INSERT INTO sync_state (path, local_hash, remote_hash, remote_mtime, local_mtime, status)
                 VALUES ('test.md', 'hash1', 'hash2', 1000, 2000, 'synced')",
                [],
            )
            .expect("insert old row");
        }
        // Re-open with StateDb::open which should run the migration
        let db = StateDb::open(tmp.path()).expect("open should migrate successfully");
        let row = db
            .get_row("test.md")
            .expect("get_row")
            .expect("row should exist");
        assert_eq!(
            row.conflict_at, 0,
            "migrated row should have conflict_at=0 default"
        );
    }
}
