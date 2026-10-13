//! AI error classes, as tagged [`DocMonkError`]s. Each
//! keeps the exit code of the class it derives from.

use docmonk_core::errors::{DocMonkError, ErrorKind};

fn tagged(kind: ErrorKind, class: &'static str, message: impl Into<String>) -> DocMonkError {
    DocMonkError::new(kind, message).with_class(class)
}

/// No usable provider (exit 3).
pub fn not_configured(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Config, "AiNotConfiguredError", m)
}

/// `privacy.external_ai_allowed` blocks a network provider (exit 8).
pub fn privacy_blocked(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::SecurityViolation, "AiPrivacyBlockedError", m)
}

/// The provider failed once called (exit 1).
pub fn provider_error(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Generic, "AiProviderError", m)
}

/// A subscription provider needs sign-in (exit 3).
pub fn authentication_required(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Config, "AiAuthenticationRequiredError", m)
}

/// The runtime is not installed or cannot start (exit 3).
pub fn runtime_unavailable(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Config, "AiRuntimeUnavailableError", m)
}

/// The runtime version is outside the tested range (exit 3).
pub fn unsupported_version(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Config, "AiUnsupportedVersionError", m)
}

/// A provider or policy refused the request (exit 8).
pub fn policy_blocked(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::SecurityViolation, "AiPolicyBlockedError", m)
}

/// The subscription allowance is spent (exit 1).
pub fn quota_exhausted(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Generic, "AiQuotaExhaustedError", m)
}

/// Output that cannot be trusted as a final answer (exit 1).
pub fn invalid_response(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Generic, "AiInvalidResponseError", m)
}

/// No final answer within the deadline (exit 1).
pub fn timeout(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Generic, "AiTimeoutError", m)
}

/// An error response from a stdio runtime (exit 1).
pub fn json_rpc(m: impl Into<String>) -> DocMonkError {
    tagged(ErrorKind::Generic, "JsonRpcError", m)
}
