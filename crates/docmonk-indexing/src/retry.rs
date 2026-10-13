//! Exponential backoff for transient indexing failures.

pub const BASE_DELAY_SECONDS: f64 = 2.0;
pub const MAX_DELAY_SECONDS: f64 = 300.0;
pub const MAX_ATTEMPTS: i64 = 5;

/// `attempt` is the 1-based number of attempts made so far.
pub fn backoff_seconds(attempt: i64) -> f64 {
    let attempt = attempt.max(1);
    (BASE_DELAY_SECONDS * 2f64.powi((attempt - 1).min(30) as i32)).min(MAX_DELAY_SECONDS)
}

pub fn next_attempt_at(attempt: i64, now: chrono::DateTime<chrono::Utc>) -> String {
    let delay = chrono::Duration::milliseconds((backoff_seconds(attempt) * 1000.0) as i64);
    (now + delay).to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

pub fn is_permanent(attempt: i64) -> bool {
    attempt >= MAX_ATTEMPTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_and_permanence_policy() {
        assert_eq!(backoff_seconds(0), 2.0);
        assert_eq!(backoff_seconds(1), 2.0);
        assert_eq!(backoff_seconds(3), 8.0);
        assert_eq!(backoff_seconds(9), 300.0);
        assert!(!is_permanent(4));
        assert!(is_permanent(5));
    }
}
