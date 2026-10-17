//! Cooperative request deadlines (audit F09).
//!
//! A request (an MCP tool call) installs a [`Token`] on the thread doing
//! its work with [`Token::enter`]. Long-running layers check it without any
//! signature changes: retrieval loops call [`check`], SQLite connections
//! abort statements through a progress handler that reads [`expired`],
//! HTTP requests cap their timeout at [`remaining`] and embedding stops
//! between batches. Work spawned onto other threads carries the token with
//! [`current`] and re-enters it there.
//!
//! A thread with no token never expires, so CLI and indexing behavior is
//! unchanged.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The message cancelled work fails with.
pub const CANCELLED: &str = "request cancelled: deadline exceeded";

/// A request's deadline plus an explicit cancel flag (set when the caller
/// stops waiting, e.g. on shutdown).
#[derive(Debug, Clone)]
pub struct Token {
    deadline: Option<Instant>,
    cancelled: Arc<AtomicBool>,
}

thread_local! {
    static CURRENT: RefCell<Option<Token>> = const { RefCell::new(None) };
}

impl Token {
    /// A token expiring `timeout` from now (`None`: only explicit cancel).
    pub fn new(timeout: Option<Duration>) -> Self {
        Token {
            deadline: timeout.and_then(|t| Instant::now().checked_add(t)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_expired(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    pub fn remaining(&self) -> Option<Duration> {
        if self.cancelled.load(Ordering::Acquire) {
            return Some(Duration::ZERO);
        }
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Installs this token on the current thread until the guard drops
    /// (the previous token, if any, is restored).
    pub fn enter(&self) -> Guard {
        let previous = CURRENT.with(|c| c.replace(Some(self.clone())));
        Guard { previous }
    }
}

/// Restores the previous token on drop.
#[must_use = "the token is only active while the guard lives"]
pub struct Guard {
    previous: Option<Token>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

/// The token active on this thread, to carry onto spawned threads.
pub fn current() -> Option<Token> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Whether this thread's request has expired or been cancelled.
pub fn expired() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(Token::is_expired))
}

/// Time left for this thread's request (`None`: unbounded).
pub fn remaining() -> Option<Duration> {
    CURRENT.with(|c| c.borrow().as_ref().and_then(Token::remaining))
}

/// `Err(CANCELLED)` once this thread's request has expired.
pub fn check() -> Result<(), &'static str> {
    if expired() {
        Err(CANCELLED)
    } else {
        Ok(())
    }
}

/// Runs `f` with `token` (if any) active on this thread.
pub fn with<T>(token: Option<&Token>, f: impl FnOnce() -> T) -> T {
    let _guard = token.map(Token::enter);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_scope_to_the_thread_and_restore_on_drop() {
        assert!(!expired() && remaining().is_none());
        let t = Token::new(Some(Duration::from_secs(60)));
        {
            let _g = t.enter();
            assert!(!expired());
            assert!(remaining().unwrap() <= Duration::from_secs(60));
            // Another thread is unaffected unless the token is carried.
            let carried = current();
            std::thread::spawn(move || {
                assert!(!expired() && remaining().is_none());
                with(carried.as_ref(), || assert!(remaining().is_some()));
            })
            .join()
            .unwrap();
            t.cancel();
            assert!(expired());
            assert_eq!(check(), Err(CANCELLED));
            assert_eq!(remaining(), Some(Duration::ZERO));
        }
        assert!(!expired() && remaining().is_none());
    }

    #[test]
    fn a_passed_deadline_expires() {
        let t = Token::new(Some(Duration::ZERO));
        let _g = t.enter();
        assert!(expired());
    }
}
