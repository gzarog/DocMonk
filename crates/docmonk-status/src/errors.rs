//! The error feed: one global chronological order and redaction.
//!
//! Collectors hand over candidate errors from every source (each source's
//! newest `limit`, which is enough for a global top `limit`); [`select`]
//! orders them by the real event time, newest first, with a deterministic
//! tie break, and only then truncates. Errors without a known time sort
//! last; they are never given a made-up time.

use std::cmp::Ordering;

use crate::model::ErrorEvent;

/// Longest error message kept in a report.
pub const MAX_MESSAGE_CHARS: usize = 500;

/// Newest first; ties by source id, then path, then code.
pub fn order(a: &ErrorEvent, b: &ErrorEvent) -> Ordering {
    match (&a.occurred_at, &b.occurred_at) {
        (Some(x), Some(y)) => y.cmp(x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
    .then_with(|| a.source_id.cmp(&b.source_id))
    .then_with(|| a.path.cmp(&b.path))
    .then_with(|| a.code.cmp(&b.code))
}

/// Globally ordered, deduplicated, redacted and truncated error feed.
pub fn select(mut errors: Vec<ErrorEvent>, limit: usize, redact_paths: bool) -> Vec<ErrorEvent> {
    errors.sort_by(order);
    errors.dedup_by(|a, b| {
        a.source_id == b.source_id
            && a.path == b.path
            && a.code == b.code
            && a.message == b.message
            && a.occurred_at == b.occurred_at
    });
    errors.truncate(limit);
    for e in &mut errors {
        e.message = redact_text(&e.message);
        if redact_paths && e.path.is_some() {
            e.path = Some("<redacted>".into());
        }
    }
    errors
}

/// Removes credentials and URL userinfo from free text and bounds its size.
pub fn redact_text(text: &str) -> String {
    let redacted = docmonk_telemetry::redact::redact_urls_in_text(text);
    let mut out: String = redacted.chars().take(MAX_MESSAGE_CHARS).collect();
    if redacted.chars().count() > MAX_MESSAGE_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(source: &str, path: &str, at: Option<&str>) -> ErrorEvent {
        ErrorEvent {
            source_id: source.into(),
            path: Some(path.into()),
            code: "failed".into(),
            message: "boom".into(),
            status: Some("failed".into()),
            attempt_count: Some(1),
            occurred_at: at.map(str::to_owned),
        }
    }

    #[test]
    fn global_order_is_by_time_then_stable_ties() {
        let t1 = Some("2026-10-08T10:00:00.000000Z");
        let t2 = Some("2026-10-08T11:00:00.000000Z");
        let got = select(
            vec![
                ev("b", "z", t1),
                ev("a", "y", None),
                ev("c", "x", t2),
                ev("a", "z", t1),
                ev("a", "a", t1),
            ],
            10,
            false,
        );
        let keys: Vec<(&str, &str)> = got
            .iter()
            .map(|e| (e.source_id.as_str(), e.path.as_deref().unwrap()))
            .collect();
        assert_eq!(
            keys,
            [("c", "x"), ("a", "a"), ("a", "z"), ("b", "z"), ("a", "y")]
        );
    }

    #[test]
    fn truncation_happens_after_the_global_sort() {
        // 150 sources, each with errors; the newest 25 overall win no matter
        // which source or path they belong to.
        let mut all = Vec::new();
        for s in 0..150 {
            for k in 0..3 {
                let minute = (s * 7 + k * 13) % 600;
                all.push(ev(
                    &format!("src_{s:03}"),
                    &format!("f{k}"),
                    Some(&format!(
                        "2026-10-08T{:02}:{:02}:00.000000Z",
                        minute / 60,
                        minute % 60
                    )),
                ));
            }
        }
        let mut expected = all.clone();
        expected.sort_by(order);
        expected.truncate(25);
        let got = select(all, 25, false);
        assert_eq!(got, expected);
        assert!(got.windows(2).all(|w| w[0].occurred_at >= w[1].occurred_at));
    }

    #[test]
    fn secrets_and_paths_are_redacted() {
        let mut e = ev("s", "secret/dir/file.txt", Some("2026-10-08T10:00:00Z"));
        e.message = "GET https://user:hunter2@search.example:9200/x failed".into();
        let got = select(vec![e], 5, true);
        assert!(!got[0].message.contains("hunter2"), "{}", got[0].message);
        assert_eq!(got[0].path.as_deref(), Some("<redacted>"));
        let long = "x".repeat(2000);
        assert_eq!(redact_text(&long).chars().count(), MAX_MESSAGE_CHARS + 1);
    }
}
