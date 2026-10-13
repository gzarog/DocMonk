//! Error hierarchy mapped onto RagMonk's stable CLI exit codes.

use std::fmt;

pub const EXIT_SUCCESS: u8 = 0;
pub const EXIT_GENERIC_FAILURE: u8 = 1;
pub const EXIT_INVALID_ARGUMENTS: u8 = 2;
pub const EXIT_CONFIG_ERROR: u8 = 3;
pub const EXIT_SOURCE_UNAVAILABLE: u8 = 4;
pub const EXIT_DATABASE_ERROR: u8 = 5;
pub const EXIT_INDEXING_PARTIAL_FAILURE: u8 = 6;
pub const EXIT_HEALTH_CHECK_FAILURE: u8 = 7;
pub const EXIT_SECURITY_RESTRICTION: u8 = 8;

/// One variant per error family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Generic,
    Usage,
    Config,
    SourceUnavailable,
    Database,
    IndexingPartialFailure,
    HealthCheck,
    SecurityViolation,
    RunLockTimeout,
    /// Server mode reached local-only code; maps to the config exit code.
    LocalStorageModeRequired,
    ContentChangedDuringProcessing,
}

impl ErrorKind {
    pub const fn default_exit_code(self) -> u8 {
        match self {
            ErrorKind::Generic
            | ErrorKind::RunLockTimeout
            | ErrorKind::ContentChangedDuringProcessing => EXIT_GENERIC_FAILURE,
            ErrorKind::Usage => EXIT_INVALID_ARGUMENTS,
            ErrorKind::Config | ErrorKind::LocalStorageModeRequired => EXIT_CONFIG_ERROR,
            ErrorKind::SourceUnavailable => EXIT_SOURCE_UNAVAILABLE,
            ErrorKind::Database => EXIT_DATABASE_ERROR,
            ErrorKind::IndexingPartialFailure => EXIT_INDEXING_PARTIAL_FAILURE,
            ErrorKind::HealthCheck => EXIT_HEALTH_CHECK_FAILURE,
            ErrorKind::SecurityViolation => EXIT_SECURITY_RESTRICTION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RagMonkError {
    kind: ErrorKind,
    message: String,
    exit_code: u8,
    /// The specific error class when it is a more specific
    /// subclass than `kind` names (e.g. `AiNotConfiguredError`).
    class: Option<&'static str>,
}

impl RagMonkError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            exit_code: kind.default_exit_code(),
            class: None,
        }
    }

    /// Tags the error with a specific (sub)class name.
    pub fn with_class(mut self, class: &'static str) -> Self {
        self.class = Some(class);
        self
    }

    /// The specific class name, when more specific than `kind`.
    pub fn class(&self) -> Option<&'static str> {
        self.class
    }

    /// Overrides the exit code the kind implies.
    pub fn with_exit_code(mut self, exit_code: u8) -> Self {
        self.exit_code = exit_code;
        self
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    pub fn security(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::SecurityViolation, message)
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// `RunLockTimeoutError`: the owner metadata is diagnostic only.
    pub fn run_lock_timeout(lock_path: &str, timeout_seconds: f64, owner: &LockOwner) -> Self {
        let name = lock_path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(lock_path)
            .to_owned();
        let timeout = format_g(timeout_seconds);
        let message = if owner.is_empty() {
            format!(
                "Another RagMonk process holds {name} (owner unknown); \
                 timed out after {timeout}s waiting for it."
            )
        } else {
            let mut details = Vec::new();
            if let Some(pid) = &owner.pid {
                details.push(format!("PID {pid}"));
            }
            for (label, value) in [
                ("operation", &owner.operation),
                ("source", &owner.source_id),
                ("host", &owner.hostname),
            ] {
                if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
                    details.push(format!("{label}={v}"));
                }
            }
            let joined = if details.is_empty() {
                "unknown owner".to_owned()
            } else {
                details.join(", ")
            };
            format!(
                "Another RagMonk process holds {name} ({joined}); \
                 timed out after {timeout}s waiting for it."
            )
        };
        Self::new(ErrorKind::RunLockTimeout, message)
    }
}

/// Untrusted owner metadata read from a lock file. `present` distinguishes
/// an empty object (no owner record) from a record whose fields are null.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct LockOwner {
    pub present: bool,
    pub pid: Option<String>,
    pub operation: Option<String>,
    pub source_id: Option<String>,
    pub acquired_at: Option<String>,
    pub hostname: Option<String>,
}

impl LockOwner {
    fn is_empty(&self) -> bool {
        !self.present
    }
}

/// General (`%g`) float formatting: 6 significant digits, trailing zeros
/// stripped, exponent form below 1e-4 or at/above 1e6.
pub fn format_g(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = format!("{:.5e}", value);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..6).contains(&exp) {
        let m = strip_zeros(mantissa);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{m}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (5 - exp).max(0) as usize;
        strip_zeros(&format!("{value:.decimals$}"))
    }
}

fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s.to_owned()
    }
}

impl fmt::Display for RagMonkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RagMonkError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_by_error_kind() {
        assert_eq!(RagMonkError::usage("x").exit_code(), 2);
        assert_eq!(RagMonkError::config("x").exit_code(), 3);
        assert_eq!(
            RagMonkError::new(ErrorKind::LocalStorageModeRequired, "x").exit_code(),
            3
        );
        assert_eq!(RagMonkError::security("x").exit_code(), 8);
        assert_eq!(RagMonkError::usage("x").with_exit_code(6).exit_code(), 6);
    }

    #[test]
    fn format_g_renders_shortest_float() {
        for (v, s) in [
            (30.0, "30"),
            (0.5, "0.5"),
            (1e-7, "1e-07"),
            (3600.0, "3600"),
            (2.25, "2.25"),
            (1234567.0, "1.23457e+06"),
            (0.0001, "0.0001"),
            (123456.0, "123456"),
        ] {
            assert_eq!(format_g(v), s, "{v}");
        }
    }
}
