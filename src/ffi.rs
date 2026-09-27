//! C FFI exports for SQLKit.
//!
//! Every entry point is panic-safe: Rust panics (including internal
//! invariant failures, which the library reports as errors rather than
//! panicking) are caught at the boundary and converted to error returns,
//! so a panic can never unwind into C, which would be undefined behavior.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::connection::Connection;

unsafe fn read_str(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok().map(str::to_owned)
}

/// Run `f`, converting any Rust panic into `fallback`.
fn catch<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

/// Open a file-backed database. Returns an opaque handle or null on error.
///
/// # Safety
///
/// `path` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_open(path: *const c_char) -> *mut Connection {
    catch(std::ptr::null_mut(), || {
        // SAFETY: null checked inside `read_str` via the outer contract.
        let Some(path) = read_str(path) else {
            return std::ptr::null_mut();
        };
        match Connection::open(path) {
            Ok(conn) => Box::into_raw(Box::new(conn)),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// Open a transient in-memory database.
#[no_mangle]
pub extern "C" fn sqlkit_open_memory() -> *mut Connection {
    catch(std::ptr::null_mut(), || match Connection::open_in_memory() {
        Ok(conn) => Box::into_raw(Box::new(conn)),
        Err(_) => std::ptr::null_mut(),
    })
}

/// Execute SQL without parameters. Returns 0 on success, negative on error.
///
/// # Safety
///
/// `db` and `sql` must be valid.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_exec_batch(db: *mut Connection, sql: *const c_char) -> i32 {
    catch(-4, || {
        if db.is_null() {
            return -1;
        }
        let Some(sql) = read_str(sql) else {
            return -2;
        };
        // SAFETY: `db` is non-null per check above; caller guarantees validity.
        match (*db).execute_batch(&sql) {
            Ok(()) => 0,
            Err(_) => -3,
        }
    })
}

/// Close a database handle. Safe with null; each handle must be closed
/// exactly once (double-close is undefined behavior, as in C libraries).
///
/// # Safety
///
/// `db` must be a handle returned by this API or null.
#[no_mangle]
pub unsafe extern "C" fn sqlkit_close(db: *mut Connection) {
    let _ = catch((), || {
        if !db.is_null() {
            // SAFETY: caller guarantees a live handle closed exactly once.
            drop(Box::from_raw(db));
        }
    });
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
        // SAFETY: caller guarantees ownership transfer per API contract.
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
    catch(-4, || {
        if db.is_null() {
            return -1;
        }
        let Some(sql) = read_str(sql) else {
            return -2;
        };
        // SAFETY: `db` is non-null per check above; caller guarantees validity.
        match (*db).execute(&sql, ()) {
            Ok(changed) => changed as i64,
            Err(_) => -3,
        }
    })
}
