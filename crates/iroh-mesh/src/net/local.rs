//! The local API endpoint: a unix socket on Linux and macOS, a named pipe on Windows.

pub use crate::platform::{Listener, connect};

/// Who may open the local API besides the daemon's user. Anyone who can open it has full control
/// of the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Access {
    /// Only the daemon's user (and SYSTEM on Windows): a unix socket with mode 0600.
    #[default]
    Owner,
    /// Also the members of this group: a unix socket with mode 0660 that belongs to the group, or
    /// a pipe that also lets in Administrators and the members of this local group.
    Group(String),
}
