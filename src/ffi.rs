//! C FFI exports for SQLKit.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use crate::connection::Connection;

unsafe fn read_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok().map(str::to_owned)
}

fn out_string(value: String) -> *mut c_char {
    CString::new(value).unwrap_or_default().into_raw()
}

/// Open a file-backed database. Returns an opaque handle or null on error.
///
/// # Safety
///
/// `path` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_open(path: *const c_char) -> *mut Connection {
    let Some(path) = read_str(path) else {
        return std::ptr::null_mut();
    };
    match Connection::open(path) {
        Ok(conn) => Box::into_raw(Box::new(conn)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Open a transient in-memory database.
#[no_mangle]
pub extern "C" fn sqlkit_open_memory() -> *mut Connection {
    match Connection::open_in_memory() {
        Ok(conn) => Box::into_raw(Box::new(conn)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Execute SQL without parameters. Returns 0 on success, negative on error.
///
/// # Safety
///
/// `db` and `sql` must be valid.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_exec_batch(db: *mut Connection, sql: *const c_char) -> i32 {
    if db.is_null() {
        return -1;
    }
    let Some(sql) = read_str(sql) else {
        return -2;
    };
    match (*db).execute_batch(&sql) {
        Ok(()) => 0,
        Err(_) => -3,
    }
}

/// Close a database handle.
///
/// # Safety
///
/// `db` must be a handle returned by this API or null.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_close(db: *mut Connection) {
    if !db.is_null() {
        drop(Box::from_raw(db));
    }
}

/// Library version string (do NOT free).
#[no_mangle]
pub extern "C" fn sqlkit_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Free a string returned by this library.
///
/// # Safety
///
/// `s` must be a pointer returned by this API or null.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

/// Execute a single write statement and return rows changed, or negative.
///
/// # Safety
///
/// `db` and `sql` must be valid.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_exec(db: *mut Connection, sql: *const c_char) -> i64 {
    if db.is_null() {
        return -1;
    }
    let Some(sql) = read_str(sql) else {
        return -2;
    };
    match (*db).execute(&sql, ()) {
        Ok(changed) => changed as i64,
        Err(_) => -3,
    }
}

#[allow(dead_code)]
fn _keep_out_string_alive() {
    let _ = out_string("sqlkit".to_owned());
}
