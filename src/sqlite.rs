use crate::error::{MacEveryError, Result};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::slice;
use std::str;

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct sqlite3_stmt {
    _private: [u8; 0],
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_INTEGER: c_int = 1;
const SQLITE_TEXT: c_int = 3;
const SQLITE_NULL: c_int = 5;
const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;

#[link(name = "sqlite3")]
extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        pp_db: *mut *mut sqlite3,
        flags: c_int,
        z_vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close(db: *mut sqlite3) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_busy_timeout(db: *mut sqlite3, ms: c_int) -> c_int;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        callback: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        err_msg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_free(value: *mut c_void);
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        pp_stmt: *mut *mut sqlite3_stmt,
        pz_tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_clear_bindings(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_bind_text(
        stmt: *mut sqlite3_stmt,
        index: c_int,
        value: *const c_char,
        n: c_int,
        destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, index: c_int, value: i64) -> c_int;
    fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, index: c_int) -> c_int;
    fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_column_type(stmt: *mut sqlite3_stmt, index: c_int) -> c_int;
    fn sqlite3_column_text(stmt: *mut sqlite3_stmt, index: c_int) -> *const u8;
    fn sqlite3_column_bytes(stmt: *mut sqlite3_stmt, index: c_int) -> c_int;
    fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, index: c_int) -> i64;
    fn sqlite3_changes64(db: *mut sqlite3) -> i64;
    fn sqlite3_get_autocommit(db: *mut sqlite3) -> c_int;
}

pub struct Connection {
    raw: *mut sqlite3,
}

// Every connection is opened FULLMUTEX. Moving exclusive ownership to another
// thread is safe; Connection deliberately is not Sync, and Statement borrows
// prevent moving a connection while any statement is alive.
unsafe impl Send for Connection {}

impl Connection {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_flags(path, false, true)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        Self::open_with_flags(path, true, false)
    }

    pub fn open_existing(path: &Path) -> Result<Self> {
        Self::open_with_flags(path, false, false)
    }

    fn open_with_flags(path: &Path, read_only: bool, create: bool) -> Result<Self> {
        if create {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let filename = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| MacEveryError::Sqlite("database path contains NUL byte".to_string()))?;
        let mut raw = ptr::null_mut();
        let flags = SQLITE_OPEN_FULLMUTEX
            | if read_only {
                SQLITE_OPEN_READONLY
            } else {
                SQLITE_OPEN_READWRITE | if create { SQLITE_OPEN_CREATE } else { 0 }
            };
        let code = unsafe { sqlite3_open_v2(filename.as_ptr(), &mut raw, flags, ptr::null()) };
        if code != SQLITE_OK {
            let message = if raw.is_null() {
                format!("open failed with code {code}")
            } else {
                unsafe { errmsg(raw) }
            };
            if !raw.is_null() {
                unsafe {
                    sqlite3_close(raw);
                }
            }
            return Err(MacEveryError::Sqlite(message));
        }

        let conn = Self { raw };
        let code = unsafe { sqlite3_busy_timeout(conn.raw, 5000) };
        if code != SQLITE_OK {
            return Err(MacEveryError::Sqlite(unsafe { errmsg(conn.raw) }));
        }
        if !read_only {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        }
        conn.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA foreign_keys=ON;")?;
        Ok(conn)
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let sql = CString::new(sql)
            .map_err(|_| MacEveryError::Sqlite("SQL contains NUL byte".to_string()))?;
        let mut err_msg = ptr::null_mut();
        let code =
            unsafe { sqlite3_exec(self.raw, sql.as_ptr(), None, ptr::null_mut(), &mut err_msg) };
        if code != SQLITE_OK {
            let message = if err_msg.is_null() {
                unsafe { errmsg(self.raw) }
            } else {
                let value = unsafe { CStr::from_ptr(err_msg).to_string_lossy().to_string() };
                unsafe {
                    sqlite3_free(err_msg.cast());
                }
                value
            };
            return Err(MacEveryError::Sqlite(message));
        }
        Ok(())
    }

    pub fn prepare<'conn>(&'conn self, sql: &str) -> Result<Statement<'conn>> {
        let sql = CString::new(sql)
            .map_err(|_| MacEveryError::Sqlite("SQL contains NUL byte".to_string()))?;
        let mut raw_stmt = ptr::null_mut();
        let code = unsafe {
            sqlite3_prepare_v2(self.raw, sql.as_ptr(), -1, &mut raw_stmt, ptr::null_mut())
        };
        if code != SQLITE_OK {
            return Err(MacEveryError::Sqlite(unsafe { errmsg(self.raw) }));
        }
        Ok(Statement {
            conn: self,
            raw: raw_stmt,
            bound_text: Vec::new(),
        })
    }

    pub fn is_autocommit(&self) -> bool {
        unsafe { sqlite3_get_autocommit(self.raw) != 0 }
    }

    pub fn changes(&self) -> u64 {
        unsafe { sqlite3_changes64(self.raw).max(0) as u64 }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sqlite3_close(self.raw);
            }
        }
    }
}

