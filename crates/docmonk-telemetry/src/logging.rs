//! JSON-lines logging.
//!
//! Each record is one compact JSON object `{"timestamp", "level",
//! "component", "event", <extra fields>}` with an RFC 3339 UTC timestamp.
//! Emit records with `tracing` events carrying `component` and `event`
//! fields:
//!
//! ```ignore
//! tracing::info!(component = "indexing", event = "file_indexed", path = %p);
//! ```
//!
//! Only paths, ids and durations are logged, never file contents; string
//! values are passed through [`crate::redact::redact_urls_in_text`].

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

use crate::redact::redact_urls_in_text;

/// One structured field value.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Str(String),
    Int(i128),
    Float(f64),
    Bool(bool),
}

/// A formatted log record before serialization.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub timestamp: DateTime<Utc>,
    pub level: &'static str,
    pub component: String,
    pub event: String,
    pub fields: Vec<(String, FieldValue)>,
}

/// Level name for a tracing level.
pub fn level_name(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARNING",
        Level::INFO => "INFO",
        Level::DEBUG | Level::TRACE => "DEBUG",
    }
}

/// RFC 3339 UTC timestamp with microseconds, e.g. `2026-10-02T16:30:06.463119Z`.
pub fn timestamp(ts: &DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn json_str(s: &str, out: &mut String) {
    out.push_str(&serde_json::Value::from(s).to_string());
}

impl FieldValue {
    fn to_json(&self) -> serde_json::Value {
        match self {
            FieldValue::Str(s) => s.as_str().into(),
            FieldValue::Int(i) => {
                i64::try_from(*i).map_or_else(|_| i.to_string().into(), Into::into)
            }
            FieldValue::Float(f) => serde_json::Number::from_f64(*f)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            FieldValue::Bool(b) => (*b).into(),
        }
    }
}

impl Record {
    pub fn to_json_line(&self) -> String {
        let mut out = String::from("{");
        let mut first = true;
        let mut key = |out: &mut String, k: &str| {
            if !first {
                out.push(',');
            }
            first = false;
            json_str(k, out);
            out.push(':');
        };
        key(&mut out, "timestamp");
        json_str(&timestamp(&self.timestamp), &mut out);
        key(&mut out, "level");
        json_str(self.level, &mut out);
        key(&mut out, "component");
        json_str(&self.component, &mut out);
        key(&mut out, "event");
        json_str(&self.event, &mut out);
        for (k, v) in &self.fields {
            if matches!(k.as_str(), "timestamp" | "level" | "component" | "event") {
                continue;
            }
            key(&mut out, k);
            out.push_str(&v.to_json().to_string());
        }
        out.push('}');
        out
    }
}

#[derive(Default)]
struct Collector {
    component: Option<String>,
    event: Option<String>,
    message: Option<String>,
    fields: Vec<(String, FieldValue)>,
}

impl Collector {
    fn put(&mut self, field: &Field, value: FieldValue) {
        let name = field.name();
        let text = || match &value {
            FieldValue::Str(s) => s.clone(),
            FieldValue::Int(i) => i.to_string(),
            FieldValue::Float(f) => f.to_string(),
            FieldValue::Bool(b) => b.to_string(),
        };
        match name {
            "component" => self.component = Some(text()),
            "event" => self.event = Some(text()),
            "message" => self.message = Some(text()),
            _ => self.fields.push((name.to_owned(), value)),
        }
    }
}

impl Visit for Collector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, FieldValue::Str(redact_urls_in_text(value)));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, FieldValue::Int(value.into()));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, FieldValue::Int(value.into()));
    }
    fn record_i128(&mut self, field: &Field, value: i128) {
        self.put(field, FieldValue::Int(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, FieldValue::Float(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, FieldValue::Bool(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(
            field,
            FieldValue::Str(redact_urls_in_text(&format!("{value:?}"))),
        );
    }
}

/// Builds a [`Record`] from a tracing event.
pub fn record_from_event(event: &Event<'_>, now: DateTime<Utc>) -> Record {
    let mut c = Collector::default();
    event.record(&mut c);
    let meta = event.metadata();
    let event_name = c.event.or(c.message).unwrap_or_default();
    Record {
        timestamp: now,
        level: level_name(meta.level()),
        component: c.component.unwrap_or_else(|| meta.target().to_owned()),
        event: event_name,
        fields: c.fields,
    }
}

/// Appends JSON records to a file.
pub struct JsonFileLayer {
    file: Mutex<File>,
    max_level: Level,
}

impl JsonFileLayer {
    pub fn open(logs_dir: &Path, max_level: Level) -> std::io::Result<Self> {
        std::fs::create_dir_all(logs_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_dir.join("ragmonk.log"))?;
        Ok(Self {
            file: Mutex::new(file),
            max_level,
        })
    }
}

impl<S: Subscriber> Layer<S> for JsonFileLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() > self.max_level {
            return;
        }
        let mut line = record_from_event(event, Utc::now()).to_json_line();
        line.push('\n');
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// Console output: WARNING and above only.
pub struct ConsoleLayer {
    json: bool,
}

impl<S: Subscriber> Layer<S> for ConsoleLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if *event.metadata().level() > Level::WARN {
            return;
        }
        let record = record_from_event(event, Utc::now());
        let line = if self.json {
            record.to_json_line()
        } else {
            format!("{:<8} {}: {}", record.level, record.component, record.event)
        };
        eprintln!("{line}");
    }
}

/// Level string (`info`, `warning`, ...) to a tracing level.
pub fn parse_level(level: &str) -> Option<Level> {
    match level.trim().to_ascii_uppercase().as_str() {
        "DEBUG" | "NOTSET" => Some(Level::DEBUG),
        "INFO" => Some(Level::INFO),
        "WARNING" | "WARN" => Some(Level::WARN),
        "ERROR" | "CRITICAL" | "FATAL" => Some(Level::ERROR),
        _ => None,
    }
}

/// `configure_logging`: installs the global subscriber. Returns an error
/// for an unknown level.
pub fn configure_logging(
    logs_dir: &Path,
    level: &str,
    console_format: &str,
) -> std::io::Result<()> {
    let level = parse_level(level).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Unknown level: {level:?}"),
        )
    })?;
    let subscriber = tracing_subscriber::registry()
        .with(JsonFileLayer::open(logs_dir, level)?)
        .with(ConsoleLayer {
            json: console_format == "json",
        });
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let t = Utc.with_ymd_and_hms(2026, 10, 2, 16, 30, 6).unwrap()
            + chrono::Duration::microseconds(463_119);
        assert_eq!(timestamp(&t), "2026-10-02T16:30:06.463119Z");
    }

    #[test]
    fn json_line_shape() {
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let r = Record {
            timestamp: t,
            level: "INFO",
            component: "indexing".into(),
            event: "file_indexed".into(),
            fields: vec![
                ("path".into(), FieldValue::Str("/a/é".into())),
                ("ms".into(), FieldValue::Float(1.5)),
                ("n".into(), FieldValue::Int(3)),
                ("ok".into(), FieldValue::Bool(true)),
            ],
        };
        assert_eq!(
            r.to_json_line(),
            r#"{"timestamp":"2026-01-01T00:00:00.000000Z","level":"INFO","component":"indexing","event":"file_indexed","path":"/a/é","ms":1.5,"n":3,"ok":true}"#
        );
    }

    #[test]
    fn layer_writes_json_records_and_redacts() {
        let dir = tempfile::tempdir().unwrap();
        let layer = JsonFileLayer::open(dir.path(), Level::INFO).unwrap();
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                component = "backend",
                event = "connect",
                url = "https://u:p@h:9200"
            );
            tracing::debug!(component = "x", event = "dropped");
        });
        let text = std::fs::read_to_string(dir.path().join("ragmonk.log")).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["component"], "backend");
        assert_eq!(v["event"], "connect");
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["url"], "https://h:9200");
    }
}
