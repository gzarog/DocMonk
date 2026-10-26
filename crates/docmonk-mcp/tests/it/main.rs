//! The docmonk-mcp integration tests, one module per former test
//! binary: a single test executable per crate keeps debug builds
//! small (each binary links the whole dependency graph). Run one
//! group with `cargo test -p docmonk-mcp --test it -- <module>::`.

mod agent_flow;
mod execution;
mod protocol;
mod secrets;
mod visibility;

/// Tests that point the MCP server at a home (`DOCMONK_HOME`, read by every
/// tool call, as in the real `docmonk mcp` process) run one at a time.
pub fn home_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}
