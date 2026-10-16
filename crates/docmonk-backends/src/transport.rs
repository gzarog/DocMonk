//! HTTP transport with request telemetry. Credentials come only from
//! `DOCMONK_{OPENSEARCH,ELASTICSEARCH}_{USERNAME,PASSWORD,API_KEY}`, never
//! from config, and never appear in errors.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use crate::error::{BackendError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Method {
    Get,
    Put,
    Post,
    Delete,
    Head,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Put => "PUT",
            Method::Post => "POST",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(&self) -> Result<serde_json::Value> {
        if self.body.is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body)
            .map_err(|e| BackendError::Invalid(format!("invalid JSON response: {e}")))
    }
}

/// Request counts per `METHOD endpoint` (endpoint = path without index
/// names), plus payload bytes — used to prove bounded, bulk-based writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RequestStats {
    pub requests: u64,
    pub bytes_sent: u64,
    pub by_endpoint: BTreeMap<String, u64>,
    pub bulk_requests: u64,
    pub bulk_actions: u64,
    /// NDJSON payload bytes over all bulk requests.
    pub bulk_bytes: u64,
    /// The largest single bulk request, in actions and in bytes.
    pub max_bulk_actions: u64,
    pub max_bulk_bytes: u64,
}

/// Something that can send one HTTP request.
pub trait Transport: Send + Sync {
    fn send(&self, method: Method, path: &str, body: Option<(&[u8], &str)>) -> Result<Response>;
}

/// Wraps a transport, counting requests.
pub struct Counting<T: Transport> {
    inner: T,
    stats: Mutex<RequestStats>,
}

impl<T: Transport> Counting<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            stats: Mutex::new(RequestStats::default()),
        }
    }

    pub fn stats(&self) -> RequestStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn record_bulk(&self, actions: usize, bytes: usize) {
        if let Ok(mut s) = self.stats.lock() {
            s.bulk_requests += 1;
            s.bulk_actions += actions as u64;
            s.bulk_bytes += bytes as u64;
            s.max_bulk_actions = s.max_bulk_actions.max(actions as u64);
            s.max_bulk_bytes = s.max_bulk_bytes.max(bytes as u64);
        }
    }

    pub fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<(&[u8], &str)>,
    ) -> Result<Response> {
        // A request whose deadline passed issues no further calls (F09).
        if docmonk_core::deadline::expired() {
            return Err(BackendError::Transport(
                docmonk_core::deadline::CANCELLED.into(),
            ));
        }
        if let Ok(mut s) = self.stats.lock() {
            s.requests += 1;
            s.bytes_sent += body.map_or(0, |(b, _)| b.len() as u64);
            *s.by_endpoint
                .entry(format!("{} {}", method.as_str(), endpoint_of(path)))
                .or_default() += 1;
        }
        self.inner.send(method, path, body)
    }
}

/// `/{index}/_bulk?refresh=false` -> `_bulk`; `/{index}` -> `<index>`.
fn endpoint_of(path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let api: Vec<&str> = path.split('/').filter(|p| p.starts_with('_')).collect();
    if api.is_empty() {
        "<index>".into()
    } else {
        api.join("/")
    }
}

#[derive(Debug, Clone, Default)]
pub enum Auth {
    #[default]
    None,
    Basic {
        username: String,
        password: String,
    },
    ApiKey(String),
}

impl Auth {
    /// Reads the engine's credential env vars.
    pub fn from_env(engine: &str) -> Self {
        let Some([user, pass, key]) = docmonk_telemetry::redact::credential_env_vars(engine) else {
            return Auth::None;
        };
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(api_key) = get(key) {
            return Auth::ApiKey(api_key);
        }
        match (get(user), get(pass)) {
            (Some(username), Some(password)) => Auth::Basic { username, password },
            _ => Auth::None,
        }
    }
}

