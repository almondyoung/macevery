use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, IndexStats, PathFilter, PathFilterMode, SearchOptions};
use crate::sqlite::{Connection, Step};
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
             FROM files;",
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
             FROM files;",
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
        .column_text(6)
        .and_then(|value| FileKind::parse(&value))
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
        assert_eq!(scalar(&reopened, "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'macevery_files_%_revision_v1';"), 3);
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