pub struct Statement<'conn> {
    conn: &'conn Connection,
    raw: *mut sqlite3_stmt,
    bound_text: Vec<CString>,
}

impl Statement<'_> {
    pub fn reset(&mut self) -> Result<()> {
        let code = unsafe { sqlite3_reset(self.raw) };
        if code != SQLITE_OK && code != SQLITE_DONE {
            return Err(MacEveryError::Sqlite(unsafe { errmsg(self.conn.raw) }));
        }
        let code = unsafe { sqlite3_clear_bindings(self.raw) };
        if code != SQLITE_OK {
            return Err(MacEveryError::Sqlite(unsafe { errmsg(self.conn.raw) }));
        }
        self.bound_text.clear();
        Ok(())
    }

    pub fn bind_text(&mut self, index: i32, value: Option<&str>) -> Result<()> {
        let code = if let Some(value) = value {
            let value = CString::new(value)
                .map_err(|_| MacEveryError::Sqlite("bound text contains NUL byte".to_string()))?;
            let ptr = value.as_ptr();
            self.bound_text.push(value);
            unsafe { sqlite3_bind_text(self.raw, index, ptr, -1, None) }
        } else {
            unsafe { sqlite3_bind_null(self.raw, index) }
        };
        if code != SQLITE_OK {
            return Err(MacEveryError::Sqlite(unsafe { errmsg(self.conn.raw) }));
        }
        Ok(())
    }

    pub fn bind_i64(&mut self, index: i32, value: Option<i64>) -> Result<()> {
        let code = if let Some(value) = value {
            unsafe { sqlite3_bind_int64(self.raw, index, value) }
        } else {
            unsafe { sqlite3_bind_null(self.raw, index) }
        };
        if code != SQLITE_OK {
            return Err(MacEveryError::Sqlite(unsafe { errmsg(self.conn.raw) }));
        }
        Ok(())
    }

    pub fn step(&mut self) -> Result<Step> {
        match unsafe { sqlite3_step(self.raw) } {
            SQLITE_ROW => Ok(Step::Row),
            SQLITE_DONE => Ok(Step::Done),
            _ => Err(MacEveryError::Sqlite(unsafe { errmsg(self.conn.raw) })),
        }
    }

    pub fn column_count(&self) -> usize {
        unsafe { sqlite3_column_count(self.raw) as usize }
    }

    pub fn column_byte_len(&self, index: i32) -> usize {
        unsafe { sqlite3_column_bytes(self.raw, index).max(0) as usize }
    }

    pub fn column_text(&self, index: i32) -> Option<String> {
        self.column_str(index).map(ToOwned::to_owned)
    }

    pub fn column_str(&self, index: i32) -> Option<&str> {
        // SQLite maps SQL NULL to a null text pointer. Request UTF-8 first,
        // then its byte length, preserving empty strings and embedded NULs.
        let ptr = unsafe { sqlite3_column_text(self.raw, index) };
        if ptr.is_null() {
            return None;
        }
        let len = unsafe { sqlite3_column_bytes(self.raw, index) };
        if len < 0 {
            return None;
        }
        let bytes = unsafe { slice::from_raw_parts(ptr, len as usize) };
        str::from_utf8(bytes).ok()
    }

    pub fn column_i64(&self, index: i32) -> Option<i64> {
        match unsafe { sqlite3_column_type(self.raw, index) } {
            SQLITE_NULL => None,
            SQLITE_INTEGER => Some(unsafe { sqlite3_column_int64(self.raw, index) }),
            SQLITE_TEXT => self.column_str(index).and_then(|value| value.parse().ok()),
            _ => None,
        }
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sqlite3_finalize(self.raw);
            }
        }
    }
}

pub enum Step {
    Row,
    Done,
}

unsafe fn errmsg(db: *mut sqlite3) -> String {
    let ptr = sqlite3_errmsg(db);
    if ptr.is_null() {
        "unknown SQLite error".to_string()
    } else {
        CStr::from_ptr(ptr).to_string_lossy().to_string()
    }
}

