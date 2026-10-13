//! Structured logging and redaction.
//!
//! * [`redact`]: credential redaction in URLs and free text, and userinfo
//!   detection, with standard `urlsplit` semantics.
//! * [`logging`] writes JSON-lines records to `<home>/logs/ragmonk.log`.

pub mod logging;
pub mod redact;
pub mod urlsplit;
