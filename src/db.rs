use crate::error::{MacEveryError, Result};
use crate::model::{
    BootstrapStatus, FileKind, FileRecord, IndexStats, PathFilter, PathFilterMode, SearchOptions,
};
use crate::sqlite::{Connection, Statement, Step};
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const DELETE_PATH_PREFIX_SQL: &str = "DELETE FROM files
    WHERE path = ?1 COLLATE BINARY
       OR (path >= ?2 COLLATE BINARY AND path < ?3 COLLATE BINARY);";
const DELETE_STALE_PREFIX_SQL: &str = "DELETE FROM files
    WHERE indexed_at < ?1
      AND (path = ?2 COLLATE BINARY
           OR (path >= ?3 COLLATE BINARY AND path < ?4 COLLATE BINARY));";
const SCANNER_CANDIDATES_SQL: &str = "SELECT id, path FROM files
    WHERE path >= ?1 COLLATE BINARY AND path < ?2 COLLATE BINARY
      AND (path > ?1 COLLATE BINARY OR ?5)
      AND (path = ?3 COLLATE BINARY OR path >= ?4 COLLATE BINARY)
    ORDER BY path COLLATE BINARY LIMIT 256;";

const FILE_UPSERT_SQL: &str = "INSERT INTO files (
    path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime,
    dev, inode, indexed_at
) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
WHERE NOT EXISTS (
    SELECT 1 FROM files WHERE path = ?1 COLLATE BINARY
      AND indexed_at IS ?12
      AND path_lower IS ?2 AND basename IS ?3 AND basename_lower IS ?4
      AND ext_lower IS ?5 AND kind IS ?6 AND size IS ?7 AND mtime IS ?8
      AND ctime IS ?9 AND dev IS ?10 AND inode IS ?11
)
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
    indexed_at=excluded.indexed_at;";

// Scanner generations describe the last observed content change, not every
// successful visit. The generic upsert above retains its indexed_at contract.
const SCANNER_FILE_UPSERT_SQL: &str = "INSERT INTO files (
    path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime,
    dev, inode, indexed_at
) SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12
WHERE NOT EXISTS (
    SELECT 1 FROM files WHERE path = ?1 COLLATE BINARY
      AND path_lower IS ?2 AND basename IS ?3 AND basename_lower IS ?4
      AND ext_lower IS ?5 AND kind IS ?6 AND size IS ?7 AND mtime IS ?8
      AND ctime IS ?9 AND dev IS ?10 AND inode IS ?11
)
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
    indexed_at=excluded.indexed_at;";

// The scanner can prove a no-op and obtain its exact persisted ID in one read.
// Keep the material-field predicate identical to SCANNER_FILE_UPSERT_SQL;
// indexed_at remains the last content-change generation for scanner visits.
const SCANNER_UNCHANGED_SQL: &str = "SELECT id FROM files
    WHERE path = ?1 COLLATE BINARY
      AND path_lower IS ?2 AND basename IS ?3 AND basename_lower IS ?4
      AND ext_lower IS ?5 AND kind IS ?6 AND size IS ?7 AND mtime IS ?8
      AND ctime IS ?9 AND dev IS ?10 AND inode IS ?11;";

const SEEN_PAGE_BITS: i64 = 4096;
/// At most 2 MiB of bit payload plus bounded map/allocation overhead. Dense
/// seven-million-row indexes need about 0.9 MiB; sparse IDs cannot grow it
/// without bound. Exhaustion causes caller rollback and a legacy rescan.
pub(crate) const SCANNER_SEEN_MAX_PAGES: usize = 4096;

pub(crate) struct SeenIds {
    pages: BTreeMap<i64, Box<[u64; 64]>>,
    max_pages: usize,
}

impl SeenIds {
    fn new(max_pages: usize) -> Self {
        Self {
            pages: BTreeMap::new(),
            max_pages,
        }
    }

    fn insert(&mut self, id: i64) -> bool {
        // Euclidean division also covers negative IDs and i64::MIN without
        // abs/negation overflow. Membership is exact, never probabilistic.
        let page = id.div_euclid(SEEN_PAGE_BITS);
        let bit = id.rem_euclid(SEEN_PAGE_BITS) as usize;
        let can_add_page = self.pages.len() < self.max_pages;
        let words = match self.pages.entry(page) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                if !can_add_page {
                    return false;
                }
                entry.insert(Box::new([0; 64]))
            }
        };
        words[bit / 64] |= 1 << (bit % 64);
        true
    }

    fn contains(&self, id: i64) -> bool {
        let page = id.div_euclid(SEEN_PAGE_BITS);
        let bit = id.rem_euclid(SEEN_PAGE_BITS) as usize;
        self.pages
            .get(&page)
            .is_some_and(|words| words[bit / 64] & (1 << (bit % 64)) != 0)
    }
}

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

/// Reuse one prepared statement across a caller-owned write transaction.
/// Transaction boundaries and error rollback remain the caller's responsibility.
pub struct FileUpsert<'db> {
    conn: &'db Connection,
    statement: Statement<'db>,
    scanner: Option<ScannerUpsert<'db>>,
}

struct ScannerUpsert<'db> {
    unchanged_statement: Statement<'db>,
    id_statement: Statement<'db>,
    seen: SeenIds,
    exceeded: bool,
}

impl FileUpsert<'_> {
    /// Return whether the outer files statement inserted or updated a row.
    /// The input ID is intentionally ignored, preserving upsert_file's existing
    /// path-keyed identity contract. The generic writer treats a generation
    /// change as a row change; the scanner writer preserves unchanged rows.
    pub fn upsert(&mut self, record: &FileRecord) -> Result<bool> {
        if let Some(scanner) = &mut self.scanner {
            scanner.unchanged_statement.reset()?;
            bind_file_content(&mut scanner.unchanged_statement, record)?;
            let unchanged_id = match scanner.unchanged_statement.step() {
                Ok(Step::Row) => {
                    Some(scanner.unchanged_statement.column_i64(0).ok_or_else(|| {
                        MacEveryError::Sqlite("scanner probe returned a NULL file ID".into())
                    })?)
                }
                Ok(Step::Done) => None,
                Err(error) => {
                    let _ = scanner.unchanged_statement.reset();
                    return Err(error);
                }
            };
            // Never hold this read cursor across the fallback write or cleanup.
            scanner.unchanged_statement.reset()?;
            if let Some(id) = unchanged_id {
                scanner.exceeded |= !scanner.seen.insert(id);
                return Ok(false);
            }
        }
        self.statement.reset()?;
        bind_file_content(&mut self.statement, record)?;
        self.statement.bind_i64(12, Some(record.indexed_at))?;
        if let Err(error) = self.statement.step().and_then(expect_done) {
            // sqlite3_reset resets the VM but can report its previous step
            // error. Consume that report now, preserving the original error,
            // so the first legal retry after caller rollback can bind normally.
            let _ = self.statement.reset();
            return Err(error);
        }
        let changed = self.conn.changes() != 0;
        if let Some(scanner) = &mut self.scanner {
            scanner.id_statement.reset()?;
            scanner.id_statement.bind_text(1, Some(&record.path))?;
            let id = match scanner.id_statement.step()? {
                Step::Row => scanner.id_statement.column_i64(0).ok_or_else(|| {
                    MacEveryError::Sqlite("scanner upsert returned a NULL file ID".into())
                })?,
                Step::Done => {
                    return Err(MacEveryError::Sqlite(
                        "scanner upsert did not retain its path".into(),
                    ))
                }
            };
            scanner.exceeded |= !scanner.seen.insert(id);
            // Do not retain an active read cursor while later writes run.
            scanner.id_statement.reset()?;
        }
        Ok(changed)
    }

    pub(crate) fn seen_limit_exceeded(&self) -> bool {
        self.scanner
            .as_ref()
            .is_some_and(|scanner| scanner.exceeded)
    }

    pub(crate) fn into_seen_ids(self) -> Option<SeenIds> {
        self.scanner.map(|scanner| scanner.seen)
    }
}

