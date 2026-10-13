use docmonk_core::errors::{ErrorKind, RagMonkError};

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// Connection/timeout/TLS problem; message is already URL-redacted.
    #[error("server unreachable: {0}")]
    Transport(String),
    #[error("server returned HTTP {status} for {method} {path}: {reason}")]
    Http {
        method: String,
        path: String,
        status: u16,
        reason: String,
    },
    #[error("bulk write failed for {failed} of {total} action(s): {first_error}")]
    Bulk {
        failed: usize,
        total: usize,
        first_error: String,
    },
    /// An existing index does not match the schema this build expects.
    #[error("{0}")]
    SchemaMismatch(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

impl From<BackendError> for RagMonkError {
    fn from(e: BackendError) -> Self {
        let (kind, class) = match e {
            BackendError::Transport(_) => (ErrorKind::SourceUnavailable, "BackendUnavailableError"),
            BackendError::SchemaMismatch(_) => (ErrorKind::Config, "BackendSchemaMismatchError"),
            BackendError::Invalid(_) => (ErrorKind::Config, "BackendConfigError"),
            BackendError::NotFound(_) => (ErrorKind::Usage, "BackendNotFoundError"),
            BackendError::Conflict(_) => (ErrorKind::Generic, "BackendConflictError"),
            BackendError::Http { .. } | BackendError::Bulk { .. } => {
                (ErrorKind::Database, "BackendRequestError")
            }
        };
        RagMonkError::new(
            kind,
            docmonk_telemetry::redact::redact_urls_in_text(&e.to_string()),
        )
        .with_class(class)
    }
}

pub type Result<T> = std::result::Result<T, BackendError>;
