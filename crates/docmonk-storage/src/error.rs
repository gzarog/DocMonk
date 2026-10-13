use docmonk_core::errors::{DocMonkError, ErrorKind};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database error ({context}): {source}")]
    Sqlite {
        context: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error("{0}")]
    Io(String),
    /// The database does not have this build's schema; it is never altered.
    #[error(
        "{path} does not match this DocMonk's storage format ({found}; expected {expected}). \
         DocMonk does not convert databases: remove it (or reset the DocMonk home) and run \
         `docmonk index` to rebuild from your sources"
    )]
    IncompatibleSchema {
        path: String,
        found: String,
        expected: String,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// A server knowledge backend request failed (message already redacted).
    #[error("server backend: {0}")]
    Backend(String),
}

impl StorageError {
    pub fn sqlite(context: impl Into<String>) -> impl FnOnce(rusqlite::Error) -> Self {
        let context = context.into();
        move |source| StorageError::Sqlite { context, source }
    }
}

impl From<StorageError> for DocMonkError {
    fn from(e: StorageError) -> Self {
        let kind = match e {
            StorageError::NotFound(_) | StorageError::Invalid(_) => ErrorKind::Usage,
            _ => ErrorKind::Database,
        };
        DocMonkError::new(kind, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;