fn bind_file_content(statement: &mut Statement<'_>, record: &FileRecord) -> Result<()> {
    statement.bind_text(1, Some(&record.path))?;
    statement.bind_text(2, Some(&record.path_lower))?;
    statement.bind_text(3, Some(&record.basename))?;
    statement.bind_text(4, Some(&record.basename_lower))?;
    statement.bind_text(5, record.ext_lower.as_deref())?;
    statement.bind_text(6, Some(record.kind.as_str()))?;
    statement.bind_i64(7, record.size)?;
    statement.bind_i64(8, record.mtime)?;
    statement.bind_i64(9, record.ctime)?;
    statement.bind_i64(10, record.dev)?;
    statement.bind_i64(11, record.inode)
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

#[derive(Debug, Eq, PartialEq)]
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

#[derive(Debug, Eq, PartialEq)]
struct CompactFilePin {
    identity: DatabaseFileIdentity,
    bytes: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl CompactFilePin {
    fn at(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            identity: DatabaseFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            bytes: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CompactControlPin {
    database_id: String,
    revision: i64,
    journal_floor: i64,
    roots: Vec<String>,
    excludes: Vec<String>,
    metadata: Vec<[String; 4]>,
}

fn compact_staging_directory(parent: &Path) -> Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..32 {
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = parent.join(format!(
            ".macevery-compact-{}-{timestamp}-{serial}",
            std::process::id()
        ));
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(MacEveryError::Cli(
        "cannot allocate a private compact-copy staging directory".into(),
    ))
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

    /// Explicit maintenance only: write a fresh, compact logical copy without
    /// migrating, replacing or writing the source. Failed staging is retained
    /// for diagnosis and must not be installed. Source activity detected across
    /// the copy rejects success; callers should freeze writers before using it.
    /// Returns the number of complete FileRecord rows compared in ID order.
    pub fn write_compacted_copy(&self, output: &Path) -> Result<usize> {
        if !self.conn.is_autocommit() || self.conn.has_active_statements() {
            return Err(MacEveryError::Cli(
                "cannot compact while the caller has an active transaction or statement".into(),
            ));
        }
        if output.as_os_str().is_empty() {
            return Err(MacEveryError::Cli(
                "compacted output path must not be empty".into(),
            ));
        }
        let name = output
            .file_name()
            .ok_or_else(|| MacEveryError::Cli("compacted output must name a fresh file".into()))?;
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // Absolute ordinary filenames cannot be reinterpreted as file: URIs by
        // SQLite, and a canonical parent resolves directory/symlink aliases.
        let output = parent.canonicalize()?.join(name);
        output
            .to_str()
            .filter(|value| !value.contains('\0'))
            .ok_or_else(|| {
                MacEveryError::Cli("compacted output must be a UTF-8 path without NUL".into())
            })?;
        match std::fs::symlink_metadata(&output) {
            Ok(_) => return Err(MacEveryError::Cli("compacted output already exists".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let source_file = CompactFilePin::at(&self.path)?;
        if source_file.identity != self.identity {
            return Err(MacEveryError::Sqlite(
                "source database was replaced before compact copy".into(),
            ));
        }
        // A currently absent source WAL/SHM/journal is not a valid destination.
        // Checking its corresponding main file also catches hardlink, case and
        // filesystem Unicode-normalization aliases without creating anything.
        let output_name = name.to_str().ok_or_else(|| {
            MacEveryError::Cli("compacted output must be a UTF-8 filename".into())
        })?;
        for suffix in ["-wal", "-shm", "-journal"] {
            if let Some(tail) = output_name.get(output_name.len().saturating_sub(suffix.len())..) {
                if tail.eq_ignore_ascii_case(suffix) {
                    let main_name = &output_name[..output_name.len() - suffix.len()];
                    match DatabaseFileIdentity::at(&output.with_file_name(main_name)) {
                        Ok(identity) if identity == self.identity => {
                            return Err(MacEveryError::Cli(
                                "compacted output must not be a source database sidecar".into(),
                            ))
                        }
                        Ok(_) => {}
                        Err(MacEveryError::Io(error))
                            if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            let mut sidecar = output.as_os_str().to_owned();
            sidecar.push(suffix);
            match std::fs::symlink_metadata(PathBuf::from(sidecar)) {
                Ok(_) => {
                    return Err(MacEveryError::Cli(
                        "compacted output has an existing SQLite sidecar".into(),
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let source_path = self.path.canonicalize()?;
        if source_path.to_str().is_none() {
            return Err(MacEveryError::Cli(
                "compact source must be a UTF-8 path".into(),
            ));
        }
        let source = Self::open_read_only(source_path)?;
        if source.identity != self.identity {
            return Err(MacEveryError::Sqlite(
                "source database was replaced while opening compact copy".into(),
            ));
        }
        source
            .conn
            .execute_batch("PRAGMA temp_store=FILE; PRAGMA synchronous=FULL;")?;
        // Some system SQLite builds reject even an existing empty INTO target.
        // Let SQLite create an absent filename inside our private directory;
        // publish only the verified inode with a no-clobber hard link.
        let staging = compact_staging_directory(output.parent().ok_or_else(|| {
            MacEveryError::Cli("compacted output needs a parent directory".into())
        })?)?;
        let staged = staging.join("index.sqlite");
        let result = (|| {
            let staged_text = staged
                .to_str()
                .ok_or_else(|| MacEveryError::Cli("compacted staging path must be UTF-8".into()))?;
            let before = source.compact_control_pin()?;
            let mut vacuum = source.conn.prepare("VACUUM INTO ?1;")?;
            vacuum.bind_text(1, Some(staged_text))?;
            expect_done(vacuum.step()?)?;
            drop(vacuum);
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))?;
            let destination = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staged)?;
            destination.sync_all()?;
            let descriptor = destination.metadata()?;
            let destination_identity = DatabaseFileIdentity {
                device: descriptor.dev(),
                inode: descriptor.ino(),
            };
            if DatabaseFileIdentity::at(&staged)? != destination_identity {
                return Err(MacEveryError::Sqlite(
                    "compacted output was replaced during copy".into(),
                ));
            }
            let copied_file = CompactFilePin::at(&staged)?;
            let compacted = Self::open_read_only(staged.clone())?;
            let mut check = compacted.conn.prepare("PRAGMA quick_check;")?;
            if !matches!(check.step()?, Step::Row)
                || check.column_str(0) != Some("ok")
                || !matches!(check.step()?, Step::Done)
            {
                return Err(MacEveryError::Sqlite(
                    "compacted output quick_check failed".into(),
                ));
            }
            drop(check);
            let copied = compacted.compact_control_pin()?;
            let verified_records = source.verify_compacted_records(&compacted)?;
            let after = source.compact_control_pin()?;
            let copied_after = compacted.compact_control_pin()?;
            if before != after
                || copied != before
                || copied_after != before
                || CompactFilePin::at(&self.path)? != source_file
            {
                return Err(MacEveryError::Sqlite("source identity, revision or control metadata changed during compact copy; output retained".into()));
            }
            if CompactFilePin::at(&staged)? != copied_file {
                return Err(MacEveryError::Sqlite(
                    "compacted output was replaced during validation".into(),
                ));
            }
            drop(compacted);
            let staged_wal = PathBuf::from(format!("{}-wal", staged.display()));
            match std::fs::metadata(staged_wal) {
                Ok(metadata) if metadata.len() != 0 => {
                    return Err(MacEveryError::Sqlite(
                        "compacted staging still has WAL data; refusing main-file-only publication"
                            .into(),
                    ))
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            // hard_link fails if any destination object appeared after preflight;
            // it never overwrites an existing file or follows a dangling symlink.
            std::fs::hard_link(&staged, &output)?;
            if CompactFilePin::at(&output)? != copied_file {
                return Err(MacEveryError::Sqlite(
                    "published compacted output changed identity".into(),
                ));
            }
            drop(destination);
            std::fs::remove_dir_all(&staging)?;
            Ok(verified_records)
        })();
        result.map_err(|error: MacEveryError| {
            MacEveryError::Sqlite(format!(
                "compact copy failed; staging retained at {}: {error}",
                staging.display()
            ))
        })
    }

    fn verify_compacted_records(&self, compacted: &Self) -> Result<usize> {
        let sql = "SELECT id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at FROM files ORDER BY id;";
        let mut source = self.conn.prepare(sql)?;
        let mut copied = compacted.conn.prepare(sql)?;
        let mut records = 0usize;
        loop {
            match (source.step()?, copied.step()?) {
                (Step::Done, Step::Done) => return Ok(records),
                (Step::Row, Step::Row) => {
                    records = records.checked_add(1).ok_or_else(|| {
                        MacEveryError::Sqlite("compacted record count overflow".into())
                    })?;
                    // Both rows remain borrowed only until the next pair of
                    // steps. This checks all FileRecord fields without a Vec
                    // or result/path materialization proportional to the index.
                    if row_to_record_ref(&source)? != row_to_record_ref(&copied)? {
                        return Err(MacEveryError::Sqlite(format!(
                            "compacted records differ at ordinal {records}; output retained"
                        )));
                    }
                }
                _ => {
                    return Err(MacEveryError::Sqlite(format!(
                        "compacted record counts differ after ordinal {records}; output retained"
                    )))
                }
            }
        }
    }

    fn compact_control_pin(&self) -> Result<CompactControlPin> {
        self.with_read_snapshot(|| {
            let stamp = self.content_stamp()?;
            let (roots, excludes) = self.roots_and_excludes()?;
            let mut floor = self.conn.prepare("SELECT journal_floor FROM macevery_content_state WHERE singleton=1;")?;
            if !matches!(floor.step()?, Step::Row) {
                return Err(MacEveryError::Sqlite("missing journal floor during compact copy".into()));
            }
            let journal_floor = floor.column_i64(0).ok_or_else(|| MacEveryError::Sqlite("invalid journal floor during compact copy".into()))?;
            let mut values = self.conn.prepare("SELECT typeof(key),hex(CAST(key AS BLOB)),typeof(value),hex(CAST(value AS BLOB)) FROM meta ORDER BY 1,2,3,4;")?;
            let mut metadata = Vec::new();
            while let Step::Row = values.step()? {
                let mut row = std::array::from_fn(|_| String::new());
                for (column, value) in row.iter_mut().enumerate() {
                    *value = values.column_text(column as i32).ok_or_else(|| MacEveryError::Sqlite("invalid control metadata during compact copy".into()))?;
                }
                metadata.push(row);
            }
            Ok(CompactControlPin { database_id: stamp.database_id, revision: stamp.revision, journal_floor, roots, excludes, metadata })
        })
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

    /// Keep multi-pass searches in one snapshot without committing or rolling
    /// back a transaction owned by the caller.
    pub(crate) fn with_read_snapshot<T>(&self, read: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return read();
        }
        self.begin_read()?;
        let result = read().and_then(|value| {
            self.commit()?;
            Ok(value)
        });
        if result.is_err() {
            let _ = self.rollback();
        }
        result
    }

    /// Raw BLOB index predicates share column_str's bytes only in UTF-8
    /// databases. UTF-16 BLOB-to-text conversion can change the byte sequence.
    pub(crate) fn uses_utf8_encoding(&self) -> Result<bool> {
        let mut stmt = self.conn.prepare("PRAGMA encoding;")?;
        Ok(matches!(stmt.step()?, Step::Row)
            && stmt
                .column_str(0)
                .is_some_and(|encoding| encoding.eq_ignore_ascii_case("UTF-8")))
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

    pub fn prepare_file_upsert(&self) -> Result<FileUpsert<'_>> {
        // Filter identical input before INSERT, not only in DO UPDATE: even a
        // suppressed UPDATE would already have run the BEFORE INSERT scratch
        // trigger. All persisted input fields, including indexed_at, participate.
        Ok(FileUpsert {
            conn: &self.conn,
            statement: self.conn.prepare(FILE_UPSERT_SQL)?,
            scanner: None,
        })
    }

    pub(crate) fn prepare_scanner_upsert(&self, max_pages: usize) -> Result<FileUpsert<'_>> {
        Ok(FileUpsert {
            conn: &self.conn,
            statement: self.conn.prepare(SCANNER_FILE_UPSERT_SQL)?,
            scanner: Some(ScannerUpsert {
                unchanged_statement: self.conn.prepare(SCANNER_UNCHANGED_SQL)?,
                id_statement: self
                    .conn
                    .prepare("SELECT id FROM files WHERE path = ?1 COLLATE BINARY;")?,
                seen: SeenIds::new(max_pages),
                exceeded: false,
            }),
        })
    }

    pub fn upsert_file(&self, record: &FileRecord) -> Result<()> {
        self.prepare_file_upsert()?.upsert(record).map(|_| ())
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

    /// Delete only rows absent from a complete scanner visit. Caller owns a
    /// write transaction, so another writer cannot replace/reuse IDs between
    /// membership checks and deletion. Read cursors are closed before DELETE:
    /// mutating a table while stepping its SELECT has undefined visibility.
    pub(crate) fn delete_unseen_under_roots(
        &self,
        roots: &[PathBuf],
        seen: &SeenIds,
    ) -> Result<u64> {
        const BATCH: usize = 256;
        let mut delete = self.conn.prepare("DELETE FROM files WHERE id = ?1;")?;
        let mut deleted = 0;
        for root in roots {
            let Some((root, lower, upper)) = path_prefix_bounds(&root.to_string_lossy()) else {
                continue;
            };
            let mut cursor = root.clone();
            let mut first = true;
            loop {
                let mut stale = Vec::new();
                let mut visited = 0;
                {
                    // Keyset pagination follows the unique binary path index;
                    // neither a whole-scope ID sort nor a full ID set is needed.
                    let mut select = self.conn.prepare(SCANNER_CANDIDATES_SQL)?;
                    select.bind_text(1, Some(&cursor))?;
                    select.bind_text(2, Some(&upper))?;
                    select.bind_text(3, Some(&root))?;
                    select.bind_text(4, Some(&lower))?;
                    select.bind_i64(5, Some(i64::from(first)))?;
                    while let Step::Row = select.step()? {
                        let id = select.column_i64(0).ok_or_else(|| {
                            MacEveryError::Sqlite("NULL ID during scanner cleanup".into())
                        })?;
                        cursor = select.column_text(1).ok_or_else(|| {
                            MacEveryError::Sqlite("NULL path during scanner cleanup".into())
                        })?;
                        if !seen.contains(id) {
                            stale.push(id);
                        }
                        visited += 1;
                    }
                }
                for id in stale {
                    delete.reset()?;
                    delete.bind_i64(1, Some(id))?;
                    expect_done(delete.step()?)?;
                    deleted += self.conn.changes();
                }
                if visited < BATCH {
                    break;
                }
                first = false;
            }
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

    /// A superset of Rust basename-prefix matches for ASCII [lower, upper).
    /// TEXT and explicit BLOB values are disjoint indexed ranges: column_str
    /// decodes either when valid UTF-8. No SQL sort or LIMIT materializes the
    /// prefix set, and the caller applies the exact Rust scorer and filters.
    pub(crate) fn for_each_basename_prefix_ref<F>(
        &self,
        lower: &str,
        upper: &str,
        mut visit: F,
    ) -> Result<()>
    where
        F: FnMut(DbRecordRef<'_>) -> Result<()>,
    {
        for blob in [false, true] {
            let bounds = if blob {
                "basename_lower >= CAST(?1 AS BLOB) COLLATE BINARY
                 AND basename_lower < CAST(?2 AS BLOB) COLLATE BINARY"
            } else {
                "basename_lower >= ?1 COLLATE BINARY AND basename_lower < ?2 COLLATE BINARY"
            };
            let mut stmt = self.conn.prepare(&format!(
                "SELECT id, path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime, dev, inode, indexed_at
                 FROM files WHERE {bounds};"
            ))?;
            stmt.bind_text(1, Some(lower))?;
            stmt.bind_text(2, Some(upper))?;
            while let Step::Row = stmt.step()? {
                visit(row_to_record_ref(&stmt)?)?;
            }
        }
        Ok(())
    }

    /// Scan the smaller basename index, fetching complete rows only when its
    /// raw bytes contain a necessary literal. instr(BLOB,BLOB) preserves Rust
    /// byte semantics for Unicode and embedded NULs, unlike LIKE wildcards.
    /// This is only a superset: every row is still checked by the Rust scorer.
    pub(crate) fn for_each_basename_literal_ref<F>(&self, literal: &str, mut visit: F) -> Result<()>
    where
        F: FnMut(DbRecordRef<'_>) -> Result<()>,
    {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime, dev, inode, indexed_at
             FROM files INDEXED BY idx_files_basename_lower
             WHERE instr(CAST(basename_lower AS BLOB), CAST(?1 AS BLOB)) > 0;",
        )?;
        stmt.bind_text(1, Some(literal))?;
        while let Step::Row = stmt.step()? {
            visit(row_to_record_ref(&stmt)?)?;
        }
        Ok(())
    }

    /// Complete positive-extension candidates, with disjoint storage classes.
    /// The caller has checked UTF-8 encoding and rechecks all Rust filters.
    /// False means the bind list is unsuitable; it never means zero matches.
    pub(crate) fn for_each_extension_ref<F>(
        &self,
        extensions: &[String],
        mut visit: F,
    ) -> Result<bool>
    where
        F: FnMut(DbRecordRef<'_>) -> Result<()>,
    {
        if extensions.is_empty()
            || extensions.len() > 128
            || extensions.iter().any(|value| value.contains('\0'))
        {
            return Ok(false);
        }
        let plain = placeholders(extensions.len());
        let blobs = (0..extensions.len())
            .map(|_| "CAST(? AS BLOB)")
            .collect::<Vec<_>>()
            .join(",");
        for predicate in [
            format!("typeof(ext_lower)='text' AND ext_lower IN ({plain})"),
            format!("typeof(ext_lower)='blob' AND ext_lower IN ({blobs})"),
            format!(
                "ext_lower < '' COLLATE BINARY AND typeof(ext_lower) IN ('integer','real')
                 AND CAST(ext_lower AS TEXT) IN ({plain})"
            ),
        ] {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT id, path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime, dev, inode, indexed_at
                 FROM files INDEXED BY idx_files_ext_lower WHERE {predicate};"
            ))?;
            for (index, value) in extensions.iter().enumerate() {
                stmt.bind_text(index as i32 + 1, Some(value))?;
            }
            while let Step::Row = stmt.step()? {
                visit(row_to_record_ref(&stmt)?)?;
            }
        }
        Ok(true)
    }

    /// Let a conservative connection-local function filter the covering path
    /// index, returning only candidate IDs across the SQLite/Rust boundary.
    /// NULL/empty/invalid UTF-8 fetch the original path for Unicode fallback.
    /// If a complete basename pass preceded this, skip exactly its Rust set.
    pub(crate) fn for_each_path_literal_ref<F>(
        &self,
        literal: &str,
        skip_basename_literal: bool,
        mut visit: F,
    ) -> Result<()>
    where
        F: FnMut(DbRecordRef<'_>) -> Result<()>,
    {
        self.conn.register_path_literal_filter()?;
        let mut paths = self.conn.prepare(
            "SELECT id FROM files INDEXED BY idx_files_path_lower
             WHERE macevery_path_may_contain(path_lower,?1);",
        )?;
        paths.bind_text(1, Some(literal))?;
        let mut full = self.conn.prepare(
            "SELECT id, path, path_lower, basename, basename_lower, ext_lower, kind, size, mtime, ctime, dev, inode, indexed_at
             FROM files WHERE id=?1;",
        )?;
        while let Step::Row = paths.step()? {
            full.reset()?;
            full.bind_i64(1, paths.column_i64(0))?;
            if !matches!(full.step()?, Step::Row) {
                return Err(MacEveryError::Sqlite(
                    "path-index candidate disappeared from search snapshot".into(),
                ));
            }
            let row = row_to_record_ref(&full)?;
            if !skip_basename_literal || !row.basename_lower.contains(literal) {
                visit(row)?;
            }
        }
        Ok(())
    }

    /// Read the startup decision without counting indexed files. The first
    /// SELECT pins the snapshot used for all configuration and metadata reads.
    pub fn bootstrap_status(&self) -> Result<BootstrapStatus> {
        self.begin_read()?;
        let result = self.bootstrap_status_in_snapshot().and_then(|status| {
            self.commit()?;
            Ok(status)
        });
        if result.is_err() {
            let _ = self.rollback();
        }
        result
    }

    fn bootstrap_status_in_snapshot(&self) -> Result<BootstrapStatus> {
        let mut stmt = self
            .conn
            .prepare("SELECT EXISTS(SELECT 1 FROM files LIMIT 1);")?;
        if !matches!(stmt.step()?, Step::Row) {
            return Err(MacEveryError::Sqlite(
                "missing indexed-record existence result".to_string(),
            ));
        }
        let has_records = stmt.column_i64(0).ok_or_else(|| {
            MacEveryError::Sqlite("invalid indexed-record existence result".into())
        })? != 0;
        drop(stmt);

        let (roots, excludes) = self.roots_and_excludes()?;
        let last_indexed_at = self
            .get_meta("last_indexed_at")?
            .and_then(|value| value.parse().ok());
        Ok(BootstrapStatus {
            db_path: self.path.to_string_lossy().to_string(),
            has_records,
            roots,
            excludes,
            last_indexed_at,
        })
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

        (stats.roots, stats.excludes) = self.roots_and_excludes()?;
        Ok(stats)
    }

    /// Read watch configuration without scanning or counting indexed files.
    pub fn roots_and_excludes(&self) -> Result<(Vec<String>, Vec<String>)> {
        Ok((
            self.string_list("SELECT path FROM roots ORDER BY path;")?,
            self.string_list("SELECT pattern FROM excludes ORDER BY pattern;")?,
        ))
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

    #[test]
    fn scanner_seen_bitmap_is_exact_for_signed_extremes_and_bounded_sparse_pages() {
        let mut seen = SeenIds::new(4);
        for id in [i64::MIN, -1, 0, i64::MAX] {
            assert!(seen.insert(id));
            assert!(seen.insert(id), "duplicates do not consume more pages");
            assert!(seen.contains(id));
        }
        assert_eq!(seen.pages.len(), 4);
        for id in [i64::MIN + 1, -2, 1, i64::MAX - 1, 4096] {
            assert!(!seen.contains(id), "unvisited bits never produce a match");
        }
        assert!(!seen.insert(4096));
        assert!(!seen.contains(4096));
        assert!(seen.insert(1), "a new bit on an admitted page still fits");
        assert!(seen.contains(1));
        let mut dense = SeenIds::new(3);
        for id in -4096..8192 {
            assert!(dense.insert(id));
        }
        assert_eq!(dense.pages.len(), 3);
        assert!(!dense.insert(8192));
    }

    #[test]
    fn scanner_cleanup_candidates_seek_the_path_index_without_temp_sort() {
        let fixture = Fixture::new("scanner-keyset-plan");
        let db = Database::open(fixture.database()).unwrap();
        for first in [false, true] {
            let mut stmt = db
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {SCANNER_CANDIDATES_SQL}"))
                .unwrap();
            for (index, value) in ["/root/目录", "/root0", "/root", "/root/"]
                .into_iter()
                .enumerate()
            {
                stmt.bind_text(index as i32 + 1, Some(value)).unwrap();
            }
            stmt.bind_i64(5, Some(i64::from(first))).unwrap();
            let mut plan = Vec::new();
            while let Step::Row = stmt.step().unwrap() {
                plan.push(stmt.column_text(3).unwrap());
            }
            assert!(plan.iter().any(|line| line.contains("SEARCH files") && line.contains("path>? AND path<?")), "{plan:?}");
            assert!(
                !plan
                    .iter()
                    .any(|line| line.contains("SCAN files") || line.contains("TEMP B-TREE")),
                "{plan:?}"
            );
        }
    }

    #[test]
    fn scanner_upsert_preserves_generation_without_dml_and_tracks_actual_replaced_id() {
        let fixture = Fixture::new("scanner-content-noop");
        let db = Database::open(fixture.database()).unwrap();
        let mut value = record("/root/目录/seen", 7);
        db.upsert_file(&value).unwrap();
        db.conn
            .execute_batch("UPDATE files SET id=-9223372036854775808;")
            .unwrap();
        let original = db.all_records().unwrap()[0].clone();
        let stamp = db.content_stamp().unwrap();
        let total = scalar(&db, "SELECT total_changes();");
        value.indexed_at = 99;
        value.id = i64::MAX; // caller identity is still ignored
        db.begin().unwrap();
        let mut writer = db.prepare_scanner_upsert(4).unwrap();
        assert!(!writer.upsert(&value).unwrap());
        assert_eq!(scalar(&db, "SELECT total_changes();"), total);
        assert_eq!(db.content_stamp().unwrap(), stamp);
        assert_eq!(db.all_records().unwrap(), [original]);
        // External SQL may have assigned a new/sparse/signed identity before
        // the scanner transaction. Never infer it from last_insert_rowid/input.
        db.conn
            .execute_batch("UPDATE files SET id=9223372036854775807;")
            .unwrap();
        assert!(!writer.upsert(&value).unwrap());
        value.size = Some(123);
        assert!(writer.upsert(&value).unwrap());
        let seen = writer.into_seen_ids().unwrap();
        assert!(seen.contains(i64::MIN));
        assert!(seen.contains(i64::MAX));
        let actual = db.all_records().unwrap().pop().unwrap();
        assert_eq!(actual.id, i64::MAX);
        assert_eq!(actual.indexed_at, 99);
        db.commit().unwrap();
        let stamp = db.content_stamp().unwrap();
        value.indexed_at = 100;
        db.upsert_file(&value).unwrap();
        assert_eq!(
            db.content_stamp().unwrap().revision,
            stamp.revision + 1,
            "generic indexed_at-only updates retain their public contract"
        );
    }

    #[test]
    fn scanner_probe_tracks_post_write_trigger_identity_and_retry_after_rollback() {
        let fixture = Fixture::new("scanner-probe-trigger-retry");
        let db = Database::open(fixture.database()).unwrap();
        let mut row = record("/root/Σ.txt", 7);
        db.upsert_file(&row).unwrap();
        db.conn
            .execute_batch(
                "CREATE TRIGGER moved_after_update AFTER UPDATE ON files WHEN NEW.size=123
            BEGIN UPDATE files SET id=777 WHERE id=NEW.id; END;
            CREATE TRIGGER rejected_update BEFORE UPDATE ON files WHEN NEW.size=999
            BEGIN SELECT RAISE(ABORT,'owned failure'); END;",
            )
            .unwrap();
        db.begin().unwrap();
        let mut writer = db.prepare_scanner_upsert(4).unwrap();
        row.size = Some(123);
        row.indexed_at = 8;
        assert!(writer.upsert(&row).unwrap());
        let before = db.content_stamp().unwrap();
        row.indexed_at = 9;
        assert!(!writer.upsert(&row).unwrap());
        assert_eq!(db.content_stamp().unwrap(), before);
        let seen = writer.into_seen_ids().unwrap();
        assert!(seen.contains(777));
        db.commit().unwrap();
        let saved = db.all_records().unwrap();
        let stamp = db.content_stamp().unwrap();
        db.begin().unwrap();
        let mut writer = db.prepare_scanner_upsert(4).unwrap();
        row.size = Some(999);
        assert!(writer.upsert(&row).is_err());
        db.rollback().unwrap();
        assert_eq!(db.all_records().unwrap(), saved);
        assert_eq!(db.content_stamp().unwrap(), stamp);
        db.begin().unwrap();
        row.size = Some(124);
        assert!(writer.upsert(&row).unwrap());
        drop(writer);
        db.commit().unwrap();
        assert_eq!(db.all_records().unwrap()[0].id, 777);
    }

    #[test]
    fn scanner_upsert_compares_all_material_fields_and_null_empty_distinctions() {
        let fixture = Fixture::new("scanner-material-columns");
        let db = Database::open(fixture.database()).unwrap();
        let base = record("/root/Base.RS", 7);
        let mut cases = Vec::new();
        macro_rules! changed {
            ($field:ident, $value:expr) => {{
                let mut value = base.clone();
                value.$field = $value;
                value.indexed_at = 8;
                cases.push((stringify!($field), value));
            }};
        }
        changed!(path_lower, String::new());
        changed!(basename, "independent/长名".into());
        changed!(basename_lower, String::new());
        changed!(ext_lower, None);
        changed!(ext_lower, Some(String::new()));
        changed!(kind, FileKind::Directory);
        for value in [i64::MIN, 0, i64::MAX] {
            changed!(size, Some(value));
            changed!(mtime, Some(value));
            changed!(ctime, Some(value));
            changed!(dev, Some(value));
            changed!(inode, Some(value));
        }
        db.begin().unwrap();
        db.upsert_file(&base).unwrap();
        let id = assert_upserted_columns(&db, &base);
        let mut writer = db.prepare_scanner_upsert(1).unwrap();
        for (field, value) in cases {
            db.upsert_file(&base).unwrap();
            let before = db.content_stamp().unwrap();
            assert!(writer.upsert(&value).unwrap(), "{field}");
            assert_eq!(assert_upserted_columns(&db, &value), id, "{field}");
            assert_eq!(
                db.content_stamp().unwrap().revision,
                before.revision + 1,
                "{field}"
            );
            let total = scalar(&db, "SELECT total_changes();");
            let mut newer_visit = value;
            newer_visit.indexed_at = 9;
            assert!(!writer.upsert(&newer_visit).unwrap(), "{field}");
            assert_eq!(scalar(&db, "SELECT total_changes();"), total, "{field}");
        }
        let mut other_path = base;
        other_path.path = "/root/base.rs".into();
        assert!(writer.upsert(&other_path).unwrap());
        assert_ne!(assert_upserted_columns(&db, &other_path), id);
        drop(writer);
        db.commit().unwrap();
    }

    #[test]
    fn exact_scanner_cleanup_batches_binary_paths_and_preserves_seen_signed_ids() {
        let fixture = Fixture::new("scanner-exact-cleanup");
        let db = Database::open(fixture.database()).unwrap();
        for path in [
            "/root",
            "/root-old/neighbor",
            "/root.ignored",
            "/elsewhere/keep",
            "/root/目录/seen",
        ] {
            db.upsert_file(&record(path, i64::MAX)).unwrap();
        }
        for n in 0..610 {
            db.upsert_file(&record(&format!("/root/item-{n:04}"), i64::MAX))
                .unwrap();
        }
        db.conn
            .execute_batch(
                "UPDATE files SET id=-9223372036854775808 WHERE path='/root';
             UPDATE files SET id=9223372036854775807 WHERE path='/root/目录/seen';",
            )
            .unwrap();
        let before = db.all_records().unwrap();
        let mut seen = SeenIds::new(8);
        let kept = |row: &FileRecord| {
            row.path == "/root"
                || row.path.ends_with("/seen")
                || row
                    .path
                    .rsplit('-')
                    .next()
                    .and_then(|n| n.parse::<usize>().ok())
                    .is_some_and(|n| n % 3 == 0)
        };
        for row in &before {
            if kept(row) {
                assert!(seen.insert(row.id));
            }
        }
        let expected = before
            .iter()
            .filter(|row| !row.path.starts_with("/root/") || kept(row))
            .cloned()
            .collect::<Vec<_>>();
        db.begin().unwrap();
        let removed = db
            .delete_unseen_under_roots(&[PathBuf::from("/root/")], &seen)
            .unwrap();
        db.commit().unwrap();
        assert_eq!(removed as usize, before.len() - expected.len());
        assert_eq!(db.all_records().unwrap(), expected);
        // The filesystem root includes every absolute namespace and still
        // respects exact membership rather than a timestamp sentinel.
        db.begin().unwrap();
        db.delete_unseen_under_roots(&[PathBuf::from("/")], &seen)
            .unwrap();
        db.commit().unwrap();
        assert!(db
            .all_records()
            .unwrap()
            .iter()
            .all(|row| seen.contains(row.id)));
    }

    #[test]
    fn compacted_copy_preserves_all_records_metadata_ids_and_future_journal_writes() {
        let fixture = Fixture::new("compacted-copy-complete");
        let db = Database::open(fixture.database()).unwrap();
        db.begin().unwrap();
        for i in 0..256 {
            let mut value = FileRecord::new(
                format!("/copy/目录/Code-{i:04}.RS"),
                match i % 5 {
                    0 => FileKind::File,
                    1 => FileKind::Directory,
                    2 => FileKind::Symlink,
                    3 => FileKind::App,
                    _ => FileKind::Other,
                },
                i64::MIN + i,
            );
            if i % 5 == 0 {
                value.path_lower = format!("/independent/{i}/ος");
            }
            if i % 7 == 0 {
                value.path_lower.clear();
            }
            value.basename = format!("independent original {i}/长名");
            value.basename_lower = format!("independent lowered {i}/σ");
            value.ext_lower = match i % 3 {
                0 => None,
                1 => Some(String::new()),
                _ => Some("RS".into()),
            };
            value.size = match i % 3 {
                0 => None,
                1 => Some(i64::MIN),
                _ => Some(i64::MAX),
            };
            value.mtime = match i % 4 {
                0 => None,
                1 => Some(-1),
                2 => Some(0),
                _ => Some(i64::MAX),
            };
            value.ctime = Some(i64::MAX - i);
            value.dev = (i % 2 == 0).then_some(i64::MIN + i);
            value.inode = (i % 2 != 0).then_some(i64::MAX - i);
            db.upsert_file(&value).unwrap();
        }
        db.conn.execute_batch("UPDATE files SET id=-9223372036854775808 WHERE id=1; UPDATE files SET id=9223372036854775807 WHERE id=2; UPDATE files SET id=0,basename=basename||char(0),basename_lower=basename_lower||char(0) WHERE id=3;").unwrap();
        db.set_roots(&["/copy/目录".into(), "/copy/Other".into()])
            .unwrap();
        db.set_excludes(&[".git".into(), "cache/目录".into()])
            .unwrap();
        db.set_meta("watch_last_event_id", "9123456789").unwrap();
        db.set_meta("last_indexed_at", "1987654321").unwrap();
        db.commit().unwrap();
        let expected = db.all_records().unwrap();
        let source_file = CompactFilePin::at(db.path()).unwrap();
        let source_bytes = std::fs::read(db.path()).unwrap();
        let wal = PathBuf::from(format!("{}-wal", db.path().display()));
        let wal_before = std::fs::read(&wal).ok();
        let control = db.compact_control_pin().unwrap();
        let temp_store = scalar(&db, "PRAGMA temp_store;");
        let output = fixture.0.join("file:copy'?cache=shared.sqlite");
        assert_eq!(db.write_compacted_copy(&output).unwrap(), expected.len());
        assert!(!std::fs::read_dir(&fixture.0).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".macevery-compact-")));
        assert_eq!(CompactFilePin::at(db.path()).unwrap(), source_file);
        assert_eq!(std::fs::read(db.path()).unwrap(), source_bytes);
        assert_eq!(std::fs::read(&wal).ok(), wal_before);
        assert_eq!(scalar(&db, "PRAGMA temp_store;"), temp_store);
        assert!(db.conn.is_autocommit());
        assert_eq!(std::fs::metadata(&output).unwrap().mode() & 0o777, 0o600);
        let copied = Database::open_read_only(output.clone()).unwrap();
        assert_eq!(copied.all_records().unwrap(), expected);
        assert_eq!(copied.compact_control_pin().unwrap(), control);
        assert_ne!(copied.identity, db.identity);
        assert_eq!(
            source_file.identity,
            db.content_stamp().unwrap().file_identity
        );
        drop(copied);
        let copied = Database::open_existing(output).unwrap();
        let mut changed = expected[5].clone();
        changed.size = Some(777);
        for database in [&db, &copied] {
            let base = database.content_stamp().unwrap();
            database.upsert_file(&changed).unwrap();
            database.begin_read().unwrap();
            let delta = database.file_changes_since(&base, 4096, 4096).unwrap();
            database.commit().unwrap();
            let FileChangeRead::Available { target, changes } = delta else {
                panic!("copy must retain functional change triggers and history");
            };
            assert_eq!(target.revision, base.revision + 1);
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].id, changed.id);
            assert_eq!(changes[0].record.as_ref(), Some(&changed));
        }
        assert_eq!(db.all_records().unwrap(), copied.all_records().unwrap());
    }

    #[test]
    fn compacted_copy_rejects_existing_objects_source_aliases_and_missing_parents() {
        let fixture = Fixture::new("compacted-copy-targets");
        let db = Database::open(fixture.database()).unwrap();
        let before = CompactFilePin::at(db.path()).unwrap();
        let bytes = std::fs::read(db.path()).unwrap();
        let sentinel = fixture.0.join("existing.sqlite");
        std::fs::write(&sentinel, b"do not replace").unwrap();
        assert!(db.write_compacted_copy(&sentinel).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"do not replace");
        let empty = fixture.0.join("empty.sqlite");
        std::fs::write(&empty, b"").unwrap();
        assert!(db.write_compacted_copy(&empty).is_err());
        assert_eq!(std::fs::metadata(empty).unwrap().len(), 0);
        let dangling = fixture.0.join("dangling.sqlite");
        std::os::unix::fs::symlink(fixture.0.join("absent.sqlite"), &dangling).unwrap();
        assert!(db.write_compacted_copy(&dangling).is_err());
        assert!(std::fs::symlink_metadata(&dangling)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(db.write_compacted_copy(&fixture.0).is_err());
        assert!(db.write_compacted_copy(db.path()).is_err());
        assert!(db.write_compacted_copy(Path::new("")).is_err());
        let missing = fixture.0.join("absent-parent/copy.sqlite");
        assert!(db.write_compacted_copy(&missing).is_err());
        assert!(!missing.parent().unwrap().exists());
        let alias = fixture.0.join("source-alias.sqlite");
        std::os::unix::fs::symlink(db.path(), &alias).unwrap();
        for source_name in [db.path(), alias.as_path()] {
            for suffix in ["-journal", "-JOURNAL", "-wal", "-shm"] {
                let output = PathBuf::from(format!("{}{suffix}", source_name.display()));
                let existed = std::fs::symlink_metadata(&output).is_ok();
                assert!(db.write_compacted_copy(&output).is_err());
                assert_eq!(std::fs::symlink_metadata(&output).is_ok(), existed);
            }
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let output = fixture.0.join(format!("sidecar{suffix}.sqlite"));
            let sidecar = PathBuf::from(format!("{}{suffix}", output.display()));
            std::os::unix::fs::symlink(fixture.0.join("missing-sidecar-target"), &sidecar).unwrap();
            assert!(db.write_compacted_copy(&output).is_err());
            assert!(!output.exists());
            assert!(std::fs::symlink_metadata(sidecar)
                .unwrap()
                .file_type()
                .is_symlink());
        }
        assert_eq!(CompactFilePin::at(db.path()).unwrap(), before);
        assert_eq!(std::fs::read(db.path()).unwrap(), bytes);
    }

    #[test]
    fn compacted_copy_rejects_explicit_transactions_and_implicit_active_cursors() {
        let fixture = Fixture::new("compacted-copy-transactions");
        let db = Database::open(fixture.database()).unwrap();
        db.upsert_file(&record("/copy/code", 1)).unwrap();
        let output = fixture.0.join("copy.sqlite");
        db.begin().unwrap();
        db.set_meta("caller-uncommitted", "must survive until caller rollback")
            .unwrap();
        assert!(db.write_compacted_copy(&output).is_err());
        assert!(!output.exists());
        assert!(!db.conn.is_autocommit());
        assert_eq!(
            db.get_meta("caller-uncommitted").unwrap().as_deref(),
            Some("must survive until caller rollback")
        );
        db.rollback().unwrap();
        db.begin_read().unwrap();
        assert!(db.write_compacted_copy(&output).is_err());
        assert!(!output.exists());
        db.rollback().unwrap();
        let mut visited = false;
        db.for_each_record_ref(|_| {
            assert!(db.conn.is_autocommit());
            assert!(db.write_compacted_copy(&output).is_err());
            assert!(!output.exists());
            visited = true;
            Ok(())
        })
        .unwrap();
        assert!(visited);
        let readonly = Database::open_read_only(fixture.database()).unwrap();
        assert_eq!(readonly.write_compacted_copy(&output).unwrap(), 1);
    }

    #[test]
    fn compacted_copy_retains_failed_output_and_stream_verifier_checks_every_field() {
        let fixture = Fixture::new("compacted-copy-failures");
        let db = Database::open(fixture.database()).unwrap();
        db.upsert_file(&record("/private-fixture/do-not-include-in-errors/code", 1))
            .unwrap();
        let output = fixture.0.join("copy.sqlite");
        assert_eq!(db.write_compacted_copy(&output).unwrap(), 1);
        let copied = Database::open_existing(output.clone()).unwrap();
        for change in [
            "id=-9",
            "path='/different'",
            "path_lower='/independent-lower'",
            "basename='independent original'",
            "basename_lower='independent lower'",
            "ext_lower='changed'",
            "kind='directory'",
            "size=99",
            "mtime=99",
            "ctime=99",
            "dev=99",
            "inode=99",
            "indexed_at=99",
        ] {
            copied.begin().unwrap();
            copied
                .conn
                .execute_batch(&format!("UPDATE files SET {change};"))
                .unwrap();
            let error = db
                .verify_compacted_records(&copied)
                .unwrap_err()
                .to_string();
            assert!(error.contains("ordinal 1"), "{error}");
            assert!(!error.contains("do-not-include-in-errors"));
            copied.rollback().unwrap();
            assert!(output.exists());
        }
        copied.begin().unwrap();
        copied.conn.execute_batch("DELETE FROM files;").unwrap();
        assert!(db
            .verify_compacted_records(&copied)
            .unwrap_err()
            .to_string()
            .contains("counts differ"));
        copied.rollback().unwrap();
        assert_eq!(db.verify_compacted_records(&copied).unwrap(), 1);
        drop(copied);
        // Missing control metadata fails after creating private staging. Keep
        // its recovery pointer and never publish an unverified output.
        db.conn.execute_batch("DROP TABLE meta;").unwrap();
        let failed = fixture.0.join("failed.sqlite");
        let error = db.write_compacted_copy(&failed).unwrap_err().to_string();
        assert!(!failed.exists());
        let directories = std::fs::read_dir(&fixture.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".macevery-compact-")
            })
            .collect::<Vec<_>>();
        assert_eq!(directories.len(), 1);
        let retained = &directories[0];
        assert!(error.contains(retained.to_str().unwrap()));
        assert_eq!(std::fs::metadata(retained).unwrap().mode() & 0o777, 0o700);
        std::fs::write(retained.join("failure-evidence"), b"preserve").unwrap();
        assert!(db.write_compacted_copy(&failed).is_err());
        assert!(!failed.exists());
        assert_eq!(
            std::fs::read(retained.join("failure-evidence")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn exact_search_matches_packed_with_raw_sqlite_unicode_null_blob_and_signed_ties() {
        let fixture = Fixture::new("exact-search-fields");
        let db = Database::open(fixture.database()).unwrap();
        db.conn.execute_batch(
            "INSERT INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES
             (-9223372036854775808,CAST(x'ff' AS TEXT),NULL,'Blob original',CAST('code' AS BLOB),NULL,'linK',-9223372036854775808,NULL,9223372036854775807,-9223372036854775808,9223372036854775807,-9223372036854775808),
             (9223372036854775807,CAST(x'fe' AS TEXT),'','Text original','code','','unknown',9223372036854775807,NULL,-9223372036854775808,9223372036854775807,-9223372036854775808,9223372036854775807),
             (-2,'/A/ΟΣ',NULL,'independent original','independent lower','PDF','FILE',1,-1,2,3,4,5),
             (0,'/EMPTY/Σ','','not a suffix','code-prefix','pdf','directory',NULL,0,NULL,NULL,NULL,0),
             (1,'/nul/'||char(0)||'code',NULL,'nul'||char(0),'',NULL,'file',10,100,20,30,40,50),
             (2,'/independent/original','/elsewhere/汉','X','fuzzy-axbyczd',NULL,'app',NULL,9223372036854775807,NULL,NULL,NULL,1),
             (3,'/invalid/lower',CAST(x'ff' AS TEXT),'display',CAST(x'fe' AS BLOB),NULL,'other',NULL,0,NULL,NULL,NULL,1),
             (4,'/汉','/汉','汉','汉',NULL,'file',NULL,NULL,NULL,NULL,NULL,1),
             (5,'/abcd','/abcd','abcd','abcd',NULL,'file',NULL,1,NULL,NULL,NULL,1),
             (6,'/raw-bad/original','/code/fallback','display',CAST(x'636f6465ff' AS BLOB),NULL,'other',NULL,1,NULL,NULL,NULL,1),
             (7,'/raw-path/blob',CAST('/code/blob' AS BLOB),'independent','',NULL,'file',NULL,1,NULL,NULL,NULL,1);"
        ).unwrap();
        let records = db.all_records().unwrap();
        let packed = crate::compact_index::CompactIndex::load(&db).unwrap();
        let mut cases = crate::search::test_reference::options();
        for query in [
            "code",
            "CODE",
            "ΟΣ",
            "Σ",
            "汉",
            "~abcd",
            "\0",
            "code\0",
            "*?*",
            "invalid",
            "code fallback",
            "*a*longer*",
            "kind:symlink code",
            "!kind:file",
            "!ext:pdf",
            "path:elsewhere",
            "!path:汉",
        ] {
            cases.push(SearchOptions {
                query: query.into(),
                ..Default::default()
            });
        }
        cases.push(SearchOptions {
            query: "????".into(),
            path_only: true,
            ..Default::default()
        });
        for mut options in cases {
            for limit in [0, 1, 2, 7, 300, usize::MAX] {
                options.limit = limit;
                let expected = crate::search::test_reference::search_records(&records, &options);
                let actual = crate::search::search(&db, &options).unwrap();
                assert_eq!(actual, expected, "SQLite {options:?}");
                assert_eq!(actual, packed.search(&options), "packed {options:?}");
            }
        }
        let first = crate::search::search(
            &db,
            &SearchOptions {
                query: "code".into(),
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first[0].record.id, i64::MIN);
        assert!(db.conn.is_autocommit());
    }

    #[test]
    fn utf16_blob_text_conversion_bypasses_every_raw_byte_prepass() {
        let fixture = Fixture::new("exact-utf16-blob");
        let setup = Connection::open(&fixture.database()).unwrap();
        setup
            .execute_batch("PRAGMA encoding='UTF-16le'; CREATE TABLE encoding_probe(value TEXT);")
            .unwrap();
        drop(setup);
        let db = Database::open(fixture.database()).unwrap();
        assert!(!db.uses_utf8_encoding().unwrap());
        db.conn.execute_batch(
            "INSERT INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,indexed_at) VALUES
             (1,'/unicode-match','/different','physical',x'00d84100',x'720073','file',1),
             (2,'/B','/elsewhere','physical',x'63006f0064006500','r','file',1),
             (3,'/longer/path','/elsewhere','physical','code-prefix','r','file',1);"
        ).unwrap();
        let records = db.all_records().unwrap();
        assert_eq!(records[0].basename_lower, "\u{10041}");
        assert_eq!(records[0].ext_lower.as_deref(), Some("r"));
        assert_eq!(records[1].basename_lower, "code");
        let packed = crate::compact_index::CompactIndex::load(&db).unwrap();
        for query in [
            "code",
            "\u{10041}",
            "*\u{10041}*",
            "ext:r",
            "ext:r code",
            "r",
            "~r",
        ] {
            for limit in [0, 1, 2, 300] {
                let options = SearchOptions {
                    query: query.into(),
                    limit,
                    ..Default::default()
                };
                let got = crate::search::search(&db, &options).unwrap();
                assert_eq!(
                    got,
                    crate::search::test_reference::search_records(&records, &options),
                    "{options:?}"
                );
                assert_eq!(got, packed.search(&options), "{options:?}");
            }
        }
    }

    #[test]
    fn extension_index_stream_preserves_text_blob_numeric_and_null_semantics() {
        let fixture = Fixture::new("exact-extension-types");
        let setup = Connection::open(&fixture.database()).unwrap();
        // No affinity on this legacy ext_lower column keeps actual INTEGER and
        // REAL values available to exercise column_str's text conversion.
        setup.execute_batch(
            "CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE,path_lower TEXT,
             basename TEXT NOT NULL,basename_lower TEXT NOT NULL,ext_lower,kind TEXT NOT NULL,
             size INTEGER,mtime INTEGER,ctime INTEGER,dev INTEGER,inode INTEGER,indexed_at INTEGER NOT NULL);"
        ).unwrap();
        drop(setup);
        let db = Database::open(fixture.database()).unwrap();
        assert!(db.uses_utf8_encoding().unwrap());
        db.conn.execute_batch(
            "INSERT INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,indexed_at) VALUES
             (-9,'/text/code',NULL,'code','code','rs','file',1),
             (2,'/blob/code',NULL,'code','code',CAST('rs' AS BLOB),'file',1),
             (3,'/swift/c-o-d-e',NULL,'c-o-d-e','c-o-d-e','swift','file',1),
             (4,'/integer/c-o-d-e',NULL,'c-o-d-e','c-o-d-e',7,'file',1),
             (5,'/real/code',NULL,'code','code',1.5,'file',1),
             (6,'/null/code',NULL,'code','code',NULL,'file',1),
             (7,'/empty/code',NULL,'code','code','','file',1),
             (8,'/empty-blob/code',NULL,'code','code',x'','file',1),
             (9,'/nul/code',NULL,'code','code','r'||char(0)||'s','file',1),
             (10,'/uppercase/code',NULL,'code','code','RS','file',1),
             (11,'/unicode/code',NULL,'code','code',CAST('汉' AS BLOB),'file',1);"
        ).unwrap();
        let records = db.all_records().unwrap();
        let filters = ["rs", "swift", "7", "1.5", "", "汉"]
            .map(str::to_owned)
            .to_vec();
        let mut ids = Vec::new();
        assert!(db
            .for_each_extension_ref(&filters, |row| {
                ids.push(row.id);
                Ok(())
            })
            .unwrap());
        ids.sort();
        let expected = records
            .iter()
            .filter(|row| {
                row.ext_lower
                    .as_ref()
                    .is_some_and(|ext| filters.contains(ext))
            })
            .map(|row| row.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, expected);
        let packed = crate::compact_index::CompactIndex::load(&db).unwrap();
        for ext_filters in [
            filters,
            vec!["rs".into(), "swift".into()],
            vec!["r\0s".into()],
            vec!["rs".into(); 129],
        ] {
            for query in [
                "code",
                "~code",
                "*?code*",
                "",
                "kind:file ~cde",
                "code text",
            ] {
                let options = SearchOptions {
                    query: query.into(),
                    ext_filters: ext_filters.clone(),
                    limit: 300,
                    ..Default::default()
                };
                let got = crate::search::search(&db, &options).unwrap();
                assert_eq!(
                    got,
                    crate::search::test_reference::search_records(&records, &options),
                    "{options:?}"
                );
                assert_eq!(got, packed.search(&options), "{options:?}");
            }
        }
        for unsuitable in [vec![], vec!["r\0s".into()], vec!["rs".into(); 129]] {
            assert!(!db
                .for_each_extension_ref(&unsuitable, |_| panic!(
                    "unsuitable list must not partially visit"
                ))
                .unwrap());
        }
        for predicate in [
            "typeof(ext_lower)='text' AND ext_lower IN (?1)",
            "typeof(ext_lower)='blob' AND ext_lower IN (CAST(?1 AS BLOB))",
            "ext_lower < '' COLLATE BINARY AND typeof(ext_lower) IN ('integer','real') AND CAST(ext_lower AS TEXT) IN (?1)",
        ] {
            let mut plan = db.conn.prepare(&format!("EXPLAIN QUERY PLAN SELECT id,path FROM files INDEXED BY idx_files_ext_lower WHERE {predicate};")).unwrap();
            plan.bind_text(1, Some("rs")).unwrap();
            let mut details = Vec::new();
            while let Step::Row = plan.step().unwrap() {
                details.push(plan.column_text(3).unwrap());
            }
            assert!(
                details
                    .iter()
                    .any(|text| text.contains("SEARCH files USING INDEX idx_files_ext_lower")),
                "{details:?}"
            );
            assert!(
                !details.iter().any(|text| text.contains("TEMP B-TREE")),
                "{details:?}"
            );
        }
    }

    #[test]
    fn indexed_prefix_stream_has_no_limit_sort_or_duplicate_text_blob_rows() {
        let fixture = Fixture::new("exact-prefix-range");
        let db = Database::open(fixture.database()).unwrap();
        db.conn
            .execute_batch(
                "INSERT INTO files(id,path,basename,basename_lower,kind,indexed_at) VALUES
             (100,'/text','physical','code-z','file',1),
             (-10,'/blob','physical',CAST('code-a' AS BLOB),'file',1),
             (3,'/before','physical','cod','file',1),
             (4,'/upper','physical','codf','file',1),
             (5,'/empty','physical','','file',1),
             (6,'/unicode','physical','code汉','file',1),
             (7,'/invalid','physical',CAST(x'636f6465ff' AS BLOB),'file',1),
             (8,'/nul-containing-name','physical','before'||char(0)||'code汉%_after','file',1);",
            )
            .unwrap();
        let mut actual = Vec::new();
        db.for_each_basename_prefix_ref("code", "codf", |row| {
            // The indexed range may contain invalid UTF-8; the Rust scorer is
            // authoritative and sees the same empty fallback as a full load.
            if row.basename_lower.starts_with("code") {
                actual.push(row.id);
            }
            Ok(())
        })
        .unwrap();
        actual.sort();
        assert_eq!(actual, [-10, 6, 100]);
        for literal in ["code", "汉", "%_"] {
            let mut actual = Vec::new();
            db.for_each_basename_literal_ref(literal, |row| {
                if row.basename_lower.contains(literal) {
                    actual.push(row.id);
                }
                Ok(())
            })
            .unwrap();
            actual.sort();
            let expected = db
                .all_records()
                .unwrap()
                .into_iter()
                .filter(|row| row.basename_lower.contains(literal))
                .map(|row| row.id)
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "literal={literal:?}");
        }
        for bounds in [
            "basename_lower >= ?1 COLLATE BINARY AND basename_lower < ?2 COLLATE BINARY",
            "basename_lower >= CAST(?1 AS BLOB) COLLATE BINARY AND basename_lower < CAST(?2 AS BLOB) COLLATE BINARY",
        ] {
            let mut stmt = db.conn.prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at FROM files WHERE {bounds};"
            )).unwrap();
            stmt.bind_text(1, Some("code")).unwrap();
            stmt.bind_text(2, Some("codf")).unwrap();
            let mut details = Vec::new();
            while let Step::Row = stmt.step().unwrap() {
                details.push(stmt.column_text(3).unwrap());
            }
            assert!(details.iter().any(|text| text.contains("SEARCH files USING INDEX idx_files_basename_lower")), "{details:?}");
            assert!(!details.iter().any(|text| text.contains("TEMP B-TREE")), "{details:?}");
        }
    }

    #[test]
    fn indexed_basename_literal_scan_uses_no_temp_sort() {
        let fixture = Fixture::new("exact-literal-plan");
        let db = Database::open(fixture.database()).unwrap();
        let mut stmt = db.conn.prepare(
            "EXPLAIN QUERY PLAN SELECT id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at
             FROM files INDEXED BY idx_files_basename_lower
             WHERE instr(CAST(basename_lower AS BLOB),CAST(?1 AS BLOB)) > 0;"
        ).unwrap();
        stmt.bind_text(1, Some("code")).unwrap();
        let mut details = Vec::new();
        while let Step::Row = stmt.step().unwrap() {
            details.push(stmt.column_text(3).unwrap());
        }
        assert!(
            details
                .iter()
                .any(|text| text.contains("SCAN files USING INDEX idx_files_basename_lower")),
            "{details:?}"
        );
        assert!(
            !details.iter().any(|text| text.contains("TEMP B-TREE")),
            "{details:?}"
        );
        db.conn.register_path_literal_filter().unwrap();
        let mut path_plan = db.conn.prepare("EXPLAIN QUERY PLAN SELECT id FROM files INDEXED BY idx_files_path_lower WHERE macevery_path_may_contain(path_lower,?1);").unwrap();
        path_plan.bind_text(1, Some("code")).unwrap();
        let mut details = Vec::new();
        while let Step::Row = path_plan.step().unwrap() {
            details.push(path_plan.column_text(3).unwrap());
        }
        assert!(
            details
                .iter()
                .any(|text| text.contains("COVERING INDEX idx_files_path_lower")),
            "{details:?}"
        );
    }

    #[test]
    fn search_read_snapshot_is_shared_with_callers_and_released_on_error() {
        let fixture = Fixture::new("exact-search-snapshot");
        let db = Database::open(fixture.database()).unwrap();
        let writer = Database::open(fixture.database()).unwrap();
        db.upsert_file(&record("/old/code", 1)).unwrap();
        db.with_read_snapshot(|| {
            let expected = db.all_records()?;
            writer.upsert_file(&record("/new/code", 2))?;
            let found = crate::search::search(
                &db,
                &SearchOptions {
                    query: "code".into(),
                    limit: 300,
                    ..Default::default()
                },
            )?;
            assert_eq!(
                found,
                crate::search::test_reference::search_records(
                    &expected,
                    &SearchOptions {
                        query: "code".into(),
                        limit: 300,
                        ..Default::default()
                    }
                )
            );
            assert!(!db.conn.is_autocommit());
            Ok(())
        })
        .unwrap();
        assert!(db.conn.is_autocommit());
        assert_eq!(db.all_records().unwrap().len(), 2);

        let error = || Err::<(), _>(MacEveryError::Sqlite("injected read failure".into()));
        assert!(db.with_read_snapshot(error).is_err());
        assert!(db.conn.is_autocommit());
        db.begin().unwrap();
        db.set_meta("caller-uncommitted", "keep until caller rollback")
            .unwrap();
        assert!(db.with_read_snapshot(error).is_err());
        assert!(!db.conn.is_autocommit());
        assert_eq!(
            db.get_meta("caller-uncommitted").unwrap().as_deref(),
            Some("keep until caller rollback")
        );
        db.rollback().unwrap();
        assert!(db.get_meta("caller-uncommitted").unwrap().is_none());
    }

    fn scalar(db: &Database, sql: &str) -> i64 {
        let mut stmt = db.conn.prepare(sql).unwrap();
        assert!(matches!(stmt.step().unwrap(), Step::Row));
        stmt.column_i64(0).unwrap()
    }

    fn record(path: &str, generation: i64) -> FileRecord {
        FileRecord::new(path.to_string(), FileKind::File, generation)
    }

    fn assert_upserted_columns(db: &Database, expected: &FileRecord) -> i64 {
        // Inspect stored columns directly: all_records intentionally falls back
        // from an empty path_lower, which would hide a binding regression here.
        let mut stmt = db.conn.prepare("SELECT id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at FROM files WHERE path=?1;").unwrap();
        stmt.bind_text(1, Some(&expected.path)).unwrap();
        assert!(matches!(stmt.step().unwrap(), Step::Row));
        let id = stmt.column_i64(0).unwrap();
        assert_eq!(stmt.column_text(1).as_deref(), Some(expected.path.as_str()));
        assert_eq!(
            stmt.column_text(2).as_deref(),
            Some(expected.path_lower.as_str())
        );
        assert_eq!(
            stmt.column_text(3).as_deref(),
            Some(expected.basename.as_str())
        );
        assert_eq!(
            stmt.column_text(4).as_deref(),
            Some(expected.basename_lower.as_str())
        );
        assert_eq!(
            stmt.column_text(5).as_deref(),
            expected.ext_lower.as_deref()
        );
        assert_eq!(stmt.column_text(6).as_deref(), Some(expected.kind.as_str()));
        assert_eq!(stmt.column_i64(7), expected.size);
        assert_eq!(stmt.column_i64(8), expected.mtime);
        assert_eq!(stmt.column_i64(9), expected.ctime);
        assert_eq!(stmt.column_i64(10), expected.dev);
        assert_eq!(stmt.column_i64(11), expected.inode);
        assert_eq!(stmt.column_i64(12), Some(expected.indexed_at));
        assert!(matches!(stmt.step().unwrap(), Step::Done));
        id
    }

    #[test]
    fn prepared_upsert_identical_input_has_zero_dml_including_trigger_scratch() {
        let fixture = Fixture::new("prepared-upsert-noop");
        let db = Database::open(fixture.database()).unwrap();
        let mut value = record("/root/ΟΣ/İ-Code.'rs", i64::MIN);
        value.basename = "independent original/长字段".repeat(128);
        value.basename_lower = "independent lower/ος/σ/i\u{307}".into();
        value.ext_lower = None;
        value.size = Some(i64::MAX);
        value.mtime = Some(i64::MIN);
        value.ctime = Some(-1);
        value.dev = Some(0);
        value.inode = Some(i64::MAX);
        db.begin().unwrap();
        let mut upsert = db.prepare_file_upsert().unwrap();
        assert!(upsert.upsert(&value).unwrap());
        let id = assert_upserted_columns(&db, &value);
        let before = db.content_stamp().unwrap();
        let journal = scalar(&db, "SELECT COUNT(*) FROM macevery_file_changes;");
        let scratch = scalar(
            &db,
            "SELECT journal_conflict_id IS NULL FROM macevery_content_state;",
        );
        for ignored_id in [0, id, i64::MIN, i64::MAX] {
            value.id = ignored_id;
            let total = scalar(&db, "SELECT total_changes();");
            assert!(!upsert.upsert(&value).unwrap());
            assert_eq!(db.conn.changes(), 0);
            assert_eq!(scalar(&db, "SELECT total_changes();"), total);
            assert_eq!(db.content_stamp().unwrap(), before);
            assert_eq!(
                scalar(&db, "SELECT COUNT(*) FROM macevery_file_changes;"),
                journal
            );
            assert_eq!(
                scalar(
                    &db,
                    "SELECT journal_conflict_id IS NULL FROM macevery_content_state;"
                ),
                scratch
            );
            assert_eq!(assert_upserted_columns(&db, &value), id);
        }
        drop(upsert);
        db.commit().unwrap();
        // The compatibility entry point shares the same zero-DML semantics.
        let total = scalar(&db, "SELECT total_changes();");
        db.upsert_file(&value).unwrap();
        assert_eq!(scalar(&db, "SELECT total_changes();"), total);
        assert_eq!(db.content_stamp().unwrap(), before);
    }

    #[test]
    fn prepared_upsert_compares_every_persisted_value_and_null_distinction() {
        let fixture = Fixture::new("prepared-upsert-columns");
        let db = Database::open(fixture.database()).unwrap();
        let base = record("/root/Base.RS", 7);
        let mut cases = Vec::new();
        macro_rules! changed {
            ($field:ident, $value:expr) => {{
                let mut value = base.clone();
                value.$field = $value;
                cases.push((stringify!($field), value));
            }};
        }
        changed!(path_lower, String::new());
        changed!(path_lower, "/independent/ος/i\u{307}/\u{10041}".into());
        changed!(basename, String::new());
        changed!(basename, "non-suffix/Σ/Name".repeat(128));
        changed!(basename_lower, String::new());
        changed!(basename_lower, "non-suffix/σ/lower".into());
        changed!(ext_lower, None);
        changed!(ext_lower, Some(String::new()));
        changed!(ext_lower, Some("Σ/İ/\u{10041}".into()));
        for kind in [
            FileKind::Directory,
            FileKind::Symlink,
            FileKind::App,
            FileKind::Other,
        ] {
            changed!(kind, kind);
        }
        for value in [i64::MIN, -1, 0, i64::MAX] {
            changed!(size, Some(value));
            changed!(mtime, Some(value));
            changed!(ctime, Some(value));
            changed!(dev, Some(value));
            changed!(inode, Some(value));
            changed!(indexed_at, value);
        }
        db.begin().unwrap();
        let mut upsert = db.prepare_file_upsert().unwrap();
        assert!(upsert.upsert(&base).unwrap());
        let original_id = assert_upserted_columns(&db, &base);
        for (field, value) in cases {
            upsert.upsert(&base).unwrap();
            let before = db.content_stamp().unwrap();
            assert!(upsert.upsert(&value).unwrap(), "{field}");
            assert_eq!(db.conn.changes(), 1, "{field}");
            assert_eq!(assert_upserted_columns(&db, &value), original_id, "{field}");
            assert_eq!(db.content_stamp().unwrap().revision, before.revision + 1);
            let total = scalar(&db, "SELECT total_changes();");
            assert!(!upsert.upsert(&value).unwrap(), "{field}");
            assert_eq!(scalar(&db, "SELECT total_changes();"), total, "{field}");
        }
        // A distinct path must insert even if every other persisted value equals
        // an existing row; comparing only payload columns would lose this row.
        let mut other_path = base.clone();
        other_path.path = "/root/base.rs".into();
        assert!(upsert.upsert(&other_path).unwrap());
        assert_ne!(assert_upserted_columns(&db, &other_path), original_id);
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM files;"), 2);
        drop(upsert);
        db.commit().unwrap();
    }

    #[test]
    fn prepared_upsert_alternating_bindings_clear_null_and_empty_values() {
        let fixture = Fixture::new("prepared-upsert-bindings");
        let db = Database::open(fixture.database()).unwrap();
        let mut filled = record("/root/Filled.RS", i64::MAX);
        filled.ext_lower = Some(String::new());
        filled.size = Some(i64::MIN);
        filled.mtime = Some(i64::MAX);
        filled.ctime = Some(-1);
        filled.dev = Some(0);
        filled.inode = Some(i64::MAX);
        let mut empty = record("", i64::MIN);
        empty.path_lower.clear();
        empty.basename.clear();
        empty.basename_lower.clear();
        empty.ext_lower = None;
        let mut upsert = db.prepare_file_upsert().unwrap();
        db.begin().unwrap();
        let mut ids = [None, None];
        for round in 0..16 {
            for (index, value) in [&filled, &empty].into_iter().enumerate() {
                let total = scalar(&db, "SELECT total_changes();");
                assert_eq!(upsert.upsert(value).unwrap(), round == 0);
                let id = assert_upserted_columns(&db, value);
                if let Some(previous) = ids[index] {
                    assert_eq!(id, previous);
                    assert_eq!(scalar(&db, "SELECT total_changes();"), total);
                }
                ids[index] = Some(id);
            }
        }
        // Repairing a legacy NULL lower path remains an observable write;
        // NULL and the explicit lowered string must not compare as identical.
        db.conn
            .execute_batch("UPDATE files SET path_lower=NULL WHERE path='/root/Filled.RS';")
            .unwrap();
        let before = db.content_stamp().unwrap();
        assert!(upsert.upsert(&filled).unwrap());
        assert_eq!(db.content_stamp().unwrap().revision, before.revision + 1);
        assert_upserted_columns(&db, &filled);
        drop(upsert);
        db.commit().unwrap();
    }

    #[test]
    fn prepared_upsert_rollback_preserves_rows_journal_and_writer_reuse() {
        let fixture = Fixture::new("prepared-upsert-rollback");
        let db = Database::open(fixture.database()).unwrap();
        let initial = record("/root/persisted", 10);
        db.upsert_file(&initial).unwrap();
        let rows = db.all_records().unwrap();
        let before = db.content_stamp().unwrap();
        let journal = scalar(&db, "SELECT COUNT(*) FROM macevery_file_changes;");
        let mut upsert = db.prepare_file_upsert().unwrap();
        db.begin().unwrap();
        let mut changed = initial.clone();
        changed.indexed_at = 11;
        assert!(upsert.upsert(&changed).unwrap());
        assert!(upsert.upsert(&record("/root/rolled-back", 11)).unwrap());
        assert!(!db.conn.is_autocommit());
        db.rollback().unwrap();
        assert_eq!(db.all_records().unwrap(), rows);
        assert_eq!(db.content_stamp().unwrap(), before);
        assert_eq!(
            scalar(&db, "SELECT COUNT(*) FROM macevery_file_changes;"),
            journal
        );
        db.begin().unwrap();
        let total = scalar(&db, "SELECT total_changes();");
        assert!(!upsert.upsert(&initial).unwrap());
        assert_eq!(scalar(&db, "SELECT total_changes();"), total);
        changed.indexed_at = 12;
        assert!(upsert.upsert(&changed).unwrap());
        db.commit().unwrap();
        assert_eq!(db.content_stamp().unwrap().revision, before.revision + 1);
        db.begin_read().unwrap();
        let FileChangeRead::Available { target, changes } =
            db.file_changes_since(&before, 4096, 4096).unwrap()
        else {
            panic!("prepared upsert must preserve journal coverage");
        };
        db.commit().unwrap();
        assert_eq!(target.revision, before.revision + 1);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].id, rows[0].id);
        assert_eq!(changes[0].record.as_ref().unwrap().indexed_at, 12);
    }

    #[test]
    fn prepared_upsert_rechecks_external_replace_and_preserves_signed_ids() {
        let fixture = Fixture::new("prepared-upsert-external");
        let db = Database::open(fixture.database()).unwrap();
        let value = record("/root/replaced", 10);
        let mut upsert = db.prepare_file_upsert().unwrap();
        assert!(upsert.upsert(&value).unwrap());
        let external = Connection::open_existing(&fixture.database()).unwrap();
        for replacement_id in [i64::MIN, i64::MAX] {
            let old_id = assert_upserted_columns(&db, &value);
            let before = db.content_stamp().unwrap();
            external.execute_batch(&format!("PRAGMA recursive_triggers=OFF; INSERT OR REPLACE INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) SELECT {replacement_id},path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at FROM files WHERE path='/root/replaced';")).unwrap();
            let replaced = db.content_stamp().unwrap();
            assert!(replaced.revision > before.revision);
            let total = scalar(&db, "SELECT total_changes();");
            assert!(!upsert.upsert(&value).unwrap());
            assert_eq!(scalar(&db, "SELECT total_changes();"), total);
            assert_eq!(assert_upserted_columns(&db, &value), replacement_id);
            assert_eq!(db.content_stamp().unwrap(), replaced);
            db.begin_read().unwrap();
            let FileChangeRead::Available { changes, .. } =
                db.file_changes_since(&before, 4096, 4096).unwrap()
            else {
                panic!("external OR REPLACE must keep both old/new identities");
            };
            db.commit().unwrap();
            assert_eq!(changes.len(), 2);
            assert!(changes
                .iter()
                .any(|change| change.id == old_id && change.record.is_none()));
            assert!(changes.iter().any(|change| change.id == replacement_id
                && change
                    .record
                    .as_ref()
                    .is_some_and(|row| row.path == value.path)));
        }
        // A no-op deliberately does not touch the fixed conflict scratch cell.
        // The next real INSERT must overwrite it instead of leaking an old ID.
        let before = db.content_stamp().unwrap();
        assert!(upsert
            .upsert(&record("/root/fresh-after-replace", 11))
            .unwrap());
        db.begin_read().unwrap();
        let FileChangeRead::Available { changes, .. } =
            db.file_changes_since(&before, 4096, 4096).unwrap()
        else {
            panic!("fresh insert must keep journal coverage");
        };
        db.commit().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].record.as_ref().unwrap().path,
            "/root/fresh-after-replace"
        );
    }

    #[test]
    fn prepared_upsert_fresh_generation_retains_stale_cleanup_and_revision_semantics() {
        let fixture = Fixture::new("prepared-upsert-generation");
        let db = Database::open(fixture.database()).unwrap();
        db.upsert_file(&record("/root/kept", 10)).unwrap();
        db.upsert_file(&record("/root/stale", 10)).unwrap();
        let before = db.content_stamp().unwrap();
        let mut upsert = db.prepare_file_upsert().unwrap();
        db.begin().unwrap();
        assert!(upsert.upsert(&record("/root/kept", 20)).unwrap());
        assert_eq!(
            db.delete_stale_under_roots(&[PathBuf::from("/root")], 20)
                .unwrap(),
            1
        );
        db.commit().unwrap();
        assert_eq!(db.content_stamp().unwrap().revision, before.revision + 2);
        let rows = db.all_records().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/root/kept");
        assert_eq!(rows[0].indexed_at, 20);
        db.begin_read().unwrap();
        let FileChangeRead::Available { changes, .. } =
            db.file_changes_since(&before, 4096, 4096).unwrap()
        else {
            panic!("generation update and stale deletion must remain observable");
        };
        db.commit().unwrap();
        assert_eq!(changes.len(), 2);
        assert_eq!(changes.iter().filter(|row| row.record.is_none()).count(), 1);
        assert_eq!(
            changes
                .iter()
                .filter_map(|row| row.record.clone())
                .collect::<Vec<_>>(),
            rows
        );
    }

    #[test]
    fn prepared_upsert_binding_error_can_be_rolled_back_without_stale_parameters() {
        let fixture = Fixture::new("prepared-upsert-error");
        let db = Database::open(fixture.database()).unwrap();
        let value = record("/root/valid", 7);
        let mut upsert = db.prepare_file_upsert().unwrap();
        assert!(upsert.upsert(&value).unwrap());
        let before = db.content_stamp().unwrap();
        db.begin().unwrap();
        let total = scalar(&db, "SELECT total_changes();");
        let mut invalid = value.clone();
        invalid.ext_lower = Some("invalid\0extension".into());
        assert!(upsert.upsert(&invalid).is_err());
        assert_eq!(scalar(&db, "SELECT total_changes();"), total);
        assert_eq!(db.content_stamp().unwrap(), before);
        db.rollback().unwrap();
        assert!(!upsert.upsert(&value).unwrap());
        assert_upserted_columns(&db, &value);
    }

    #[test]
    fn prepared_upsert_step_error_preserves_original_error_and_first_retry_after_rollback() {
        let fixture = Fixture::new("prepared-upsert-step-error");
        let db = Database::open(fixture.database()).unwrap();
        let mut value = record("/root/valid", 7);
        let mut upsert = db.prepare_file_upsert().unwrap();
        assert!(upsert.upsert(&value).unwrap());
        let original_id = assert_upserted_columns(&db, &value);
        db.conn
            .execute_batch(
                "CREATE TEMP TRIGGER prepared_upsert_abort_insert
                 BEFORE INSERT ON files WHEN NEW.path='/root/blocked'
                 BEGIN SELECT RAISE(ABORT,'injected prepared INSERT failure'); END;
                 CREATE TEMP TRIGGER prepared_upsert_abort_update
                 BEFORE UPDATE ON files WHEN NEW.indexed_at=99
                 BEGIN SELECT RAISE(ABORT,'injected prepared UPDATE failure'); END;
                 CREATE UNIQUE INDEX prepared_upsert_unique_basename ON files(basename);",
            )
            .unwrap();
        let mut failed_update = value.clone();
        failed_update.indexed_at = 99;
        for (invalid, expected_error) in [
            (
                record("/root/blocked", 8),
                "injected prepared INSERT failure",
            ),
            (failed_update, "injected prepared UPDATE failure"),
            (record("/other/valid", 8), "UNIQUE constraint failed"),
        ] {
            let before = db.content_stamp().unwrap();
            let journal = scalar(&db, "SELECT COUNT(*) FROM macevery_file_changes;");
            db.begin().unwrap();
            let error = upsert.upsert(&invalid).unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error}");
            db.rollback().unwrap();
            db.begin().unwrap();
            value.indexed_at += 1;
            // This must be the first subsequent upsert call, with no sacrificial
            // reset/retry to clear a stale sqlite_step error from the old VM.
            assert!(upsert.upsert(&value).unwrap());
            db.commit().unwrap();
            assert_eq!(assert_upserted_columns(&db, &value), original_id);
            assert_eq!(scalar(&db, "SELECT COUNT(*) FROM files;"), 1);
            assert_eq!(
                scalar(&db, "SELECT COUNT(*) FROM macevery_file_changes;"),
                journal + 1
            );
            assert_eq!(db.content_stamp().unwrap().revision, before.revision + 1);
            db.begin_read().unwrap();
            let FileChangeRead::Available { changes, .. } =
                db.file_changes_since(&before, 4096, 4096).unwrap()
            else {
                panic!("failed statement rollback must not break journal coverage");
            };
            db.commit().unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].id, original_id);
            assert_eq!(
                changes[0].record.as_ref().unwrap().indexed_at,
                value.indexed_at
            );
        }
    }

    #[test]
    fn scanner_real_deferred_fk_commit_failure_rolls_back_all_state_and_retries_once() {
        let fixture = Fixture::new("prepared-scanner-commit-failure");
        let root = fixture.0.join("scan-root");
        std::fs::create_dir(&root).unwrap();
        let existing_path = root.join("existing.txt");
        std::fs::write(&existing_path, b"before").unwrap();
        let db = Database::open(fixture.database()).unwrap();
        crate::scanner::refresh_path(&db, &root, &[]).unwrap();
        db.set_meta("commit-failure-probe", "before").unwrap();
        db.set_meta("watch_last_event_id", "123").unwrap();
        db.set_meta("last_indexed_at", "456").unwrap();
        assert_eq!(scalar(&db, "PRAGMA foreign_keys;"), 1);

        let journal = || {
            let mut stmt = db.conn.prepare("SELECT revision,old_id,new_id,conflict_id FROM macevery_file_changes ORDER BY revision;").unwrap();
            let mut rows = Vec::new();
            while let Step::Row = stmt.step().unwrap() {
                rows.push([
                    stmt.column_i64(0),
                    stmt.column_i64(1),
                    stmt.column_i64(2),
                    stmt.column_i64(3),
                ]);
            }
            rows
        };
        let metadata = || {
            let mut stmt = db
                .conn
                .prepare("SELECT key,value FROM meta ORDER BY key;")
                .unwrap();
            let mut rows = Vec::new();
            while let Step::Row = stmt.step().unwrap() {
                rows.push((stmt.column_text(0).unwrap(), stmt.column_text(1).unwrap()));
            }
            rows
        };
        let before_rows = db.all_records().unwrap();
        let before = db.content_stamp().unwrap();
        let before_journal = journal();
        let before_metadata = metadata();
        db.conn
            .execute_batch(
                "CREATE TABLE commit_fault_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE commit_fault_child(
                parent_id INTEGER NOT NULL REFERENCES commit_fault_parent(id)
                DEFERRABLE INITIALLY DEFERRED
             );
             CREATE TRIGGER commit_fault_insert AFTER INSERT ON files BEGIN
                INSERT INTO commit_fault_child VALUES(1);
                UPDATE meta SET value='during-failed-scan' WHERE key='commit-failure-probe';
             END;
             CREATE TRIGGER commit_fault_update AFTER UPDATE ON files BEGIN
                INSERT INTO commit_fault_child VALUES(1);
                UPDATE meta SET value='during-failed-scan' WHERE key='commit-failure-probe';
             END;",
            )
            .unwrap();

        // Prove that the fault permits the real upsert step, then fails at
        // COMMIT rather than simulating an error in a scanner callback.
        db.begin().unwrap();
        {
            let mut upsert = db.prepare_file_upsert().unwrap();
            let mut probe = before_rows[0].clone();
            probe.indexed_at += 1;
            assert!(upsert.upsert(&probe).unwrap());
        }
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM commit_fault_child;"), 1);
        let commit_error = db.commit().unwrap_err();
        assert!(commit_error
            .to_string()
            .contains("FOREIGN KEY constraint failed"));
        assert!(!db.conn.is_autocommit());
        db.rollback().unwrap();
        assert_eq!(db.all_records().unwrap(), before_rows);
        assert_eq!(db.content_stamp().unwrap(), before);
        assert_eq!(journal(), before_journal);
        assert_eq!(metadata(), before_metadata);

        std::fs::write(&existing_path, b"after-change-longer").unwrap();
        std::fs::write(root.join("created-after.txt"), b"new").unwrap();
        let error = crate::scanner::refresh_path(&db, &root, &[]).unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY constraint failed"));
        // A failed SQLite COMMIT leaves its transaction active. The production
        // scanner must roll it back, including IDs, triggers, control metadata,
        // and every successfully stepped row in that recursive scan.
        assert!(db.conn.is_autocommit());
        assert_eq!(db.all_records().unwrap(), before_rows);
        assert_eq!(db.content_stamp().unwrap(), before);
        assert_eq!(journal(), before_journal);
        assert_eq!(metadata(), before_metadata);
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM commit_fault_child;"), 0);

        db.conn
            .execute_batch("DROP TRIGGER commit_fault_insert; DROP TRIGGER commit_fault_update;")
            .unwrap();
        let summary = crate::scanner::refresh_path(&db, &root, &[]).unwrap();
        assert_eq!(summary.indexed, 3);
        assert!(summary.warnings.is_empty());
        assert!(db.conn.is_autocommit());
        let rows = db.all_records().unwrap();
        assert_eq!(rows.len(), 3);
        for old in &before_rows {
            assert_eq!(
                rows.iter().find(|row| row.path == old.path).unwrap().id,
                old.id
            );
        }
        let updated = rows
            .iter()
            .find(|row| row.path == existing_path.to_string_lossy())
            .unwrap();
        assert_eq!(updated.size, Some(b"after-change-longer".len() as i64));
        let changed_rows = rows
            .iter()
            .filter(|row| before_rows.iter().find(|old| old.id == row.id) != Some(*row))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(summary.rows_written as usize, changed_rows.len());
        assert_eq!(
            db.content_stamp().unwrap().revision,
            before.revision + changed_rows.len() as i64
        );
        assert_eq!(journal().len(), before_journal.len() + changed_rows.len());
        assert_eq!(metadata(), before_metadata);
        db.begin_read().unwrap();
        let FileChangeRead::Available { target, changes } =
            db.file_changes_since(&before, 4096, 4096).unwrap()
        else {
            panic!("retry after failed COMMIT must retain full delta coverage");
        };
        db.commit().unwrap();
        assert_eq!(target.revision, before.revision + changed_rows.len() as i64);
        assert_eq!(
            changes
                .into_iter()
                .filter_map(|change| change.record)
                .collect::<Vec<_>>(),
            changed_rows
        );
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

    #[test]
    fn lightweight_watch_configuration_matches_stats_order_and_content() {
        let fixture = Fixture::new("watch-config");
        let db = Database::open(fixture.database()).unwrap();
        assert_eq!(db.roots_and_excludes().unwrap(), (vec![], vec![]));
        db.set_roots(&["/资料/Σ".into(), "/z".into(), "/A".into(), "/z".into()])
            .unwrap();
        db.set_excludes(&[
            "缓存".into(),
            "node_modules".into(),
            ".git".into(),
            ".git".into(),
        ])
        .unwrap();
        db.upsert_file(&record("/A/Report.txt", 7)).unwrap();
        let before = db.content_stamp().unwrap();
        let expected = db.stats().unwrap();
        let (roots, excludes) = db.roots_and_excludes().unwrap();
        assert_eq!(roots, expected.roots);
        assert_eq!(excludes, expected.excludes);
        assert_eq!(roots, ["/A", "/z", "/资料/Σ"]);
        assert_eq!(excludes, [".git", "node_modules", "缓存"]);
        assert_eq!(db.content_stamp().unwrap(), before);
    }

    #[test]
    fn lightweight_watch_configuration_does_not_query_the_files_table() {
        let fixture = Fixture::new("watch-config-no-files");
        let db = Database::open(fixture.database()).unwrap();
        db.set_roots(&["/Only/Configuration".into()]).unwrap();
        db.set_excludes(&[".git".into()]).unwrap();
        // Removing files in this isolated empty DB makes any accidental stats
        // or file-count dependency fail deterministically, without timing tests.
        db.conn.execute_batch("DROP TABLE files;").unwrap();
        let reader = Database::open_read_only(fixture.database()).unwrap();
        assert_eq!(
            reader.roots_and_excludes().unwrap(),
            (
                vec!["/Only/Configuration".to_string()],
                vec![".git".to_string()]
            )
        );
        assert!(reader.stats().is_err());
    }

    #[test]
    fn bootstrap_covers_empty_and_configured_databases_independently() {
        for has_records in [false, true] {
            for has_roots in [false, true] {
                let fixture = Fixture::new("bootstrap-grid");
                let db = Database::open(fixture.database()).unwrap();
                if has_records {
                    db.upsert_file(&record("/Unrelated/Record.txt", 7)).unwrap();
                }
                if has_roots {
                    db.set_roots(&["/资料/Σ".into(), "/A".into(), "/A".into()])
                        .unwrap();
                }
                db.set_excludes(&["缓存".into(), ".git".into()]).unwrap();
                let expected_roots: Vec<String> = if has_roots {
                    vec!["/A".into(), "/资料/Σ".into()]
                } else {
                    vec![]
                };
                let initial = db.content_stamp().unwrap();
                let status = db.bootstrap_status().unwrap();
                assert_eq!(status.db_path, fixture.database().to_string_lossy());
                assert_eq!(status.has_records, has_records);
                assert_eq!(status.roots, expected_roots);
                assert_eq!(status.excludes, [".git", "缓存"]);
                assert_eq!(status.last_indexed_at, None);
                for value in [i64::MIN, 0, i64::MAX] {
                    db.set_meta("last_indexed_at", &value.to_string()).unwrap();
                    assert_eq!(db.bootstrap_status().unwrap().last_indexed_at, Some(value));
                }
                db.set_meta("last_indexed_at", "not-an-integer").unwrap();
                assert_eq!(db.bootstrap_status().unwrap().last_indexed_at, None);
                assert_eq!(db.content_stamp().unwrap(), initial);
            }
        }
    }

    #[test]
    fn bootstrap_preserves_required_legacy_migration() {
        let fixture = Fixture::new("bootstrap-legacy");
        let legacy = Connection::open(&fixture.database()).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE files (
                    id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                    basename TEXT NOT NULL, basename_lower TEXT NOT NULL,
                    ext_lower TEXT, kind TEXT NOT NULL, size INTEGER,
                    mtime INTEGER, ctime INTEGER, dev INTEGER, inode INTEGER,
                    indexed_at INTEGER NOT NULL);
                 INSERT INTO files(id,path,basename,basename_lower,kind,indexed_at)
                    VALUES(1,'/Legacy/Σ.txt','Σ.txt','σ.txt','file',12);
                 CREATE TABLE roots(path TEXT PRIMARY KEY);
                 INSERT INTO roots VALUES('/Legacy');
                 CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta VALUES('last_indexed_at','12');",
            )
            .unwrap();
        drop(legacy);
        let db = Database::open(fixture.database()).unwrap();
        let status = db.bootstrap_status().unwrap();
        assert!(status.has_records);
        assert_eq!(status.roots, ["/Legacy"]);
        assert!(status.excludes.is_empty());
        assert_eq!(status.last_indexed_at, Some(12));
        assert_eq!(db.all_records().unwrap()[0].path_lower, "/legacy/σ.txt");
        assert!(db.has_change_journal().unwrap());
        let schema = scalar(&db, "PRAGMA schema_version;");
        drop(db);
        let reopened = Database::open(fixture.database()).unwrap();
        assert_eq!(reopened.bootstrap_status().unwrap(), status);
        assert_eq!(scalar(&reopened, "PRAGMA schema_version;"), schema);
    }

    #[test]
    fn bootstrap_read_errors_are_not_empty_and_release_the_transaction() {
        let fixture = Fixture::new("bootstrap-errors");
        let db = Database::open(fixture.database()).unwrap();
        db.upsert_file(&record("/Retained.txt", 7)).unwrap();
        db.begin().unwrap();
        db.set_meta("caller-owned-transaction", "uncommitted")
            .unwrap();
        assert!(db.bootstrap_status().is_err());
        assert_eq!(
            db.get_meta("caller-owned-transaction").unwrap().as_deref(),
            Some("uncommitted")
        );
        db.rollback().unwrap();
        assert!(db.get_meta("caller-owned-transaction").unwrap().is_none());
        // Fail after the existence query has already pinned a snapshot.
        db.conn.execute_batch("DROP TABLE roots;").unwrap();
        assert!(db.bootstrap_status().is_err());
        db.conn
            .execute_batch("CREATE TABLE roots(path TEXT PRIMARY KEY);")
            .unwrap();
        assert!(db.bootstrap_status().unwrap().has_records);
        // A broken files schema must also propagate, never becoming false.
        db.conn.execute_batch("DROP TABLE files;").unwrap();
        assert!(db.bootstrap_status().is_err());
        db.begin_read().unwrap();
        db.rollback().unwrap();

        let broken = fixture.0.join("corrupt.sqlite");
        std::fs::write(&broken, b"this is not a SQLite database").unwrap();
        assert!(Database::open(broken).is_err());
    }

    #[test]
    fn bootstrap_configuration_and_existence_share_a_wal_snapshot() {
        let fixture = Fixture::new("bootstrap-snapshot");
        let writer = Database::open(fixture.database()).unwrap();
        writer.set_roots(&["/Before".into()]).unwrap();
        writer.set_excludes(&["old-exclude".into()]).unwrap();
        writer.set_meta("last_indexed_at", "10").unwrap();
        let reader = Database::open_read_only(fixture.database()).unwrap();
        reader.begin_read().unwrap();
        let before = reader.bootstrap_status_in_snapshot().unwrap();
        assert!(!before.has_records);

        writer.begin().unwrap();
        writer.upsert_file(&record("/After/New.txt", 20)).unwrap();
        writer.set_roots(&["/After".into()]).unwrap();
        writer.set_excludes(&["new-exclude".into()]).unwrap();
        writer.set_meta("last_indexed_at", "20").unwrap();
        writer.commit().unwrap();
        assert_eq!(reader.bootstrap_status_in_snapshot().unwrap(), before);
        reader.commit().unwrap();

        let after = reader.bootstrap_status().unwrap();
        assert!(after.has_records);
        assert_eq!(after.roots, ["/After"]);
        assert_eq!(after.excludes, ["new-exclude"]);
        assert_eq!(after.last_indexed_at, Some(20));
        assert_eq!(before.roots, ["/Before"]);
        assert_eq!(before.excludes, ["old-exclude"]);
        assert_eq!(before.last_indexed_at, Some(10));
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
