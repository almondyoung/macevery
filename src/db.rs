use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, IndexStats, PathFilter, PathFilterMode, SearchOptions};
use crate::sqlite::{Connection, Step};
use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DELETE_PATH_PREFIX_SQL: &str = "DELETE FROM files
    WHERE path = ?1 COLLATE BINARY
       OR (path >= ?2 COLLATE BINARY AND path < ?3 COLLATE BINARY);";
const DELETE_STALE_PREFIX_SQL: &str = "DELETE FROM files
    WHERE indexed_at < ?1
      AND (path = ?2 COLLATE BINARY
           OR (path >= ?3 COLLATE BINARY AND path < ?4 COLLATE BINARY));";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseFileIdentity {
    pub device: u64,
    pub inode: u64,
}

impl DatabaseFileIdentity {
    fn at(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentStamp {
    pub database_id: String,
    pub revision: i64,
    pub file_identity: DatabaseFileIdentity,
}

pub const FILE_CHANGE_RETENTION: i64 = 16_384;
pub const FILE_CHANGE_PRUNE_INTERVAL: i64 = 1_024;
const MAX_FILE_CHANGE_BATCH: usize = 4_096;
pub const FILE_CHANGE_MAX_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct FileChange {
    pub id: i64,
    pub record: Option<FileRecord>,
}

#[derive(Clone, Debug)]
pub enum FileChangeRead {
    Available {
        target: ContentStamp,
        changes: Vec<FileChange>,
    },
    Fallback {
        reason: &'static str,
    },
}

pub struct Database {
    path: PathBuf,
    conn: Connection,
    identity: DatabaseFileIdentity,
}

// The observer serializes its connection, and reconnects after atomic database
// replacement. A failed reopen leaves None so a later request can retry.
pub struct ContentObserver {
    path: PathBuf,
    database: Mutex<Option<Database>>,
}

impl ContentObserver {
    pub fn open(path: PathBuf) -> Result<Self> {
        let database = Database::open_read_only(path.clone())?;
        Ok(Self {
            path,
            database: Mutex::new(Some(database)),
        })
    }

    pub fn content_stamp(&self) -> Result<ContentStamp> {
        let mut database = self
            .database
            .lock()
            .map_err(|_| MacEveryError::Sqlite("content observer lock poisoned".to_string()))?;
        let identity = match DatabaseFileIdentity::at(&self.path) {
            Ok(identity) => identity,
            Err(err) => {
                *database = None;
                return Err(err);
            }
        };
        if database.as_ref().is_none_or(|db| db.identity != identity) {
            // Close the old inode before opening a new connection at the same
            // pathname, including its associated WAL and SHM lifecycle.
            *database = None;
            *database = Some(Database::open_read_only(self.path.clone())?);
        }
        let stamp = database
            .as_ref()
            .ok_or_else(|| MacEveryError::Sqlite("content observer is disconnected".to_string()))?
            .content_stamp();
        if stamp.is_err() {
            // SQLite can retain an I/O error after a temporary rename/removal.
            // Discard that connection so the next observation starts afresh.
            *database = None;
        }
        stamp
    }
}

pub struct DbRecordRef<'a> {
    pub id: i64,
    pub path: &'a str,
    pub path_lower: Option<&'a str>,
    pub basename: &'a str,
    pub basename_lower: &'a str,
    pub ext_lower: Option<&'a str>,
    pub kind: FileKind,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    pub ctime: Option<i64>,
    pub dev: Option<i64>,
    pub inode: Option<i64>,
    pub indexed_at: i64,
}

impl Database {
    pub fn open(path: PathBuf) -> Result<Self> {
        let before = match DatabaseFileIdentity::at(&path) {
            Ok(identity) => Some(identity),
            Err(MacEveryError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let conn = Connection::open(&path)?;
        let identity = DatabaseFileIdentity::at(&path)?;
        if before.is_some_and(|before| before != identity) {
            return Err(MacEveryError::Sqlite(
                "database replaced while opening; retry".to_string(),
            ));
        }
        let db = Self {
            path,
            conn,
            identity,
        };
        db.init()?;
        Ok(db)
    }

    /// Migrate an existing database in place, without SQLITE_OPEN_CREATE.
    pub fn open_existing(path: PathBuf) -> Result<Self> {
        let before = DatabaseFileIdentity::at(&path)?;
        let conn = Connection::open_existing(&path)?;
        let identity = DatabaseFileIdentity::at(&path)?;
        if identity != before {
            return Err(MacEveryError::Sqlite(
                "database replaced while opening; retry".to_string(),
            ));
        }
        let db = Self {
            path,
            conn,
            identity,
        };
        db.init()?;
        Ok(db)
    }

    /// Open an existing migrated database without creating it or running schema DDL.
    pub fn open_read_only(path: PathBuf) -> Result<Self> {
        let before = DatabaseFileIdentity::at(&path)?;
        let conn = Connection::open_read_only(&path)?;
        let identity = DatabaseFileIdentity::at(&path)?;
        if identity != before {
            return Err(MacEveryError::Sqlite(
                "database replaced while opening; retry".to_string(),
            ));
        }
        Ok(Self {
            path,
            conn,
            identity,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A single-row read; includes WAL commits and excludes control-plane metadata.
    /// Every API-visible files value, including indexed_at, participates; no-op
    /// updates leave the revision unchanged.
    pub fn content_stamp(&self) -> Result<ContentStamp> {
        let mut stmt = self.conn.prepare(
            "SELECT database_id, revision FROM macevery_content_state WHERE singleton = 1;",
        )?;
        if !matches!(stmt.step()?, Step::Row) {
            return Err(MacEveryError::Sqlite(
                "missing content revision state".to_string(),
            ));
        }
        let database_id = stmt
            .column_text(0)
            .ok_or_else(|| MacEveryError::Sqlite("invalid database identity".to_string()))?;
        let revision = stmt
            .column_i64(1)
            .ok_or_else(|| MacEveryError::Sqlite("invalid content revision".to_string()))?;
        Ok(ContentStamp {
            database_id,
            revision,
            file_identity: self.identity,
        })
    }

    /// The first SELECT pins a WAL read snapshot; callers must commit or roll back.
    pub fn begin_read(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN DEFERRED TRANSACTION;")
    }

    pub fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                path_lower TEXT,
                basename TEXT NOT NULL,
                basename_lower TEXT NOT NULL,
                ext_lower TEXT,
                kind TEXT NOT NULL,
                size INTEGER,
                mtime INTEGER,
                ctime INTEGER,
                dev INTEGER,
                inode INTEGER,
                indexed_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_files_basename_lower ON files(basename_lower);
            CREATE INDEX IF NOT EXISTS idx_files_ext_lower ON files(ext_lower);
            CREATE INDEX IF NOT EXISTS idx_files_kind ON files(kind);
            CREATE INDEX IF NOT EXISTS idx_files_mtime ON files(mtime);
            CREATE TABLE IF NOT EXISTS roots (
                path TEXT PRIMARY KEY
            );
            CREATE TABLE IF NOT EXISTS excludes (
                pattern TEXT PRIMARY KEY
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        if !self.has_column("files", "path_lower")? {
            self.conn
                .execute_batch("ALTER TABLE files ADD COLUMN path_lower TEXT;")?;
        }
        if self.get_meta("schema_path_lower_backfilled_v2")?.as_deref() != Some("1") {
            self.backfill_path_lower()?;
            self.set_meta("schema_path_lower_backfilled_v2", "1")?;
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_files_path_lower ON files(path_lower);",
        )?;
        self.init_content_revision()?;
        self.init_file_changes()?;
        Ok(())
    }

    fn init_content_revision(&self) -> Result<()> {
        const MIGRATION: &str = "schema_content_revision_v1";
        if self.get_meta(MIGRATION)?.as_deref() == Some("1") {
            return Ok(());
        }
        self.begin()?;
        let migration = (|| {
            // Another opener may have installed the migration while BEGIN waited.
            if self.get_meta(MIGRATION)?.as_deref() != Some("1") {
                self.conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS macevery_content_state (
                        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                        database_id TEXT NOT NULL,
                        revision INTEGER NOT NULL CHECK (typeof(revision) = 'integer' AND revision >= 0)
                     );
                     INSERT OR IGNORE INTO macevery_content_state(singleton, database_id, revision)
                     VALUES (1, lower(hex(randomblob(16))), 0);
                     CREATE TRIGGER IF NOT EXISTS macevery_files_insert_revision_v1
                     AFTER INSERT ON files BEGIN
                        UPDATE macevery_content_state SET revision = revision + 1 WHERE singleton = 1;
                     END;
                     CREATE TRIGGER IF NOT EXISTS macevery_files_delete_revision_v1
                     AFTER DELETE ON files BEGIN
                        UPDATE macevery_content_state SET revision = revision + 1 WHERE singleton = 1;
                     END;
                     CREATE TRIGGER IF NOT EXISTS macevery_files_update_revision_v1
                     AFTER UPDATE ON files
                     WHEN OLD.id IS NOT NEW.id OR OLD.path IS NOT NEW.path
                       OR OLD.path_lower IS NOT NEW.path_lower
                       OR OLD.basename IS NOT NEW.basename OR OLD.basename_lower IS NOT NEW.basename_lower
                       OR OLD.ext_lower IS NOT NEW.ext_lower OR OLD.kind IS NOT NEW.kind
                       OR OLD.size IS NOT NEW.size OR OLD.mtime IS NOT NEW.mtime OR OLD.ctime IS NOT NEW.ctime
                       OR OLD.dev IS NOT NEW.dev OR OLD.inode IS NOT NEW.inode
                       OR OLD.indexed_at IS NOT NEW.indexed_at
                     BEGIN
                        UPDATE macevery_content_state SET revision = revision + 1 WHERE singleton = 1;
                     END;"
                )?;
                self.set_meta(MIGRATION, "1")?;
            }
            self.commit()
        })();
        if migration.is_err() {
            let _ = self.rollback();
        }
        migration
    }

    pub fn has_change_journal(&self) -> Result<bool> {
        if self.get_meta("schema_file_changes_v1")?.as_deref() != Some("1") {
            return Ok(false);
        }
        let mut stmt = self.conn.prepare(
            "SELECT COUNT(*) FROM sqlite_master WHERE
                (type = 'table' AND name = 'macevery_file_changes') OR
                (type = 'trigger' AND name IN (
                    'macevery_files_before_insert_changes_v1',
                    'macevery_files_before_update_changes_v1',
                    'macevery_files_insert_changes_v1',
                    'macevery_files_update_changes_v1',
                    'macevery_files_delete_changes_v1',
                    'macevery_file_changes_prune_v1'));",
        )?;
        Ok(matches!(stmt.step()?, Step::Row) && stmt.column_i64(0) == Some(7))
    }

    fn init_file_changes(&self) -> Result<()> {
        if self.has_change_journal()? {
            return Ok(());
        }
        self.begin()?;
        let migration = (|| {
            if !self.has_change_journal()? {
                if !self.has_column("macevery_content_state", "journal_floor")? {
                    self.conn.execute_batch("ALTER TABLE macevery_content_state ADD COLUMN journal_floor INTEGER NOT NULL DEFAULT 0;")?;
                }
                if !self.has_column("macevery_content_state", "journal_conflict_id")? {
                    self.conn.execute_batch("ALTER TABLE macevery_content_state ADD COLUMN journal_conflict_id INTEGER;")?;
                }
                // Reinstall only when absent/incomplete, including a dropped journal
                // whose metadata marker survived. Reset coverage, not content identity.
                // SQLite serializes writers. BEFORE triggers overwrite one fixed
                // scratch ID for each row, capturing OR REPLACE path victims even
                // with recursive_triggers=OFF. Extra custom files triggers issuing
                // nested writes/schema changes are outside this owned-schema contract.
                self.conn.execute_batch(&format!(
                    "DROP TRIGGER IF EXISTS macevery_files_insert_revision_v1;
                     DROP TRIGGER IF EXISTS macevery_files_update_revision_v1;
                     DROP TRIGGER IF EXISTS macevery_files_delete_revision_v1;
                     DROP TRIGGER IF EXISTS macevery_files_before_insert_changes_v1;
                     DROP TRIGGER IF EXISTS macevery_files_before_update_changes_v1;
                     DROP TRIGGER IF EXISTS macevery_files_insert_changes_v1;
                     DROP TRIGGER IF EXISTS macevery_files_update_changes_v1;
                     DROP TRIGGER IF EXISTS macevery_files_delete_changes_v1;
                     DROP TRIGGER IF EXISTS macevery_file_changes_prune_v1;
                     CREATE TABLE IF NOT EXISTS macevery_file_changes (
                        revision INTEGER PRIMARY KEY,
                        old_id INTEGER,
                        new_id INTEGER,
                        conflict_id INTEGER
                     );
                     DELETE FROM macevery_file_changes;
                     UPDATE macevery_content_state SET journal_floor = revision, journal_conflict_id = NULL WHERE singleton = 1;
                     CREATE TRIGGER macevery_files_before_insert_changes_v1
                     BEFORE INSERT ON files BEGIN
                        UPDATE macevery_content_state SET journal_conflict_id =
                            (SELECT id FROM files WHERE path = NEW.path COLLATE BINARY)
                        WHERE singleton = 1;
                     END;
                     CREATE TRIGGER macevery_files_before_update_changes_v1
                     BEFORE UPDATE ON files BEGIN
                        UPDATE macevery_content_state SET journal_conflict_id =
                            CASE WHEN NEW.path IS OLD.path THEN NULL ELSE
                                (SELECT id FROM files WHERE path = NEW.path COLLATE BINARY AND id != OLD.id)
                            END
                        WHERE singleton = 1;
                     END;
                     CREATE TRIGGER macevery_files_insert_changes_v1
                     AFTER INSERT ON files BEGIN
                        UPDATE macevery_content_state SET revision = revision + 1 WHERE singleton = 1;
                        INSERT INTO macevery_file_changes(revision,old_id,new_id,conflict_id)
                            SELECT revision,NULL,NEW.id,journal_conflict_id FROM macevery_content_state WHERE singleton = 1;
                     END;
                     CREATE TRIGGER macevery_files_update_changes_v1
                     AFTER UPDATE ON files
                     WHEN OLD.id IS NOT NEW.id OR OLD.path IS NOT NEW.path
                       OR OLD.path_lower IS NOT NEW.path_lower
                       OR OLD.basename IS NOT NEW.basename OR OLD.basename_lower IS NOT NEW.basename_lower
                       OR OLD.ext_lower IS NOT NEW.ext_lower OR OLD.kind IS NOT NEW.kind
                       OR OLD.size IS NOT NEW.size OR OLD.mtime IS NOT NEW.mtime OR OLD.ctime IS NOT NEW.ctime
                       OR OLD.dev IS NOT NEW.dev OR OLD.inode IS NOT NEW.inode
                       OR OLD.indexed_at IS NOT NEW.indexed_at
                     BEGIN
                        UPDATE macevery_content_state SET revision = revision + 1 WHERE singleton = 1;
                        INSERT INTO macevery_file_changes(revision,old_id,new_id,conflict_id)
                            SELECT revision,OLD.id,NEW.id,journal_conflict_id FROM macevery_content_state WHERE singleton = 1;
                     END;
                     CREATE TRIGGER macevery_files_delete_changes_v1
                     AFTER DELETE ON files BEGIN
                        UPDATE macevery_content_state SET revision = revision + 1 WHERE singleton = 1;
                        INSERT INTO macevery_file_changes(revision,old_id,new_id,conflict_id)
                            SELECT revision,OLD.id,NULL,NULL FROM macevery_content_state WHERE singleton = 1;
                     END;
                     CREATE TRIGGER macevery_file_changes_prune_v1
                     AFTER INSERT ON macevery_file_changes WHEN NEW.revision % {interval} = 0 BEGIN
                        UPDATE macevery_content_state SET journal_floor = MAX(journal_floor,
                            COALESCE((SELECT MAX(revision) FROM macevery_file_changes WHERE revision <= NEW.revision - {retention}),journal_floor))
                        WHERE singleton = 1;
                        DELETE FROM macevery_file_changes WHERE revision <= NEW.revision - {retention};
                     END;",
                    interval = FILE_CHANGE_PRUNE_INTERVAL,
                    retention = FILE_CHANGE_RETENTION
                ))?;
                self.set_meta("schema_file_changes_v1", "1")?;
            }
            self.commit()
        })();
        if migration.is_err() {
            let _ = self.rollback();
        }
        migration
    }

    /// Read a bounded final-state delta inside the caller's already-open read
    /// transaction. The caller owns commit/rollback. All IDs and rows come from
    /// the same snapshot as target; a later writer is detected on the next check.
    pub fn file_changes_since(
        &self,
        base: &ContentStamp,
        max_events: usize,
        max_ids: usize,
    ) -> Result<FileChangeRead> {
        self.file_changes_since_with_byte_limit(
            base,
            max_events,
            max_ids,
            FILE_CHANGE_MAX_PAYLOAD_BYTES,
        )
    }

    fn file_changes_since_with_byte_limit(
        &self,
        base: &ContentStamp,
        max_events: usize,
        max_ids: usize,
        max_payload_bytes: usize,
    ) -> Result<FileChangeRead> {
        if self.conn.is_autocommit() {
            return Err(MacEveryError::Sqlite(
                "file_changes_since requires an active read transaction".into(),
            ));
        }
        let target = self.content_stamp()?;
        let fallback = |reason| Ok(FileChangeRead::Fallback { reason });
        if base.database_id != target.database_id || base.file_identity != target.file_identity {
            return fallback("database_replaced");
        }
        if target.revision < base.revision {
            return fallback("revision_regressed");
        }
        if !self.has_change_journal()? {
            return fallback("journal_unavailable");
        }
        if target.revision == base.revision {
            return Ok(FileChangeRead::Available {
                target,
                changes: Vec::new(),
            });
        }
        let mut floor = self
            .conn
            .prepare("SELECT journal_floor FROM macevery_content_state WHERE singleton = 1;")?;
        if !matches!(floor.step()?, Step::Row)
            || floor
                .column_i64(0)
                .is_none_or(|floor| base.revision < floor)
        {
            return fallback("history_gap");
        }
        drop(floor);
        let span = target
            .revision
            .checked_sub(base.revision)
            .ok_or_else(|| MacEveryError::Sqlite("invalid revision span".into()))?;
        if span as u64 > max_events.min(MAX_FILE_CHANGE_BATCH) as u64 {
            return fallback("event_limit");
        }
        let mut events = self.conn.prepare("SELECT revision,old_id,new_id,conflict_id FROM macevery_file_changes WHERE revision > ?1 AND revision <= ?2 ORDER BY revision;")?;
        events.bind_i64(1, Some(base.revision))?;
        events.bind_i64(2, Some(target.revision))?;
        let mut previous = base.revision;
        let mut ids = BTreeSet::new();
        while let Step::Row = events.step()? {
            let Some(revision) = events.column_i64(0) else {
                return fallback("history_gap");
            };
            if revision.checked_sub(previous) != Some(1) {
                return fallback("history_gap");
            }
            previous = revision;
            let mut has_id = false;
            for column in 1..=3 {
                if let Some(id) = events.column_i64(column) {
                    has_id = true;
                    ids.insert(id);
                    if ids.len() > max_ids.min(MAX_FILE_CHANGE_BATCH) {
                        return fallback("id_limit");
                    }
                }
            }
            if !has_id {
                return fallback("history_gap");
            }
        }
        if previous != target.revision {
            return fallback("history_gap");
        }
        drop(events);
        let mut rows = self.conn.prepare(
            "SELECT id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at FROM files WHERE id = ?1;"
        )?;
        let mut changes = Vec::with_capacity(ids.len());
        let mut payload_bytes = 0usize;
        for id in ids {
            rows.reset()?;
            rows.bind_i64(1, Some(id))?;
            let record = match rows.step()? {
                Step::Row => {
                    // Borrow SQLite's current row before allocating any owned
                    // FileRecord strings. NULL/empty path_lower uses the same
                    // Unicode-lowercase fallback as row_to_record below.
                    let raw_bytes = (1..=6).fold(0usize, |sum, column| {
                        sum.saturating_add(rows.column_byte_len(column))
                    });
                    if payload_bytes.saturating_add(raw_bytes) > max_payload_bytes {
                        return fallback("byte_limit");
                    }
                    let bytes = row_text_payload_bytes(&rows);
                    let Some(total) = payload_bytes.checked_add(bytes) else {
                        return fallback("byte_limit");
                    };
                    if total > max_payload_bytes {
                        return fallback("byte_limit");
                    }
                    payload_bytes = total;
                    Some(row_to_record(&rows)?)
                }
                Step::Done => None,
            };
            changes.push(FileChange { id, record });
        }
        Ok(FileChangeRead::Available { target, changes })
    }

    pub fn begin(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE TRANSACTION;")
    }

    pub fn commit(&self) -> Result<()> {
        self.conn.execute_batch("COMMIT;")
    }

    pub fn rollback(&self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK;")
    }

    pub fn clear_index(&self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM files; DELETE FROM roots; DELETE FROM excludes;")
    }

    pub fn set_roots(&self, roots: &[String]) -> Result<()> {
        self.conn.execute_batch("DELETE FROM roots;")?;
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO roots(path) VALUES (?1);")?;
        for root in roots {
            stmt.reset()?;
            stmt.bind_text(1, Some(root))?;
            expect_done(stmt.step()?)?;
        }
        Ok(())
    }

    pub fn set_excludes(&self, excludes: &[String]) -> Result<()> {
        self.conn.execute_batch("DELETE FROM excludes;")?;
        let mut stmt = self
            .conn
            .prepare("INSERT OR REPLACE INTO excludes(pattern) VALUES (?1);")?;
        for pattern in excludes {
            stmt.reset()?;
            stmt.bind_text(1, Some(pattern))?;
            expect_done(stmt.step()?)?;
        }
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value;",
        )?;
        stmt.bind_text(1, Some(key))?;
        stmt.bind_text(2, Some(value))?;
        expect_done(stmt.step()?)
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key = ?1 LIMIT 1;")?;
        stmt.bind_text(1, Some(key))?;
        match stmt.step()? {
            Step::Row => Ok(stmt.column_text(0)),
            Step::Done => Ok(None),
        }
    }

    pub fn upsert_file(&self, record: &FileRecord) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO files (
                path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime,
                dev, inode, indexed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(path) DO UPDATE SET
                path_lower=excluded.path_lower,
                basename=excluded.basename,
                basename_lower=excluded.basename_lower,
                ext_lower=excluded.ext_lower,
                kind=excluded.kind,
                size=excluded.size,
                mtime=excluded.mtime,
                ctime=excluded.ctime,
                dev=excluded.dev,
                inode=excluded.inode,
                indexed_at=excluded.indexed_at;",
        )?;
        stmt.bind_text(1, Some(&record.path))?;
        stmt.bind_text(2, Some(&record.path_lower))?;
        stmt.bind_text(3, Some(&record.basename))?;
        stmt.bind_text(4, Some(&record.basename_lower))?;
        stmt.bind_text(5, record.ext_lower.as_deref())?;
        stmt.bind_text(6, Some(record.kind.as_str()))?;
        stmt.bind_i64(7, record.size)?;
        stmt.bind_i64(8, record.mtime)?;
        stmt.bind_i64(9, record.ctime)?;
        stmt.bind_i64(10, record.dev)?;
        stmt.bind_i64(11, record.inode)?;
        stmt.bind_i64(12, Some(record.indexed_at))?;
        expect_done(stmt.step()?)
    }

    pub fn delete_path_prefix(&self, path: &str) -> Result<()> {
        let Some((path, lower, upper)) = path_prefix_bounds(path) else {
            return Ok(());
        };
        let mut stmt = self.conn.prepare(DELETE_PATH_PREFIX_SQL)?;
        stmt.bind_text(1, Some(&path))?;
        stmt.bind_text(2, Some(&lower))?;
        stmt.bind_text(3, Some(&upper))?;
        expect_done(stmt.step()?)
    }

    pub fn delete_path_exact(&self, path: &str) -> Result<()> {
        let mut stmt = self.conn.prepare("DELETE FROM files WHERE path = ?1;")?;
        stmt.bind_text(1, Some(path))?;
        expect_done(stmt.step()?)
    }

    pub fn kind_for_path(&self, path: &str) -> Result<Option<FileKind>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind FROM files WHERE path = ?1 LIMIT 1;")?;
        stmt.bind_text(1, Some(path))?;
        match stmt.step()? {
            Step::Row => Ok(stmt
                .column_text(0)
                .and_then(|value| FileKind::parse(&value))),
            Step::Done => Ok(None),
        }
    }

    pub fn delete_stale_under_roots(&self, roots: &[PathBuf], indexed_at: i64) -> Result<u64> {
        let mut stmt = self.conn.prepare(DELETE_STALE_PREFIX_SQL)?;
        let mut deleted = 0;
        for root in roots {
            let Some((root, lower, upper)) = path_prefix_bounds(&root.to_string_lossy()) else {
                continue;
            };
            stmt.reset()?;
            stmt.bind_i64(1, Some(indexed_at))?;
            stmt.bind_text(2, Some(&root))?;
            stmt.bind_text(3, Some(&lower))?;
            stmt.bind_text(4, Some(&upper))?;
            expect_done(stmt.step()?)?;
            deleted += self.conn.changes();
        }
        Ok(deleted)
    }

    pub fn candidate_records(
        &self,
        options: &SearchOptions,
        max_candidates: usize,
    ) -> Result<Vec<FileRecord>> {
        self.candidate_records_impl(options, max_candidates, true)
    }

    pub fn filtered_records(
        &self,
        options: &SearchOptions,
        max_candidates: usize,
    ) -> Result<Vec<FileRecord>> {
        self.candidate_records_impl(options, max_candidates, false)
    }

    fn candidate_records_impl(
        &self,
        options: &SearchOptions,
        max_candidates: usize,
        include_text_terms: bool,
    ) -> Result<Vec<FileRecord>> {
        let options = options.normalized();
        let tokens = options.terms();
        let mut sql = String::from(
            "SELECT id, path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime, dev, inode, indexed_at
             FROM files",
        );
        let mut clauses = Vec::new();
        let mut bindings: Vec<Binding> = Vec::new();

        if !options.ext_filters.is_empty() {
            clauses.push(format!(
                "ext_lower IN ({})",
                placeholders(options.ext_filters.len())
            ));
            for ext in &options.ext_filters {
                bindings.push(Binding::Text(ext.trim_start_matches('.').to_lowercase()));
            }
        }
        if !options.excluded_ext_filters.is_empty() {
            clauses.push(format!(
                "(ext_lower IS NULL OR ext_lower NOT IN ({}))",
                placeholders(options.excluded_ext_filters.len())
            ));
            for ext in &options.excluded_ext_filters {
                bindings.push(Binding::Text(ext.trim_start_matches('.').to_lowercase()));
            }
        }
        if !options.kind_filters.is_empty() {
            clauses.push(format!(
                "kind IN ({})",
                placeholders(options.kind_filters.len())
            ));
            for kind in &options.kind_filters {
                bindings.push(Binding::Text(kind.as_str().to_string()));
            }
        }
        if !options.excluded_kind_filters.is_empty() {
            clauses.push(format!(
                "kind NOT IN ({})",
                placeholders(options.excluded_kind_filters.len())
            ));
            for kind in &options.excluded_kind_filters {
                bindings.push(Binding::Text(kind.as_str().to_string()));
            }
        }
        for path_filter in &options.path_filters {
            clauses.push("path_lower LIKE ? ESCAPE '\\'".to_string());
            bindings.push(Binding::Text(path_filter_like(path_filter)));
        }
        for path_filter in &options.excluded_path_filters {
            if matches!(path_filter.mode, PathFilterMode::Contains) {
                clauses.push("path_lower NOT LIKE ? ESCAPE '\\'".to_string());
                bindings.push(Binding::Text(path_filter_like(path_filter)));
            }
        }
        if let Some(modified_after) = options.modified_after {
            clauses.push("mtime IS NOT NULL AND mtime >= ?".to_string());
            bindings.push(Binding::Int(modified_after));
        }
        if let Some(modified_before) = options.modified_before {
            clauses.push("(mtime IS NULL OR mtime < ?)".to_string());
            bindings.push(Binding::Int(modified_before));
        }
        if include_text_terms {
            for token in &tokens {
                let token_lower = token.to_lowercase();
                let is_glob = token.contains('*') || token.contains('?');
                if options.path_only {
                    clauses.push("path_lower LIKE ? ESCAPE '\\'".to_string());
                    bindings.push(Binding::Text(if is_glob {
                        glob_to_like(&token_lower)
                    } else {
                        format!("%{}%", escape_like(&token_lower))
                    }));
                } else {
                    clauses.push(
                        "(basename_lower LIKE ? ESCAPE '\\' OR path_lower LIKE ? ESCAPE '\\')"
                            .to_string(),
                    );
                    let needle = if is_glob {
                        glob_to_like(&token_lower)
                    } else {
                        format!("%{}%", escape_like(&token_lower))
                    };
                    bindings.push(Binding::Text(needle.clone()));
                    bindings.push(Binding::Text(needle));
                }
            }
        }

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(" ORDER BY mtime DESC LIMIT ?");
        bindings.push(Binding::Int(max_candidates as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        for (index, binding) in bindings.iter().enumerate() {
            match binding {
                Binding::Text(value) => stmt.bind_text((index + 1) as i32, Some(value))?,
                Binding::Int(value) => stmt.bind_i64((index + 1) as i32, Some(*value))?,
            }
        }

        let mut records = Vec::new();
        while let Step::Row = stmt.step()? {
            records.push(row_to_record(&stmt)?);
        }
        Ok(records)
    }

    pub fn all_records(&self) -> Result<Vec<FileRecord>> {
        let mut records = Vec::new();
        self.for_each_record(|record| {
            records.push(record);
            Ok(())
        })?;
        Ok(records)
    }

    pub fn for_each_record<F>(&self, mut visit: F) -> Result<()>
    where
        F: FnMut(FileRecord) -> Result<()>,
    {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime, dev, inode, indexed_at
             FROM files ORDER BY id;",
        )?;
        while let Step::Row = stmt.step()? {
            visit(row_to_record(&stmt)?)?;
        }
        Ok(())
    }

    pub fn for_each_record_ref<F>(&self, mut visit: F) -> Result<()>
    where
        F: FnMut(DbRecordRef<'_>) -> Result<()>,
    {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime, dev, inode, indexed_at
             FROM files ORDER BY id;",
        )?;
        while let Step::Row = stmt.step()? {
            visit(row_to_record_ref(&stmt)?)?;
        }
        Ok(())
    }

    pub fn stats(&self) -> Result<IndexStats> {
        let mut stats = IndexStats {
            db_path: self.path.to_string_lossy().to_string(),
            ..IndexStats::default()
        };

        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM files GROUP BY kind;")?;
        while let Step::Row = stmt.step()? {
            let kind = stmt.column_text(0).unwrap_or_default();
            let count = stmt.column_i64(1).unwrap_or(0);
            match kind.as_str() {
                "file" => stats.files = count,
                "dir" => stats.dirs = count,
                "symlink" => stats.symlinks = count,
                "app" => stats.apps = count,
                _ => stats.others += count,
            }
        }

        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key = 'last_indexed_at';")?;
        if let Step::Row = stmt.step()? {
            stats.last_indexed_at = stmt.column_text(0).and_then(|value| value.parse().ok());
        }

        stats.roots = self.string_list("SELECT path FROM roots ORDER BY path;")?;
        stats.excludes = self.string_list("SELECT pattern FROM excludes ORDER BY pattern;")?;
        Ok(stats)
    }

    fn string_list(&self, sql: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql)?;
        let mut values = Vec::new();
        while let Step::Row = stmt.step()? {
            if let Some(value) = stmt.column_text(0) {
                values.push(value);
            }
        }
        Ok(values)
    }

    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table});"))?;
        while let Step::Row = stmt.step()? {
            if stmt.column_text(1).as_deref() == Some(column) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn backfill_path_lower(&self) -> Result<()> {
        let mut read_stmt = self.conn.prepare(
            "SELECT id, path FROM files WHERE path_lower IS NULL OR path_lower = lower(path);",
        )?;
        let mut updates: Vec<(i64, String)> = Vec::new();
        while let Step::Row = read_stmt.step()? {
            let id = read_stmt.column_i64(0).unwrap_or(0);
            if let Some(path) = read_stmt.column_text(1) {
                let lowered = path.to_lowercase();
                updates.push((id, lowered));
            }
        }
        drop(read_stmt);

        if updates.is_empty() {
            return Ok(());
        }

        self.conn.execute_batch("BEGIN;")?;
        let result = (|| {
            let mut stmt = self
                .conn
                .prepare("UPDATE files SET path_lower = ?1 WHERE id = ?2;")?;
            for (id, lowered) in &updates {
                stmt.reset()?;
                stmt.bind_text(1, Some(lowered))?;
                stmt.bind_i64(2, Some(*id))?;
                expect_done(stmt.step()?)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.conn.execute_batch("COMMIT;"),
            Err(err) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(err)
            }
        }
    }
}

// The slash terminating a descendant prefix is ASCII 0x2f, so replacing it
// with 0x30 gives its exclusive BINARY upper bound, including Unicode paths.
fn path_prefix_bounds(path: &str) -> Option<(String, String, String)> {
    let trimmed = path.trim_end_matches('/');
    let root = if trimmed.is_empty() && path.starts_with('/') {
        "/"
    } else {
        trimmed
    };
    if root.is_empty() {
        return None;
    }
    let lower = if root == "/" {
        "/".to_string()
    } else {
        format!("{root}/")
    };
    let mut upper = lower.clone();
    upper.pop();
    upper.push('0');
    Some((root.to_string(), lower, upper))
}

enum Binding {
    Text(String),
    Int(i64),
}

fn row_text_payload_bytes(stmt: &crate::sqlite::Statement<'_>) -> usize {
    let text_bytes = |column| match stmt.column_str(column) {
        Some(value) => value.len(),
        // Charge malformed UTF-8 conservatively, including a future lossy
        // decoder. NULL has zero raw bytes. Never interpret None as zero
        // without checking the underlying SQLite byte count.
        None => stmt.column_byte_len(column).saturating_mul(3),
    };
    let fallback_lower = || match stmt.column_str(1) {
        Some(path) => path
            .chars()
            .flat_map(char::to_lowercase)
            .fold(0usize, |sum, value| sum.saturating_add(value.len_utf8())),
        None => stmt.column_byte_len(1).saturating_mul(3),
    };
    let lower_bytes = match stmt.column_str(2).filter(|value| !value.is_empty()) {
        Some(lower) => lower.len(),
        // Context-sensitive final sigma has the same UTF-8 width as sigma;
        // expansions are counted without constructing a lowercase String.
        None => fallback_lower().max(stmt.column_byte_len(2).saturating_mul(3)),
    };
    [
        text_bytes(1),
        lower_bytes,
        text_bytes(3),
        text_bytes(4),
        text_bytes(5),
        // FileKind::parse lowercases the complete SQLite value, including
        // unknown kinds. Reserve its worst-case Unicode expansion before parse.
        stmt.column_byte_len(6).saturating_mul(3),
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add)
}

fn row_to_record_ref<'a>(stmt: &'a crate::sqlite::Statement<'_>) -> Result<DbRecordRef<'a>> {
    Ok(DbRecordRef {
        id: stmt.column_i64(0).unwrap_or(0),
        path: stmt.column_str(1).unwrap_or_default(),
        path_lower: stmt.column_str(2).filter(|value| !value.is_empty()),
        basename: stmt.column_str(3).unwrap_or_default(),
        basename_lower: stmt.column_str(4).unwrap_or_default(),
        ext_lower: stmt.column_str(5),
        kind: stmt
            .column_str(6)
            .and_then(FileKind::parse)
            .unwrap_or(FileKind::Other),
        size: stmt.column_i64(7),
        mtime: stmt.column_i64(8),
        ctime: stmt.column_i64(9),
        dev: stmt.column_i64(10),
        inode: stmt.column_i64(11),
        indexed_at: stmt.column_i64(12).unwrap_or(0),
    })
}

fn expect_done(step: Step) -> Result<()> {
    match step {
        Step::Done => Ok(()),
        Step::Row => Err(MacEveryError::Sqlite(
            "statement unexpectedly returned a row".to_string(),
        )),
    }
}

fn row_to_record(stmt: &crate::sqlite::Statement<'_>) -> Result<FileRecord> {
    let kind = stmt
        .column_str(6)
        .and_then(FileKind::parse)
        .unwrap_or(FileKind::Other);
    let path = stmt.column_text(1).unwrap_or_default();
    let path_lower = stmt
        .column_text(2)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| path.to_lowercase());
    Ok(FileRecord {
        id: stmt.column_i64(0).unwrap_or(0),
        path_lower,
        path,
        basename: stmt.column_text(3).unwrap_or_default(),
        basename_lower: stmt.column_text(4).unwrap_or_default(),
        ext_lower: stmt.column_text(5),
        kind,
        size: stmt.column_i64(7),
        mtime: stmt.column_i64(8),
        ctime: stmt.column_i64(9),
        dev: stmt.column_i64(10),
        inode: stmt.column_i64(11),
        indexed_at: stmt.column_i64(12).unwrap_or(0),
    })
}

pub fn query_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn is_smart_case_sensitive(query: &str) -> bool {
    query.chars().any(|ch| ch.is_uppercase())
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn glob_to_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for ch in value.chars() {
        match ch {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            ch => out.push(ch),
        }
    }
    out
}

fn placeholders(count: usize) -> String {
    (0..count).map(|_| "?").collect::<Vec<_>>().join(",")
}

fn path_filter_like(filter: &PathFilter) -> String {
    match filter.mode {
        PathFilterMode::Contains => {
            if filter.value.contains('*') || filter.value.contains('?') {
                glob_to_like(&filter.value)
            } else {
                format!("%{}%", escape_like(&filter.value))
            }
        }
        PathFilterMode::Component => {
            if filter.value.contains('*') || filter.value.contains('?') {
                glob_to_like(&format!("*/{}/*", filter.value))
            } else {
                format!("%/{}%", escape_like(&filter.value))
            }
        }
    }
}

#[cfg(test)]
mod prefix_cleanup_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_database() -> (Database, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "macevery-prefix-{}-{unique}-{serial}",
            std::process::id()
        ));
        (Database::open(dir.join("index.sqlite")).unwrap(), dir)
    }

    fn insert(db: &Database, path: &str, generation: i64) {
        db.upsert_file(&FileRecord::new(
            path.to_string(),
            FileKind::File,
            generation,
        ))
        .unwrap();
    }

    fn remaining(db: &Database) -> Vec<String> {
        let mut paths = db
            .all_records()
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[test]
    fn prefix_delete_preserves_case_siblings_and_literal_unicode_paths() {
        let (db, dir) = temp_database();
        for path in [
            "/Foo",
            "/Foo/a",
            "/Foo/目录/文件",
            "/foo/b",
            "/Foobar/c",
            "/Foo-old/d",
            "/Foo0/x",
            "/特殊%_\\",
            "/特殊%_\\/子",
            "/特殊X_\\/子",
        ] {
            insert(&db, path, 1);
        }
        db.delete_path_prefix("/Foo/").unwrap();
        assert_eq!(
            remaining(&db),
            [
                "/Foo-old/d",
                "/Foo0/x",
                "/Foobar/c",
                "/foo/b",
                "/特殊%_\\",
                "/特殊%_\\/子",
                "/特殊X_\\/子"
            ]
        );
        db.delete_path_prefix("/特殊%_\\").unwrap();
        assert_eq!(
            remaining(&db),
            [
                "/Foo-old/d",
                "/Foo0/x",
                "/Foobar/c",
                "/foo/b",
                "/特殊X_\\/子"
            ]
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_prefix_cleanup_preserves_new_generation_and_handles_root() {
        let (db, dir) = temp_database();
        for path in [
            "/",
            "/Foo",
            "/Foo/old",
            "/foo/keep",
            "/Foobar/keep",
            "relative",
            "relative/old",
        ] {
            insert(&db, path, 1);
        }
        insert(&db, "/Foo/new", 2);
        assert_eq!(
            db.delete_stale_under_roots(&[PathBuf::from("/Foo/")], 2)
                .unwrap(),
            2
        );
        assert!(remaining(&db).contains(&"/Foo/new".to_string()));
        assert!(remaining(&db).contains(&"/foo/keep".to_string()));
        assert_eq!(
            db.delete_stale_under_roots(&[PathBuf::from("/")], 2)
                .unwrap(),
            3
        );
        assert_eq!(remaining(&db), ["/Foo/new", "relative", "relative/old"]);
        db.delete_path_prefix("").unwrap();
        assert_eq!(remaining(&db).len(), 3);
        db.delete_path_prefix("/").unwrap();
        assert_eq!(remaining(&db), ["relative", "relative/old"]);
        db.delete_path_prefix("relative/").unwrap();
        assert!(remaining(&db).is_empty());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prefix_cleanup_uses_path_index() {
        let (db, dir) = temp_database();
        for (sql, stale) in [
            (DELETE_PATH_PREFIX_SQL, false),
            (DELETE_STALE_PREFIX_SQL, true),
        ] {
            let mut stmt = db
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let offset = if stale {
                stmt.bind_i64(1, Some(2)).unwrap();
                1
            } else {
                0
            };
            for (index, value) in ["/目录%_", "/目录%_/", "/目录%_0"].iter().enumerate() {
                stmt.bind_text(index as i32 + 1 + offset, Some(value))
                    .unwrap();
            }
            let mut details = Vec::new();
            while let Step::Row = stmt.step().unwrap() {
                details.push(stmt.column_text(3).unwrap());
            }
            println!("{sql} => {}", details.join("; "));
            assert!(
                details.iter().any(|line| line.contains("SEARCH files")),
                "{details:?}"
            );
            assert!(
                !details.iter().any(|line| line.contains("SCAN files")),
                "{details:?}"
            );
        }
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod content_revision_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "macevery-content-{label}-{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn database(&self) -> PathBuf {
            self.0.join("index.sqlite")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scalar(db: &Database, sql: &str) -> i64 {
        let mut stmt = db.conn.prepare(sql).unwrap();
        assert!(matches!(stmt.step().unwrap(), Step::Row));
        stmt.column_i64(0).unwrap()
    }

    fn record(path: &str, generation: i64) -> FileRecord {
        FileRecord::new(path.to_string(), FileKind::File, generation)
    }

    #[test]
    fn control_metadata_and_noop_updates_do_not_change_file_content_stamp() {
        let fixture = Fixture::new("control");
        let db = Database::open(fixture.database()).unwrap();
        db.upsert_file(&record("/root/a.txt", 10)).unwrap();
        let initial = db.content_stamp().unwrap();
        db.set_meta("watch_last_event_id", "20").unwrap();
        db.set_meta("last_indexed_at", "30").unwrap();
        db.set_roots(&["/root".to_string()]).unwrap();
        db.set_excludes(&[".git".to_string()]).unwrap();
        db.upsert_file(&record("/root/a.txt", 10)).unwrap();
        assert_eq!(db.content_stamp().unwrap(), initial);
        db.upsert_file(&record("/root/a.txt", 11)).unwrap();
        // indexed_at remains an API-visible FileRecord field, so changing it is
        // intentionally a content change until that public contract is revised.
        assert_eq!(db.content_stamp().unwrap().revision, initial.revision + 1);
    }

    #[test]
    fn external_file_writes_and_reused_ids_advance_revision_transactionally() {
        let fixture = Fixture::new("external");
        let db = Database::open(fixture.database()).unwrap();
        db.upsert_file(&record("/root/a.txt", 10)).unwrap();
        let initial = db.content_stamp().unwrap();
        // This connection bypasses every Database write method. The persistent
        // triggers apply equally to sqlite3/Python or any other SQLite client.
        let external = Connection::open(&fixture.database()).unwrap();
        external
            .execute_batch("BEGIN; UPDATE files SET size = 42; ROLLBACK;")
            .unwrap();
        assert_eq!(db.content_stamp().unwrap(), initial);
        external.execute_batch("BEGIN; DELETE FROM files; INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,indexed_at) VALUES(1,'/root/b.txt','/root/b.txt','b.txt','b.txt','file',10); COMMIT;").unwrap();
        let changed = db.content_stamp().unwrap();
        assert_eq!(changed.database_id, initial.database_id);
        assert_eq!(changed.revision, initial.revision + 2);
        assert_eq!(scalar(&db, "SELECT count(*) FROM files"), 1);
        assert_eq!(scalar(&db, "SELECT max(indexed_at) FROM files"), 10);
        assert_eq!(db.all_records().unwrap()[0].path, "/root/b.txt");
        external
            .execute_batch("UPDATE files SET basename = 'renamed.txt';")
            .unwrap();
        assert_eq!(db.content_stamp().unwrap().revision, changed.revision + 1);
    }

    #[test]
    fn read_snapshot_stamp_matches_rows_while_external_wal_writer_commits() {
        let fixture = Fixture::new("snapshot");
        let writer = Database::open(fixture.database()).unwrap();
        writer.upsert_file(&record("/root/a.txt", 10)).unwrap();
        let reader = Database::open_read_only(fixture.database()).unwrap();
        reader.begin_read().unwrap();
        let before = reader.content_stamp().unwrap();
        writer.upsert_file(&record("/root/b.txt", 20)).unwrap();
        assert_eq!(reader.all_records().unwrap().len(), 1);
        assert_eq!(reader.content_stamp().unwrap(), before);
        reader.commit().unwrap();
        assert_eq!(reader.all_records().unwrap().len(), 2);
        assert!(reader.content_stamp().unwrap().revision > before.revision);
        reader.begin_read().unwrap();
        assert_eq!(reader.all_records().unwrap().len(), 2);
        reader.rollback().unwrap();
        writer.upsert_file(&record("/root/c.txt", 30)).unwrap();
        assert_eq!(reader.all_records().unwrap().len(), 3);
    }

    #[test]
    fn observer_sees_wal_content_changes_but_ignores_checkpoint_and_meta() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ContentObserver>();
        let fixture = Fixture::new("wal");
        let db = Database::open(fixture.database()).unwrap();
        db.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let observer = ContentObserver::open(fixture.database()).unwrap();
        let initial = observer.content_stamp().unwrap();
        db.upsert_file(&record("/root/a.txt", 10)).unwrap();
        assert!(
            std::fs::metadata(format!("{}-wal", fixture.database().display()))
                .unwrap()
                .len()
                > 0
        );
        let changed = observer.content_stamp().unwrap();
        assert_eq!(changed.revision, initial.revision + 1);
        db.set_meta("watch_last_event_id", "100").unwrap();
        assert_eq!(observer.content_stamp().unwrap(), changed);
        db.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        assert_eq!(observer.content_stamp().unwrap(), changed);
    }

    #[test]
    fn observer_reopens_replaced_database_even_for_clones_with_equal_revision() {
        let fixture = Fixture::new("replacement");
        let path = fixture.database();
        let original = Database::open(path.clone()).unwrap();
        original
            .upsert_file(&record("/root/original.txt", 1))
            .unwrap();
        original
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(original);
        let observer = ContentObserver::open(path.clone()).unwrap();
        let before = observer.content_stamp().unwrap();
        let replacement_path = fixture.0.join("replacement.sqlite");
        let replacement = Database::open(replacement_path.clone()).unwrap();
        replacement
            .upsert_file(&record("/root/replacement.txt", 1))
            .unwrap();
        drop(replacement);
        std::fs::rename(&replacement_path, &path).unwrap();
        let changed = observer.content_stamp().unwrap();
        assert_ne!(changed.database_id, before.database_id);
        assert_ne!(changed.file_identity, before.file_identity);
        let clone_path = fixture.0.join("clone.sqlite");
        std::fs::copy(&path, &clone_path).unwrap();
        std::fs::rename(&clone_path, &path).unwrap();
        let cloned = observer.content_stamp().unwrap();
        assert_eq!(cloned.database_id, changed.database_id);
        assert_eq!(cloned.revision, changed.revision);
        assert_ne!(cloned.file_identity, changed.file_identity);
        std::fs::rename(&path, &clone_path).unwrap();
        assert!(observer.content_stamp().is_err());
        std::fs::rename(&clone_path, &path).unwrap();
        assert_eq!(observer.content_stamp().unwrap(), cloned);
    }

    #[test]
    fn legacy_database_migrates_once_without_rewriting_files_on_reopen() {
        let fixture = Fixture::new("legacy");
        let legacy = Connection::open(&fixture.database()).unwrap();
        legacy.execute_batch("CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE,basename TEXT NOT NULL,basename_lower TEXT NOT NULL,ext_lower TEXT,kind TEXT NOT NULL,size INTEGER,mtime INTEGER,ctime INTEGER,dev INTEGER,inode INTEGER,indexed_at INTEGER NOT NULL); INSERT INTO files(path,basename,basename_lower,kind,indexed_at) VALUES('/legacy/A.txt','A.txt','a.txt','file',7);").unwrap();
        drop(legacy);
        let migrated = Database::open_existing(fixture.database()).unwrap();
        assert_eq!(
            migrated.all_records().unwrap()[0].path_lower,
            "/legacy/a.txt"
        );
        let stamp = migrated.content_stamp().unwrap();
        assert_eq!(stamp.database_id.len(), 32);
        assert_eq!(stamp.revision, 0);
        let schema_version = scalar(&migrated, "PRAGMA schema_version;");
        drop(migrated);
        let reopened = Database::open(fixture.database()).unwrap();
        assert_eq!(reopened.content_stamp().unwrap(), stamp);
        assert_eq!(scalar(&reopened, "PRAGMA schema_version;"), schema_version);
        assert_eq!(scalar(&reopened, "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'macevery_files_%_changes_v1';"), 5);
        reopened
            .conn
            .execute_batch("UPDATE files SET size=99;")
            .unwrap();
        assert_eq!(
            reopened.content_stamp().unwrap().revision,
            stamp.revision + 1
        );
    }

    #[test]
    fn existing_writable_open_never_creates_a_missing_database_or_parent() {
        let fixture = Fixture::new("existing");
        for missing in [fixture.database(), fixture.0.join("absent/index.sqlite")] {
            assert!(Database::open_existing(missing.clone()).is_err());
            assert!(!missing.exists());
        }
        assert!(!fixture.0.join("absent").exists());
        // Check the low-level flag contract too, independent of the stat guard.
        assert!(Connection::open_existing(&fixture.database()).is_err());
        assert!(!fixture.database().exists());
    }

    #[test]
    fn readonly_open_never_creates_a_missing_database_or_parent() {
        let fixture = Fixture::new("readonly");
        let missing = fixture.0.join("absent/index.sqlite");
        assert!(Database::open_read_only(missing.clone()).is_err());
        assert!(!missing.parent().unwrap().exists());
    }
}

#[cfg(test)]
mod file_change_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "macevery-journal-{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> PathBuf {
            self.0.join("index.sqlite")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn insert(db: &Database, id: i64, path: &str) {
        let mut stmt = db.conn.prepare("INSERT INTO files(id,path,path_lower,basename,basename_lower,kind,size,indexed_at) VALUES(?1,?2,?2,?2,?2,'file',1,10);").unwrap();
        stmt.bind_i64(1, Some(id)).unwrap();
        stmt.bind_text(2, Some(path)).unwrap();
        expect_done(stmt.step().unwrap()).unwrap();
    }
    fn read(db: &Database, base: &ContentStamp, events: usize, ids: usize) -> FileChangeRead {
        db.begin_read().unwrap();
        let delta = db.file_changes_since(base, events, ids).unwrap();
        db.commit().unwrap();
        delta
    }
    fn available(delta: FileChangeRead) -> (ContentStamp, Vec<FileChange>) {
        match delta {
            FileChangeRead::Available { target, changes } => (target, changes),
            other => panic!("expected available delta: {other:?}"),
        }
    }
    fn fallback(delta: FileChangeRead, expected: &str) {
        match delta {
            FileChangeRead::Fallback { reason } => assert_eq!(reason, expected),
            other => panic!("expected {expected}: {other:?}"),
        }
    }
    fn scalar(db: &Database, sql: &str) -> i64 {
        let mut stmt = db.conn.prepare(sql).unwrap();
        assert!(matches!(stmt.step().unwrap(), Step::Row));
        stmt.column_i64(0).unwrap()
    }

    #[test]
    fn delta_requires_transaction_and_metadata_is_empty() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        let base = db.content_stamp().unwrap();
        assert!(db.file_changes_since(&base, 4096, 4096).is_err());
        db.set_meta("watch_last_event_id", "123").unwrap();
        db.set_roots(&["/new-root".to_string()]).unwrap();
        db.set_excludes(&[".git".to_string()]).unwrap();
        let (target, changes) = available(read(&db, &base, 4096, 4096));
        assert_eq!(target, base);
        assert!(changes.is_empty());
    }

    #[test]
    fn final_rows_cover_external_id_changes_delete_reinsert_and_all_fields() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        for (id, path) in [(1, "/a"), (2, "/b"), (3, "/c")] {
            insert(&db, id, path);
        }
        let base = db.content_stamp().unwrap();
        let external = Connection::open_existing(&fixture.path()).unwrap();
        external.execute_batch("BEGIN; UPDATE files SET id=10,path='/renamed',path_lower=NULL,basename='Name',basename_lower='',ext_lower='',kind='app',size=NULL,mtime=-12,ctime=9223372036854775807,dev=-9,inode=88,indexed_at=111 WHERE id=1; DELETE FROM files WHERE id=2; INSERT INTO files(id,path,basename,basename_lower,kind,size,indexed_at) VALUES(2,'/reused','reuse','reuse','dir',222,112); DELETE FROM files WHERE id=3; INSERT INTO files(id,path,basename,basename_lower,kind,indexed_at) VALUES(3,'/temporary','tmp','tmp','file',113); DELETE FROM files WHERE id=3; COMMIT;").unwrap();
        let (target, changes) = available(read(&db, &base, 4096, 4096));
        assert_eq!(target, db.content_stamp().unwrap());
        assert_eq!(
            changes.iter().map(|change| change.id).collect::<Vec<_>>(),
            [1, 2, 3, 10]
        );
        assert!(changes[0].record.is_none() && changes[2].record.is_none());
        let expected = db.all_records().unwrap();
        assert_eq!(
            changes
                .into_iter()
                .filter_map(|change| change.record)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn replace_captures_hidden_path_and_id_victims_without_recursive_triggers() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        for (id, path) in [(1, "/a"), (2, "/b"), (3, "/c"), (4, "/d")] {
            insert(&db, id, path);
        }
        let base = db.content_stamp().unwrap();
        let external = Connection::open_existing(&fixture.path()).unwrap();
        external.execute_batch("PRAGMA recursive_triggers=OFF; BEGIN; INSERT OR REPLACE INTO files(id,path,basename,basename_lower,kind,indexed_at) VALUES(10,'/a','new','new','file',11); DELETE FROM files WHERE id=10; UPDATE OR REPLACE files SET id=4,path='/b' WHERE id=3; COMMIT;").unwrap();
        let (_, changes) = available(read(&db, &base, 4096, 4096));
        assert_eq!(
            changes.iter().map(|change| change.id).collect::<Vec<_>>(),
            [1, 2, 3, 4, 10]
        );
        assert_eq!(
            changes
                .iter()
                .filter_map(|change| change.record.clone())
                .collect::<Vec<_>>(),
            db.all_records().unwrap()
        );
        assert!(changes
            .iter()
            .find(|change| change.id == 4)
            .unwrap()
            .record
            .is_some());
        for id in [1, 2, 3, 10] {
            assert!(changes
                .iter()
                .find(|change| change.id == id)
                .unwrap()
                .record
                .is_none());
        }
        let base = db.content_stamp().unwrap();
        external.execute_batch("INSERT OR REPLACE INTO files(path,basename,basename_lower,kind,indexed_at) VALUES('/b','auto','auto','file',12);").unwrap();
        let (_, changes) = available(read(&db, &base, 4096, 4096));
        assert_eq!(changes.len(), 2);
        assert!(changes
            .iter()
            .find(|change| change.id == 4)
            .unwrap()
            .record
            .is_none());
        assert_eq!(
            changes
                .into_iter()
                .filter_map(|change| change.record)
                .collect::<Vec<_>>(),
            db.all_records().unwrap()
        );
    }

    #[test]
    fn ignored_and_noop_inserts_cannot_leak_scratch_ids() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        insert(&db, 1, "/a");
        insert(&db, 2, "/b");
        let base = db.content_stamp().unwrap();
        db.conn.execute_batch("INSERT OR IGNORE INTO files(id,path,basename,basename_lower,kind,indexed_at) VALUES(100,'/a','ignored','ignored','file',10); INSERT INTO files(id,path,basename,basename_lower,kind,size,indexed_at) VALUES(2,'/b','/b','/b','file',1,10) ON CONFLICT(id) DO UPDATE SET path=excluded.path; UPDATE files SET size=size WHERE id=1;").unwrap();
        assert_eq!(db.content_stamp().unwrap(), base);
        insert(&db, 200, "/fresh");
        let (_, changes) = available(read(&db, &base, 4096, 4096));
        assert_eq!(
            changes.iter().map(|change| change.id).collect::<Vec<_>>(),
            [200]
        );
        let after = db.content_stamp().unwrap();
        db.conn.execute_batch("BEGIN; INSERT OR REPLACE INTO files(path,basename,basename_lower,kind,indexed_at) VALUES('/a','rolled','rolled','file',99); ROLLBACK;").unwrap();
        assert_eq!(db.content_stamp().unwrap(), after);
        assert_eq!(available(read(&db, &after, 4096, 4096)).1.len(), 0);
        assert_eq!(
            scalar(&db, "SELECT COUNT(*) FROM macevery_content_state"),
            1
        );
        db.conn
            .execute_batch(
                "UPDATE OR IGNORE files SET path='/b' WHERE id=1; DELETE FROM files WHERE id=200;",
            )
            .unwrap();
        let (_, changes) = available(read(&db, &after, 4096, 4096));
        assert_eq!(
            changes.iter().map(|change| change.id).collect::<Vec<_>>(),
            [200]
        );
        assert!(changes[0].record.is_none());
    }

    #[test]
    fn delta_and_rows_are_from_one_pinned_wal_snapshot() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        let base = db.content_stamp().unwrap();
        insert(&db, 1, "/a");
        let reader = Database::open_read_only(fixture.path()).unwrap();
        reader.begin_read().unwrap();
        let pinned = reader.content_stamp().unwrap();
        db.conn
            .execute_batch("UPDATE files SET size=222 WHERE id=1;")
            .unwrap();
        let (target, changes) = available(reader.file_changes_since(&base, 4096, 4096).unwrap());
        assert_eq!(target, pinned);
        assert_eq!(changes[0].record.as_ref().unwrap().size, Some(1));
        reader.commit().unwrap();
        let (_, changes) = available(read(&reader, &target, 4096, 4096));
        assert_eq!(changes[0].record.as_ref().unwrap().size, Some(222));
    }

    #[test]
    fn bounds_and_missing_events_force_safe_fallback() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        insert(&db, 1, "/a");
        let base = db.content_stamp().unwrap();
        db.conn
            .execute_batch("UPDATE files SET id=2 WHERE id=1; UPDATE files SET size=7 WHERE id=2;")
            .unwrap();
        fallback(read(&db, &base, 1, 4096), "event_limit");
        fallback(read(&db, &base, 4096, 1), "id_limit");
        db.conn
            .execute_batch(&format!(
                "DELETE FROM macevery_file_changes WHERE revision={};",
                base.revision + 1
            ))
            .unwrap();
        fallback(read(&db, &base, 4096, 4096), "history_gap");
        let mut foreign = base.clone();
        foreign.file_identity.inode = foreign.file_identity.inode.wrapping_add(1);
        fallback(read(&db, &foreign, 4096, 4096), "database_replaced");
        let mut future = base.clone();
        future.revision += 100;
        fallback(read(&db, &future, 4096, 4096), "revision_regressed");
    }

    #[test]
    fn retention_is_bounded_and_pruned_floor_prevents_partial_delta() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        insert(&db, 1, "/a");
        let old = db.content_stamp().unwrap();
        db.begin().unwrap();
        let mut update = db
            .conn
            .prepare("UPDATE files SET size=?1 WHERE id=1;")
            .unwrap();
        for value in 2..(FILE_CHANGE_RETENTION + FILE_CHANGE_PRUNE_INTERVAL * 2) {
            update.reset().unwrap();
            update.bind_i64(1, Some(value)).unwrap();
            expect_done(update.step().unwrap()).unwrap();
        }
        drop(update);
        db.commit().unwrap();
        let count = scalar(&db, "SELECT COUNT(*) FROM macevery_file_changes");
        assert!(count < FILE_CHANGE_RETENTION + FILE_CHANGE_PRUNE_INTERVAL);
        assert!(
            scalar(
                &db,
                "SELECT journal_floor FROM macevery_content_state WHERE singleton=1"
            ) > old.revision
        );
        fallback(read(&db, &old, 4096, 4096), "history_gap");
        let mut recent = db.content_stamp().unwrap();
        recent.revision -= 1000;
        let (_, changes) = available(read(&db, &recent, 4096, 4096));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].id, 1);
        println!(
            "bounded journal retained {count} events after {} file changes",
            db.content_stamp().unwrap().revision
        );
    }

    #[test]
    fn missing_journal_with_surviving_marker_is_repaired_once_with_new_floor() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        insert(&db, 1, "/a");
        let old = db.content_stamp().unwrap();
        db.conn
            .execute_batch("UPDATE files SET size=2 WHERE id=1; DROP TABLE macevery_file_changes;")
            .unwrap();
        assert!(!db.has_change_journal().unwrap());
        fallback(read(&db, &old, 4096, 4096), "journal_unavailable");
        let current = db.content_stamp().unwrap();
        drop(db);
        let repaired = Database::open_existing(fixture.path()).unwrap();
        assert!(repaired.has_change_journal().unwrap());
        assert_eq!(repaired.content_stamp().unwrap(), current);
        fallback(read(&repaired, &old, 4096, 4096), "history_gap");
        repaired
            .conn
            .execute_batch("UPDATE files SET size=3 WHERE id=1;")
            .unwrap();
        let (_, changes) = available(read(&repaired, &current, 4096, 4096));
        assert_eq!(changes[0].record.as_ref().unwrap().size, Some(3));
        let schema = scalar(&repaired, "PRAGMA schema_version");
        drop(repaired);
        let reopened = Database::open_existing(fixture.path()).unwrap();
        assert_eq!(scalar(&reopened, "PRAGMA schema_version"), schema);
    }

    #[test]
    fn delta_payload_cap_counts_effective_strings_before_copying_rows() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        let base = db.content_stamp().unwrap();
        insert(&db, 1, "/İ/ΟΣ");
        insert(&db, 2, "/longer/path");
        db.conn.execute_batch("UPDATE files SET path_lower=NULL,ext_lower='' WHERE id=1; UPDATE files SET path_lower='',ext_lower='a-repeated-extension' WHERE id=2;").unwrap();
        let (_, full) = available(read(&db, &base, 4096, 4096));
        let bytes = full
            .iter()
            .filter_map(|change| change.record.as_ref())
            .map(|row| {
                row.path.len()
                    + row.path_lower.len()
                    + row.basename.len()
                    + row.basename_lower.len()
                    + row.ext_lower.as_ref().map_or(0, String::len)
                    + row.kind.as_str().len() * 3
            })
            .sum::<usize>();
        assert_eq!(FILE_CHANGE_MAX_PAYLOAD_BYTES, 32 * 1024 * 1024);
        db.begin_read().unwrap();
        fallback(
            db.file_changes_since_with_byte_limit(&base, 4096, 4096, bytes - 1)
                .unwrap(),
            "byte_limit",
        );
        db.rollback().unwrap();
        db.begin_read().unwrap();
        let (_, at_limit) = available(
            db.file_changes_since_with_byte_limit(&base, 4096, 4096, bytes)
                .unwrap(),
        );
        db.commit().unwrap();
        assert_eq!(
            at_limit
                .iter()
                .filter_map(|change| change.record.clone())
                .collect::<Vec<_>>(),
            full.iter()
                .filter_map(|change| change.record.clone())
                .collect::<Vec<_>>()
        );
        db.begin_read().unwrap();
        // Even the first oversized row is rejected before row_to_record copies it.
        fallback(
            db.file_changes_since_with_byte_limit(&base, 4096, 4096, 1)
                .unwrap(),
            "byte_limit",
        );
        db.commit().unwrap();
    }

    #[test]
    fn malformed_utf8_cannot_bypass_delta_byte_limit() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        let base = db.content_stamp().unwrap();
        insert(&db, 1, "/a");
        db.conn.execute_batch("UPDATE files SET basename_lower=CAST(x'80ff' AS TEXT),ext_lower=CAST(x'8080' AS TEXT) WHERE id=1;").unwrap();
        let (_, full) = available(read(&db, &base, 4096, 4096));
        let row = full[0].record.as_ref().unwrap();
        let owned_bytes = row.path.len()
            + row.path_lower.len()
            + row.basename.len()
            + row.basename_lower.len()
            + row.ext_lower.as_ref().map_or(0, String::len)
            + row.kind.as_str().len() * 3;
        db.begin_read().unwrap();
        // Current decoding discards invalid fields. The guard still charges
        // their raw bytes, rather than allowing column_str(None) to count zero.
        fallback(
            db.file_changes_since_with_byte_limit(&base, 4096, 4096, owned_bytes + 1)
                .unwrap(),
            "byte_limit",
        );
        db.commit().unwrap();
    }

    #[test]
    fn oversized_unknown_kind_is_budgeted_before_parse() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        let base = db.content_stamp().unwrap();
        insert(&db, 1, "/a");
        let kind = "İ".repeat(4096);
        let mut update = db
            .conn
            .prepare("UPDATE files SET kind=?1 WHERE id=1;")
            .unwrap();
        update.bind_text(1, Some(&kind)).unwrap();
        expect_done(update.step().unwrap()).unwrap();
        drop(update);
        // Four two-byte strings plus the parser's conservative lowercase budget.
        let budget = 8 + kind.len() * 3;
        db.begin_read().unwrap();
        // The first limit exercises the raw guard; the second reaches the
        // Unicode expansion guard. Neither may call the allocating parser.
        for limit in [kind.len() - 1, budget - 1] {
            fallback(
                db.file_changes_since_with_byte_limit(&base, 4096, 4096, limit)
                    .unwrap(),
                "byte_limit",
            );
        }
        let (_, changes) = available(
            db.file_changes_since_with_byte_limit(&base, 4096, 4096, budget)
                .unwrap(),
        );
        assert_eq!(changes[0].record.as_ref().unwrap().kind, FileKind::Other);
        db.commit().unwrap();
    }

    #[test]
    fn full_record_iteration_has_explicit_signed_id_order() {
        let fixture = Fixture::new();
        let db = Database::open(fixture.path()).unwrap();
        for (id, path) in [(10, "/z"), (-5, "/a"), (0, "/m")] {
            insert(&db, id, path);
        }
        assert_eq!(
            db.all_records()
                .unwrap()
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [-5, 0, 10]
        );
        let mut ids = Vec::new();
        db.for_each_record_ref(|r| {
            ids.push(r.id);
            Ok(())
        })
        .unwrap();
        assert_eq!(ids, [-5, 0, 10]);
    }
}
