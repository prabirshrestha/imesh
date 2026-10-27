//! imesh: machines that talk over iroh, with no control plane.
//!
//! The `imesh` binary is the CLI and the daemon. Apps build their own daemon on this crate with
//! [`daemon::Builder`]: add API methods and ALPN protocols, then call [`daemon::serve`].
//!
//! - [`daemon`]: the machine, its links to members, and its API methods.
//! - [`state`]: the replicated records and how members sync them.
//! - [`api`]: the API wire format and its reply types.
//! - [`client`]: reaching a machine from the CLI or an app, and the CLI config.
//! - [`net`]: network options, relays, and the local API socket.
//! - [`install`]: installing the daemon over SSH or on this machine.

pub mod api;
pub mod client;
pub mod daemon;
pub mod install;
pub mod net;
pub mod os;
pub mod state;
