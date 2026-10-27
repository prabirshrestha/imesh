//! Operating system code. Everything with a `#[cfg]` for an OS lives here, so the rest of the
//! crate has none: `unix.rs` for Linux and macOS, `windows.rs` for Windows. Both give the same
//! functions and types.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
