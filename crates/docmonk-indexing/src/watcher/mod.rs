//! Change detection feeding the daemon (`docmonk.watcher`).
//!
//! * [`local::LocalWatcher`]: native filesystem events through `notify`
//!   (inotify, FSEvents, ReadDirectoryChangesW), falling back to `notify`'s
//!   polling watcher when native watching is unavailable. Debounced per
//!   path.
//! * [`network::NetworkWatcher`]: network roots get no reliable native
//!   events, so they are fingerprinted (path to size and mtime) on a fixed
//!   interval, and each tick reports exactly the paths that changed.
//! * [`git::GitWatcher`]: git sources ask their remote for the branch head
//!   and trigger a full pass when it moved.

pub mod debounce;
pub mod git;
pub mod local;
pub mod network;