#[cfg(test)]
mod column_tests {
    use super::*;
    use std::path::PathBuf;
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
            let directory = std::env::temp_dir().join(format!(
                "macevery-sqlite-columns-{}-{nanos}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&directory).unwrap();
            Self(directory)
        }

        fn database(&self) -> PathBuf {
            self.0.join("columns.sqlite")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Freeze the previous implementation so SQLite coercion and malformed-text
    // behavior are compared independently of the new pointer-first helper.
    fn previous_column_str<'a>(statement: &'a Statement<'_>, index: i32) -> Option<&'a str> {
        if unsafe { sqlite3_column_type(statement.raw, index) } == SQLITE_NULL {
            return None;
        }
        let pointer = unsafe { sqlite3_column_text(statement.raw, index) };
        if pointer.is_null() {
            return None;
        }
        let length = unsafe { sqlite3_column_bytes(statement.raw, index) };
        if length < 0 {
            return None;
        }
        let bytes = unsafe { slice::from_raw_parts(pointer, length as usize) };
        str::from_utf8(bytes).ok()
    }

    #[test]
    fn pointer_first_text_matches_previous_null_empty_nul_blob_and_numeric_reads() {
        let fixture = Fixture::new();
        let connection = Connection::open(&fixture.database()).unwrap();
        for expression in [
            "NULL",
            "''",
            "'报告-ΟΣ-İ'",
            "'a'||char(0)||'b'",
            "CAST(x'80ff' AS TEXT)",
            "x''",
            "x'616263'",
            "x'610062'",
            "x'80ff'",
            "zeroblob(0)",
            "123",
            "-9223372036854775808",
            "9223372036854775807",
            "1.25",
            "'00042'",
            "'not-a-number'",
        ] {
            let sql = format!("SELECT {expression};");
            let mut current = connection.prepare(&sql).unwrap();
            let mut previous = connection.prepare(&sql).unwrap();
            assert!(matches!(current.step().unwrap(), Step::Row));
            assert!(matches!(previous.step().unwrap(), Step::Row));
            let expected = previous_column_str(&previous, 0);
            assert_eq!(current.column_str(0), expected, "{expression}");
            assert_eq!(
                current.column_text(0).as_deref(),
                expected,
                "owned {expression}"
            );
        }
        let mut statement = connection
            .prepare("SELECT NULL,'','a'||char(0)||'b',CAST(x'80ff' AS TEXT),x'616263',123;")
            .unwrap();
        assert!(matches!(statement.step().unwrap(), Step::Row));
        assert_eq!(statement.column_str(0), None);
        assert_eq!(statement.column_str(1), Some(""));
        assert_eq!(statement.column_str(2), Some("a\0b"));
        assert_eq!(statement.column_str(3), None);
        assert_eq!(statement.column_str(4), Some("abc"));
        assert_eq!(statement.column_str(5), Some("123"));
    }

    #[test]
    fn integer_reads_keep_strict_existing_type_and_parse_semantics() {
        let fixture = Fixture::new();
        let connection = Connection::open(&fixture.database()).unwrap();
        for (expression, expected) in [
            ("NULL", None),
            ("-9223372036854775808", Some(i64::MIN)),
            ("9223372036854775807", Some(i64::MAX)),
            ("0", Some(0)),
            ("'00042'", Some(42)),
            ("'+42'", Some(42)),
            ("'-42'", Some(-42)),
            ("' 42'", None),
            ("'42 '", None),
            ("'42.0'", None),
            ("42.0", None),
            ("x'3432'", None),
            ("'9223372036854775808'", None),
            ("'42suffix'", None),
            ("'42'||char(0)", None),
            ("CAST(x'80ff' AS TEXT)", None),
        ] {
            let mut statement = connection
                .prepare(&format!("SELECT {expression};"))
                .unwrap();
            assert!(matches!(statement.step().unwrap(), Step::Row));
            assert_eq!(statement.column_i64(0), expected, "{expression}");
        }
    }

    #[test]
    fn text_binding_still_rejects_nul_but_accepts_empty_text() {
        let fixture = Fixture::new();
        let connection = Connection::open(&fixture.database()).unwrap();
        let mut statement = connection.prepare("SELECT ?1;").unwrap();
        assert!(statement.bind_text(1, Some("a\0b")).is_err());
        statement.bind_text(1, Some("")).unwrap();
        assert!(matches!(statement.step().unwrap(), Step::Row));
        assert_eq!(statement.column_str(0), Some(""));
    }
}
