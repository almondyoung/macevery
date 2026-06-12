use crate::error::{MacEveryError, Result};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::ptr;

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct sqlite3_stmt {
    _private: [u8; 0],
}

type Destructor = Option<unsafe extern "C" fn(*mut c_void)>;

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_INTEGER: c_int = 1;
const SQLITE_TEXT: c_int = 3;
const SQLITE_NULL: c_int = 5;
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
        destructor: Destructor,
    ) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, index: c_int, value: i64) -> c_int;
    fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, index: c_int) -> c_int;
    fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_column_type(stmt: *mut sqlite3_stmt, index: c_int) -> c_int;
    fn sqlite3_column_text(stmt: *mut sqlite3_stmt, index: c_int) -> *const u8;
    fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, index: c_int) -> i64;
}

pub struct Connection {
    raw: *mut sqlite3,
}

impl Connection {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let filename = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| MacEveryError::Sqlite("database path contains NUL byte".to_string()))?;
        let mut raw = ptr::null_mut();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX;
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
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA temp_store=MEMORY;
             PRAGMA foreign_keys=ON;",
        )?;
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
        })
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
        Ok(())
    }

    pub fn bind_text(&mut self, index: i32, value: Option<&str>) -> Result<()> {
        let code = if let Some(value) = value {
            let value = CString::new(value)
                .map_err(|_| MacEveryError::Sqlite("bound text contains NUL byte".to_string()))?;
            unsafe { sqlite3_bind_text(self.raw, index, value.as_ptr(), -1, sqlite_transient()) }
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

    pub fn column_text(&self, index: i32) -> Option<String> {
        if unsafe { sqlite3_column_type(self.raw, index) } == SQLITE_NULL {
            return None;
        }
        let ptr = unsafe { sqlite3_column_text(self.raw, index) };
        if ptr.is_null() {
            return None;
        }
        let value = unsafe { CStr::from_ptr(ptr.cast()) };
        Some(value.to_string_lossy().to_string())
    }

    pub fn column_i64(&self, index: i32) -> Option<i64> {
        match unsafe { sqlite3_column_type(self.raw, index) } {
            SQLITE_NULL => None,
            SQLITE_INTEGER => Some(unsafe { sqlite3_column_int64(self.raw, index) }),
            SQLITE_TEXT => self.column_text(index).and_then(|value| value.parse().ok()),
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

fn sqlite_transient() -> Destructor {
    unsafe { std::mem::transmute::<isize, Destructor>(-1) }
}
