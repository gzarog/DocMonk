//! A bounded, id-correlated JSON-RPC client over newline-delimited JSON
//! on a byte stream, shared by the subscription
//! runtimes.
//!
//! A reader thread frames the stream into lines, so a message split
//! across reads is reassembled. Notifications and replies to unknown ids
//! are ignored. A malformed line, an unframed message over 8 MiB or EOF
//! fails the pending call (and every later one) as an invalid response,
//! never as an answer.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

/// Parsed runtime messages buffered between the reader thread and the client.
const EVENT_BUFFER: usize = 256;
use std::time::{Duration, Instant};

use docmonk_core::errors::DocMonkError;
use serde_json::{json, Map, Value};

use crate::errors::{invalid_response, json_rpc, provider_error, timeout};
use crate::text::plain;

const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

enum Event {
    Line(Vec<u8>),
    Eof,
    Oversized,
    Failed(String),
}

/// A request's failure: the runtime's own error reply (kept apart so an
/// adapter can map its code), or anything else.
#[derive(Debug)]
pub enum RpcError {
    Remote { code: Value, message: String },
    Other(DocMonkError),
}

impl RpcError {
    /// The JSON-RPC error text shown to the user.
    pub fn into_error(self) -> DocMonkError {
        match self {
            RpcError::Remote { code, message } => {
                json_rpc(format!("runtime error {}: {message}", plain(&code)))
            }
            RpcError::Other(e) => e,
        }
    }
}

pub struct JsonRpcClient {
    writer: Box<dyn Write + Send>,
    events: Receiver<Event>,
    next_id: i64,
    fatal: Option<DocMonkError>,
    closed: bool,
}

impl JsonRpcClient {
    /// Starts reading `reader` on a background thread.
    pub fn new(reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>) -> Self {
        // Bounded: a runtime that floods output makes the reader wait for
        // the client instead of buffering without limit.
        let (tx, rx) = mpsc::sync_channel(EVENT_BUFFER);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffer: Vec<u8> = Vec::new();
            let mut chunk = vec![0u8; 65536];
            loop {
                let n = match reader.read(&mut chunk) {
                    Ok(0) => {
                        let _ = tx.send(Event::Eof);
                        return;
                    }
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ = tx.send(Event::Failed(e.to_string()));
                        return;
                    }
                };
                buffer.extend_from_slice(&chunk[..n]);
                while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let trimmed = line.trim_ascii();
                    if !trimmed.is_empty() && tx.send(Event::Line(trimmed.to_vec())).is_err() {
                        return;
                    }
                }
                if buffer.len() > MAX_MESSAGE_BYTES {
                    let _ = tx.send(Event::Oversized);
                    return;
                }
            }
        });
        Self {
            writer,
            events: rx,
            next_id: 0,
            fatal: None,
            closed: false,
        }
    }

    fn write(&mut self, payload: &Value) -> Result<(), DocMonkError> {
        let mut line = serde_json::to_vec(payload).unwrap_or_default();
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .and_then(|()| self.writer.flush())
            .map_err(|e| provider_error(format!("ai runtime transport failed: {e}")))
    }

    fn fail(&mut self, e: DocMonkError) -> RpcError {
        self.fatal = Some(e.clone());
        RpcError::Other(e)
    }

    /// Sends `method` and waits up to `timeout_s` for its reply.
    pub fn request(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout_s: f64,
    ) -> Result<Value, RpcError> {
        if self.closed {
            return Err(RpcError::Other(provider_error(
                "ai runtime client is closed",
            )));
        }
        if let Some(e) = &self.fatal {
            return Err(RpcError::Other(e.clone()));
        }
        self.next_id += 1;
        let id = self.next_id;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params.unwrap_or_else(|| json!({})),
        });
        self.write(&payload).map_err(RpcError::Other)?;
        let deadline = Instant::now()
            + Duration::try_from_secs_f64(timeout_s.max(0.0)).unwrap_or(Duration::MAX);
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            let event = match self.events.recv_timeout(wait) {
                Ok(e) => e,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(RpcError::Other(timeout(format!(
                        "ai runtime did not answer '{method}' within {timeout_s:.0}s"
                    ))))
                }
                Err(RecvTimeoutError::Disconnected) => Event::Eof,
            };
            let line = match event {
                Event::Line(l) => l,
                Event::Eof => {
                    return Err(self.fail(invalid_response(
                        "ai runtime closed the connection unexpectedly",
                    )))
                }
                Event::Oversized => {
                    return Err(self.fail(invalid_response(
                        "ai runtime sent an oversized message with no framing",
                    )))
                }
                Event::Failed(e) => {
                    return Err(
                        self.fail(provider_error(format!("ai runtime transport failed: {e}")))
                    )
                }
            };
            let message: Value = match serde_json::from_slice(&line) {
                Ok(v) => v,
                Err(e) => {
                    return Err(self.fail(invalid_response(format!(
                        "ai runtime sent malformed JSON: {e}"
                    ))))
                }
            };
            let Value::Object(message) = message else {
                return Err(self.fail(invalid_response("ai runtime sent a non-object message")));
            };
            // Notifications and late/unknown replies are tolerated.
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            match message.get("error") {
                Some(Value::Null) | None => {}
                Some(Value::Object(err)) => {
                    return Err(RpcError::Remote {
                        code: err.get("code").cloned().unwrap_or(Value::Null),
                        message: err
                            .get("message")
                            .map(plain)
                            .unwrap_or_else(|| "unknown error".into()),
                    })
                }
                Some(other) => {
                    return Err(RpcError::Remote {
                        code: Value::Null,
                        message: plain(other),
                    })
                }
            }
            return Ok(match message.get("result") {
                Some(Value::Object(r)) => Value::Object(r.clone()),
                other => {
                    let mut m = Map::new();
                    m.insert("result".into(), other.cloned().unwrap_or(Value::Null));
                    Value::Object(m)
                }
            });
        }
    }

    pub fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), DocMonkError> {
        self.write(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or_else(|| json!({})),
        }))
    }

    pub fn close(&mut self) {
        self.closed = true;
    }
}
