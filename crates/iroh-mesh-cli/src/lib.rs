//! The `imesh` command, built on the `imesh` library: install machines over SSH or on this
//! computer, manage a cluster through contexts, and run the daemon.
//!
//! The binary is [`run`] with nothing added. An app that wants this command line with its own
//! tables, methods, and protocols calls [`run`] with a `register` function; an app that wants its
//! own command line uses the `imesh` library directly.

mod args;
mod bundle;
mod cli;
mod config;
mod connect;
mod ctx;
mod devtunnel_cli;
mod install;
mod local_state;
mod machine;
mod output;
mod pair;
mod paths;
#[cfg(windows)]
mod service;
mod sftp;
mod skill;
mod ssh;
mod ssh_client;
mod ssh_config;
mod ssh_policy;
pub mod ssh_server;

pub use cli::{Register, run};
