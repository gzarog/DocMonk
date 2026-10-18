//! Persisted retry state of consumers whose cross-source dependencies are
//! not current (survives restarts): attempts, the last error and when to
//! try again, with exponential backoff so a dependency that cannot resolve
//! never loops busily. Host-local: the process that runs the dependency
//! stage owns it; status reads it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use docmonk_core::paths::Home;
use serde::{Deserialize, Serialize};

/// First retry delay; doubled per failed attempt up to [`MAX_DELAY`].
pub const BASE_DELAY: chrono::Duration = chrono::Duration::seconds(30);
pub const MAX_DELAY: chrono::Duration = chrono::Duration::hours(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryEntry {
    pub code: String,
    pub last_error: String,
    pub attempts: u32,
    pub first_failed_at: String,
    pub last_attempt_at: String,
    pub next_attempt_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryBook {
    #[serde(default)]
    pub consumers: BTreeMap<String, RetryEntry>,
}

pub fn path(home: &Home) -> std::path::PathBuf {
    home.state_dir().join("dependency-retries.json")
}

/// The delay before attempt `attempts + 1`.
pub fn delay(attempts: u32) -> chrono::Duration {
    let shift = attempts.saturating_sub(1).min(16);
    (BASE_DELAY * (1i32 << shift)).min(MAX_DELAY)
}

impl RetryBook {
    /// The persisted book (empty when missing or unreadable: the next
    /// full dependency stage rebuilds it).
    pub fn load(home: &Home) -> Self {
        std::fs::read(path(home))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, home: &Home) -> std::io::Result<()> {
        let p = path(home);
        if self.consumers.is_empty() {
            return match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &p)
    }

    /// Records a failed attempt of an unresolved consumer and
    /// schedules the next one with backoff.
    pub fn failed(&mut self, consumer: &str, code: &str, reason: &str, now: DateTime<Utc>) {
        let at = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let prev = self.consumers.remove(consumer);
        let attempts = prev.as_ref().map_or(0, |p| p.attempts) + 1;
        self.consumers.insert(
            consumer.to_owned(),
            RetryEntry {
                code: code.to_owned(),
                last_error: reason.to_owned(),
                attempts,
                first_failed_at: prev.map_or_else(|| at.clone(), |p| p.first_failed_at),
                last_attempt_at: at,
                next_attempt_at: (now + delay(attempts))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            },
        );
    }

    pub fn clear(&mut self, consumer: &str) {
        self.consumers.remove(consumer);
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.consumers.retain(|id, _| keep(id));
    }

    /// Consumers whose next attempt is due at `now`.
    pub fn due(&self, now: DateTime<Utc>) -> BTreeSet<String> {
        self.consumers
            .iter()
            .filter(|(_, e)| {
                DateTime::parse_from_rfc3339(&e.next_attempt_at).map_or(true, |t| t <= now)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The earliest scheduled attempt.
    pub fn next_due(&self) -> Option<DateTime<Utc>> {
        self.consumers
            .values()
            .filter_map(|e| DateTime::parse_from_rfc3339(&e.next_attempt_at).ok())
            .map(|t| t.with_timezone(&Utc))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(delay(1), chrono::Duration::seconds(30));
        assert_eq!(delay(2), chrono::Duration::seconds(60));
        assert_eq!(delay(4), chrono::Duration::seconds(240));
        assert_eq!(delay(40), MAX_DELAY);
    }

    #[test]
    fn retries_survive_a_restart_and_become_due_after_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let now = t("2026-01-01T00:00:00Z");
        let mut book = RetryBook::default();
        book.failed("app", "producer_lookup_failed", "server down", now);
        book.failed("app", "producer_lookup_failed", "server down", now);
        book.save(&home).unwrap();

        let book = RetryBook::load(&home);
        let e = &book.consumers["app"];
        assert_eq!(e.attempts, 2);
        assert_eq!(e.first_failed_at, "2026-01-01T00:00:00Z");
        assert_eq!(e.next_attempt_at, "2026-01-01T00:01:00Z");
        assert!(book.due(t("2026-01-01T00:00:59Z")).is_empty());
        assert!(book.due(t("2026-01-01T00:01:00Z")).contains("app"));
        assert_eq!(book.next_due(), Some(t("2026-01-01T00:01:00Z")));

        let mut book = book;
        book.clear("app");
        book.save(&home).unwrap();
        assert!(!path(&home).exists());
        assert_eq!(RetryBook::load(&home), RetryBook::default());
    }
}
