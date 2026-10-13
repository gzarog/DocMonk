//! Change detection feeding the daemon (`ragmonk.watcher`).
//!
//! * [`local::LocalWatcher`]: native filesystem events through `notify`
//!   (inotify, FSEvents, ReadDirectoryChangesW), falling back to `notify`'s
//!   polling watcher when native watching is unavailable. Debounced per
//!   path.
//! * [`network::NetworkWatcher`]: network roots get no reliable native
//!   events, so they are fingerprinted (path to size and mtime) on a fixed
//!   interval, and each tick reports exactly the paths that changed.

pub mod debounce;
pub mod local;
pub mod network;