/// Blocking reqwest transport (rustls).
pub struct HttpTransport {
    client: reqwest::blocking::Client,
    base: String,
    auth: Auth,
}

impl HttpTransport {
    pub fn new(base_url: &str, auth: Auth, timeout: Duration, verify_tls: bool) -> Result<Self> {
        if docmonk_telemetry::redact::url_has_userinfo(base_url) {
            return Err(BackendError::Invalid(
                "server URL must not contain credentials; use the DOCMONK_<ENGINE>_* env vars"
                    .into(),
            ));
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(!verify_tls)
            .build()
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        Ok(Self {
            client,
            base: base_url.trim_end_matches('/').to_owned(),
            auth,
        })
    }
}

impl Transport for HttpTransport {
    fn send(&self, method: Method, path: &str, body: Option<(&[u8], &str)>) -> Result<Response> {
        let url = format!("{}{}", self.base, path);
        let m = match method {
            Method::Get => reqwest::Method::GET,
            Method::Put => reqwest::Method::PUT,
            Method::Post => reqwest::Method::POST,
            Method::Delete => reqwest::Method::DELETE,
            Method::Head => reqwest::Method::HEAD,
        };
        let mut req = self.client.request(m, &url);
        // Never wait past the calling request's deadline (the client's
        // configured timeout still applies when it is shorter).
        if let Some(left) = docmonk_core::deadline::remaining() {
            req = req.timeout(left.max(std::time::Duration::from_millis(1)));
        }
        req = match &self.auth {
            Auth::None => req,
            Auth::Basic { username, password } => req.basic_auth(username, Some(password)),
            Auth::ApiKey(key) => req.header("Authorization", format!("ApiKey {key}")),
        };
        if let Some((bytes, content_type)) = body {
            req = req
                .header("Content-Type", content_type)
                .body(bytes.to_vec());
        }
        let resp = req.send().map_err(|e| {
            BackendError::Transport(docmonk_telemetry::redact::redact_urls_in_text(
                &e.to_string(),
            ))
        })?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .map_err(|e| BackendError::Transport(e.to_string()))?
            .to_vec();
        Ok(Response { status, body })
    }
}

impl Transport for Box<dyn Transport> {
    fn send(&self, method: Method, path: &str, body: Option<(&[u8], &str)>) -> Result<Response> {
        (**self).send(method, path, body)
    }
}

/// The client type the backend uses: any transport plus request telemetry.
pub type Client = Counting<Box<dyn Transport>>;

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl Transport for Fake {
        fn send(&self, _: Method, _: &str, _: Option<(&[u8], &str)>) -> Result<Response> {
            Ok(Response {
                status: 200,
                body: Vec::new(),
            })
        }
    }

    /// Audit F09: an expired request issues no further backend calls.
    #[test]
    fn expired_requests_issue_no_calls() {
        let client = Counting::new(Box::new(Fake) as Box<dyn Transport>);
        assert!(client.send(Method::Get, "/x", None).is_ok());
        let token = docmonk_core::deadline::Token::new(None);
        token.cancel();
        let _g = token.enter();
        let err = client.send(Method::Get, "/x", None).unwrap_err();
        assert!(err.to_string().contains("deadline"), "{err}");
    }

    #[test]
    fn endpoints_hide_index_names() {
        assert_eq!(endpoint_of("/docmonk-chunks/_bulk?refresh=false"), "_bulk");
        assert_eq!(endpoint_of("/docmonk-files"), "<index>");
        assert_eq!(endpoint_of("/_cat/indices"), "_cat");
        assert_eq!(endpoint_of("/x/_doc/abc"), "_doc");
    }

    #[test]
    fn credentials_in_url_are_rejected() {
        let err = HttpTransport::new(
            "https://u:p@h:9200",
            Auth::None,
            Duration::from_secs(1),
            true,
        )
        .err()
        .unwrap();
        assert!(!err.to_string().contains("u:p"));
    }
}
