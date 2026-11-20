//! imesh: a cluster of machines over iroh, with a signed membership roster and no control plane.
//!
//! This crate is a library, like iroh: it runs a machine and talks to one. It has no command
//! line and reads no config files or environment variables. The `imesh` command is one program
//! built on it (the `iroh-mesh-cli` crate).
//!
//! - [`daemon`]: a machine. [`daemon::Builder`] adds an app's API methods and ALPN protocols;
//!   [`daemon::Builder::spawn`] starts it; [`daemon::Machine`] runs cluster operations.
//! - [`roster`]: who is in the cluster, as a log of signed changes that every member keeps.
//! - [`state`]: this machine's copy of the roster, and the view of the cluster it gives.
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
pub mod roster;
pub mod state;
