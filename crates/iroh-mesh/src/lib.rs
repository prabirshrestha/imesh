//! imesh: a cluster of equal machines over iroh, with one replicated SQLite database and no
//! control plane.
//!
//! This crate is a library, like iroh: it runs a machine and talks to one. It has no command
//! line and reads no config files or environment variables. The `imesh` command is one program
//! built on it (the `iroh-mesh-cli` crate).
//!
//! - [`daemon`]: a machine. [`daemon::Builder`] adds an app's tables, API methods, and ALPN
//!   protocols; [`daemon::Builder::spawn`] starts it; [`daemon::Machine`] runs cluster operations.
//! - [`state`]: the replicated database: embedded Corrosion, with the schema gate.
//! - [`client`]: calling a machine's API over its local socket, over iroh, or any stream.
//! - [`api`]: the API wire format, its reply types, and join tickets.
//! - [`net`]: network settings, relays, and the local API socket.
//! - [`os`]: small helpers for daemons: signals, systemd readiness, private files.

pub mod api;
pub mod client;
pub mod daemon;
pub mod net;
pub mod os;
mod platform;
pub mod state;
