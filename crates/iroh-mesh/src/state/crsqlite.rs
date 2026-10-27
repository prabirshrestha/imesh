//! Serves the statically linked cr-sqlite to SQLite's extension loader.
//!
//! Corrosion writes its bundled `crsqlite.so` to a temp dir and calls `load_extension` on it.
//! A static musl binary cannot dlopen, and Windows has no bundled build, so this registers a
//! default VFS that is the platform VFS except for the dl* methods: opening a file named
//! `crsqlite` returns a marker, and looking up `sqlite3_crsqlite_init` in it returns the copy
//! linked in by build.rs.

use std::ffi::{CStr, c_char, c_int, c_void};

use rusqlite::ffi;

unsafe extern "C" {
    fn sqlite3_crsqlite_init(db: *mut ffi::sqlite3, err: *mut *mut c_char, api: *const c_void) -> c_int;
}

const MARKER: *mut c_void = std::ptr::dangling_mut::<c_void>();

/// Must run before Corrosion opens a database. Runs once per process.
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let base = ffi::sqlite3_vfs_find(std::ptr::null());
        assert!(!base.is_null(), "no default sqlite vfs");
        let mut vfs = *base;
        vfs.zName = c"imesh-unix".as_ptr();
        vfs.pNext = std::ptr::null_mut();
        vfs.xDlOpen = Some(dl_open);
        vfs.xDlError = Some(dl_error);
        vfs.xDlSym = Some(dl_sym);
        vfs.xDlClose = Some(dl_close);
        let rc = ffi::sqlite3_vfs_register(Box::into_raw(Box::new(vfs)), 1);
        assert_eq!(rc, ffi::SQLITE_OK, "register vfs");
    });
}

unsafe extern "C" fn dl_open(_: *mut ffi::sqlite3_vfs, path: *const c_char) -> *mut c_void {
    let path = unsafe { CStr::from_ptr(path) }.to_bytes();
    if path.ends_with(b"/crsqlite") || path.ends_with(b"\\crsqlite") { MARKER } else { std::ptr::null_mut() }
}

unsafe extern "C" fn dl_error(_: *mut ffi::sqlite3_vfs, n: c_int, out: *mut c_char) {
    let msg = b"only the built-in crsqlite extension can be loaded\0";
    let len = msg.len().min(n.max(0) as usize);
    if len > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(msg.as_ptr() as *const c_char, out, len);
            *out.add(len - 1) = 0;
        }
    }
}

type Sym = unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char);

unsafe extern "C" fn dl_sym(_: *mut ffi::sqlite3_vfs, handle: *mut c_void, name: *const c_char) -> Option<Sym> {
    let name = unsafe { CStr::from_ptr(name) };
    if handle == MARKER && name.to_bytes() == b"sqlite3_crsqlite_init" {
        // SQLite casts the symbol back to the extension entry point signature before calling it.
        Some(unsafe { std::mem::transmute::<*const (), Sym>(sqlite3_crsqlite_init as *const ()) })
    } else {
        None
    }
}

unsafe extern "C" fn dl_close(_: *mut ffi::sqlite3_vfs, _: *mut c_void) {}
