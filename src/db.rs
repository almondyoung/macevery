use crate::error::{MacEveryError, Result};
use crate::model::{FileKind, FileRecord, IndexStats, PathFilter, PathFilterMode, SearchOptions};
use crate::sqlite::{Connection, Step};
use std::path::{Path, PathBuf};

const DELETE_PATH_PREFIX_SQL: &str = "DELETE FROM files
    WHERE path = ?1 COLLATE BINARY
       OR (path >= ?2 COLLATE BINARY AND path < ?3 COLLATE BINARY);";
const DELETE_STALE_PREFIX_SQL: &str = "DELETE FROM files
    WHERE indexed_at < ?1
      AND (path = ?2 COLLATE BINARY
           OR (path >= ?3 COLLATE BINARY AND path < ?4 COLLATE BINARY));";

pub struct Database {
    path: PathBuf,
    conn: Connection,
}

pub struct DbRecordRef<'a> {
    pub id: i64,
    pub path: &'a str,
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
        let conn = Connection::open(&path)?;
        let db = Self { path, conn };
        db.init()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
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
        Ok(())
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
