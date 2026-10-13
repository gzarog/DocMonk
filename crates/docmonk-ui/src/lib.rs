//! The local Admin UI (`docmonk ui`).
//!
//! An axum server rendering embedded Jinja templates (see [`templates`])
//! from the same services the CLI uses. It binds to localhost by default
//! and has no authentication; it validates the `Host` header
//! (DNS-rebinding defense) and requires a double-submit CSRF token on
//! every unsafe request. Database work runs on blocking threads,
//! serialized by one lock; daemon controls and the progress stream never
//! take that lock.

mod config_form;
mod data;
mod indexer;
pub mod templates;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::Home;
use serde_json::{json, Value};

use indexer::Indexer;

type Params = HashMap<String, String>;

const CSRF_COOKIE: &str = "docmonk_csrf";
const CSRF_HEADER: &str = "x-csrf-token";
const CSRF_FIELD: &str = "csrf_token";

const NAV_ITEMS: &[(&str, &str, &str)] = &[
    ("dashboard", "Dashboard", "/"),
    ("sources", "Sources", "/sources"),
    ("indexing", "Indexing", "/indexing"),
    ("documents", "Documents", "/documents"),
    ("search", "Search", "/search"),
    ("knowledge", "Knowledge", "/knowledge"),
    ("ai", "AI", "/ai"),
    ("config", "Configuration", "/config"),
    ("daemon", "Daemon", "/daemon"),
    ("health", "Health", "/health/"),
    ("backups", "Backups", "/backups"),
    ("logs", "Logs", "/logs"),
    ("system", "System", "/system"),
];

/// A JSON value as display text.
pub(crate) fn plain_json(v: &Value) -> String {
    docmonk_ai::text::plain(v)
}

#[derive(Clone)]
struct AppState {
    home: Home,
    allowed_hosts: Arc<Vec<String>>,
    indexer: Indexer,
    db: Arc<Mutex<()>>,
}

/// The CSRF token this request renders into its page.
#[derive(Clone)]
struct CsrfToken(String);

// ------------------------------------------------------------ security ---

fn allowed_hosts(host: &str, port: u16) -> Vec<String> {
    let mut out = Vec::new();
    for name in ["localhost", "127.0.0.1", "[::1]", "::1", host] {
        if name.is_empty() {
            continue;
        }
        out.push(name.to_owned());
        out.push(format!("{name}:{port}"));
    }
    out
}

fn plain(status: StatusCode, text: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text.into(),
    )
        .into_response()
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS randomness");
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut bits = 0u32;
    let mut acc = 0u32;
    for b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(ALPHABET[((acc >> bits) & 63) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (6 - bits)) & 63) as usize] as char);
    }
    out
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim_matches('"').to_owned())
}

fn query_param(uri: &axum::http::Uri, name: &str) -> Option<String> {
    let q = uri.query()?;
    serde_urlencoded_pairs(q)
        .into_iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v)
}

fn serde_urlencoded_pairs(q: &str) -> Vec<(String, String)> {
    let decode = |s: &str| -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => out.push(b' '),
                b'%' if i + 2 < bytes.len() => {
                    match std::str::from_utf8(&bytes[i + 1..i + 3])
                        .ok()
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                    {
                        Some(b) => {
                            out.push(b);
                            i += 2;
                        }
                        None => out.push(b'%'),
                    }
                }
                b => out.push(b),
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

async fn host_guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if !host.is_empty() && !st.allowed_hosts.iter().any(|a| a == host) {
        return plain(
            StatusCode::MISDIRECTED_REQUEST,
            "Host not allowed (DNS-rebinding protection).",
        );
    }
    next.run(req).await
}

async fn csrf_guard(mut req: Request, next: Next) -> Response {
    let existing = cookie(req.headers(), CSRF_COOKIE);
    let safe = matches!(
        *req.method(),
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    );
    if !safe {
        let submitted = req
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| query_param(req.uri(), CSRF_FIELD));
        let ok = matches!((&existing, &submitted), (Some(c), Some(s)) if constant_eq(c, s));
        if !ok {
            return plain(StatusCode::FORBIDDEN, "CSRF validation failed.");
        }
    }
    let minted = existing.is_none().then(new_token);
    let token = existing
        .clone()
        .or_else(|| minted.clone())
        .unwrap_or_default();
    req.extensions_mut().insert(CsrfToken(token));
    let mut resp = next.run(req).await;
    if let Some(t) = minted {
        if let Ok(v) = HeaderValue::from_str(&format!("{CSRF_COOKIE}={t}; Path=/; SameSite=strict"))
        {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    resp
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

// ----------------------------------------------------------- responses ---

fn error_response(e: &DocMonkError) -> Response {
    let status = if matches!(
        e.kind(),
        ErrorKind::LocalStorageModeRequired | ErrorKind::SourceUnavailable
    ) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    plain(status, e.message())
}

/// A 204 telling HTMX to navigate to `url`.
fn hx_redirect(url: &str) -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = HeaderValue::from_bytes(url.as_bytes()) {
        resp.headers_mut().insert("hx-redirect", v);
    }
    resp
}

fn html(name: &str, active: &str, csrf: &CsrfToken, mut ctx: Value) -> Response {
    if let Some(o) = ctx.as_object_mut() {
        o.insert(
            "nav_items".into(),
            json!(NAV_ITEMS
                .iter()
                .map(|(a, b, c)| [a, b, c])
                .collect::<Vec<_>>()),
        );
        o.insert("active".into(), json!(active));
        o.insert(
            "docmonk_version".into(),
            json!(docmonk_core::version::version()),
        );
        o.insert("csrf_token".into(), json!(csrf.0));
    }
    match templates::render(name, &ctx) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            body,
        )
            .into_response(),
        Err(e) => plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {e:#}"),
        ),
    }
}

/// Runs `f` on a blocking thread holding the database lock.
async fn locked<T: Send + 'static>(
    st: &AppState,
    f: impl FnOnce(&Home) -> T + Send + 'static,
) -> T {
    let home = st.home.clone();
    let db = st.db.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&home)
    })
    .await
    .expect("blocking task")
}

/// Runs `f` on a blocking thread without the database lock.
async fn unlocked<T: Send + 'static>(
    st: &AppState,
    f: impl FnOnce(&Home) -> T + Send + 'static,
) -> T {
    let home = st.home.clone();
    tokio::task::spawn_blocking(move || f(&home))
        .await
        .expect("blocking task")
}

fn param<'a>(p: &'a Params, k: &str) -> Option<&'a str> {
    p.get(k).map(String::as_str).filter(|v| !v.is_empty())
}

// -------------------------------------------------------------- routes ---

/// Liveness: the UI process answers. Says nothing about the backend.
async fn health_probe() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Readiness: a status snapshot of the configured backend could be taken.
/// An unreachable server (or a schema that does not match) is `503`.
async fn ready_probe(State(st): State<AppState>) -> Response {
    match locked(&st, data::report).await {
        Ok(r) => Json(json!({
            "ready": true,
            "health": r.health.state,
            "mode": r.mode,
            "backend": r.backend.kind,
            "observed_at": r.observed_at,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ready": false, "error": {"type": e.class().unwrap_or("DocMonkError"), "message": e.message()}})),
        )
            .into_response(),
    }
}

/// The canonical status report as JSON (the CLI's `status --json` data).
async fn status_api(State(st): State<AppState>) -> Response {
    match locked(&st, data::report).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => (
            if e.kind() == ErrorKind::SourceUnavailable {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            },
            Json(json!({"error": {"type": e.class().unwrap_or("DocMonkError"), "message": e.message()}})),
        )
            .into_response(),
    }
}

async fn dashboard(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
) -> Response {
    let r = locked(&st, data::dashboard).await;
    match r {
        Ok(ctx) => html("dashboard.html", "dashboard", &csrf, ctx),
        Err(e) => error_response(&e),
    }
}

async fn sources_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let error = p.get("error").cloned();
    match locked(&st, data::list_sources).await {
        Ok(sources) => html(
            "sources/list.html",
            "sources",
            &csrf,
            json!({"sources": sources, "error": error}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn source_detail(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Path(id): Path<String>,
) -> Response {
    match locked(&st, move |home| data::source_detail(home, &id)).await {
        Ok(detail) => html(
            "sources/detail.html",
            "sources",
            &csrf,
            json!({"source": detail}),
        ),
        Err(e) => hx_redirect(&format!("/sources?error={}", e.message())),
    }
}

fn split_patterns(raw: Option<&String>) -> Vec<String> {
    raw.map(|r| {
        r.replace('\n', ",")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

async fn add_source(State(st): State<AppState>, Form(f): Form<Params>) -> Response {
    let Some(path) = f.get("path").cloned() else {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"detail": [{"type": "missing", "loc": ["body", "path"], "msg": "Field required", "input": null}]}))).into_response();
    };
    let include = split_patterns(f.get("include_patterns"));
    let exclude = split_patterns(f.get("exclude_patterns"));
    let r = locked(&st, move |home| {
        let mut cp = docmonk_service::sources::catalog(home)?;
        cp.add(&path, include, exclude).map(|_| ())
    })
    .await;
    match r {
        Ok(()) => hx_redirect("/sources"),
        Err(e) => hx_redirect(&format!("/sources?error={}", e.message())),
    }
}

async fn source_action(
    State(st): State<AppState>,
    Path((id, action)): Path<(String, String)>,
) -> Response {
    let r = locked(&st, move |home| -> Result<(), DocMonkError> {
        match action.as_str() {
            "enable" | "disable" => {
                let mut cp = docmonk_service::sources::catalog(home)?;
                cp.set_enabled(&id, action == "enable")
            }
            "remove" => docmonk_service::sources::remove_source(home, &id).map(|_| ()),
            _ => Err(DocMonkError::usage("unknown action")),
        }
    })
    .await;
    match r {
        Ok(()) => hx_redirect("/sources"),
        Err(e) => hx_redirect(&format!("/sources?error={}", e.message())),
    }
}

async fn indexing_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let indexer = st.indexer.clone();
    match locked(&st, data::indexing_overview).await {
        Ok(mut overview) => {
            overview["running"] = json!(indexer.is_running());
            overview["last_summary"] = indexer.last_summary();
            html(
                "indexing/index.html",
                "indexing",
                &csrf,
                json!({"overview": overview, "error": p.get("error")}),
            )
        }
        Err(e) => error_response(&e),
    }
}

async fn failed_files(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
) -> Response {
    match locked(&st, data::failed_files).await {
        Ok(failed) => html(
            "indexing/failed.html",
            "indexing",
            &csrf,
            json!({"failed": failed}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn start_indexing(State(st): State<AppState>, Form(f): Form<Params>) -> Response {
    let source = param(&f, "source_id").map(str::to_owned);
    if !st.indexer.start(st.home.clone(), source, false, false) {
        return hx_redirect("/indexing?error=an+indexing+run+is+already+in+progress");
    }
    hx_redirect("/indexing")
}

async fn rebuild(State(st): State<AppState>, Form(f): Form<Params>) -> Response {
    let source = param(&f, "source_id").map(str::to_owned);
    let fresh = param(&f, "fresh").is_some();
    if !st.indexer.start(st.home.clone(), source, true, fresh) {
        return hx_redirect("/indexing?error=an+indexing+run+is+already+in+progress");
    }
    hx_redirect("/indexing")
}

async fn indexing_events(State(st): State<AppState>) -> Response {
    use std::time::Duration;
    const POLL: Duration = Duration::from_millis(500);
    const KEEPALIVE: Duration = Duration::from_secs(15);
    let indexer = st.indexer.clone();
    let stream = futures_util::stream::unfold(
        (indexer, 0u64, Duration::ZERO, true),
        move |(indexer, mut last, mut idle, first)| async move {
            if !first {
                tokio::time::sleep(POLL).await;
            }
            let events = indexer.events_since(last);
            let mut chunk = String::new();
            if events.is_empty() {
                idle += POLL;
                if idle >= KEEPALIVE {
                    idle = Duration::ZERO;
                    chunk.push_str(": keepalive\n\n");
                }
            } else {
                idle = Duration::ZERO;
                for e in events {
                    last = last.max(e["seq"].as_u64().unwrap_or(0));
                    chunk.push_str(&format!(
                        "event: {}\ndata: {}\n\n",
                        e["kind"].as_str().unwrap_or_default(),
                        e
                    ));
                }
            }
            Some((
                Ok::<_, std::convert::Infallible>(chunk),
                (indexer, last, idle, false),
            ))
        },
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn documents_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let source = param(&p, "source_id").map(str::to_owned);
    let q = param(&p, "q").map(str::to_owned);
    let fmt = param(&p, "fmt").map(str::to_owned);
    let page = p
        .get("page")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1);
    let filters = json!({
        "source_id": source.clone().unwrap_or_default(),
        "q": q.clone().unwrap_or_default(),
        "fmt": fmt.clone().unwrap_or_default(),
    });
    let r = locked(&st, move |home| -> Result<Value, DocMonkError> {
        Ok(json!({
            "result": data::list_documents(home, source.as_deref(), q.as_deref(), fmt.as_deref(), page)?,
            "sources": data::list_sources(home)?,
        }))
    })
    .await;
    match r {
        Ok(mut ctx) => {
            ctx["filters"] = filters;
            html("documents/list.html", "documents", &csrf, ctx)
        }
        Err(e) => error_response(&e),
    }
}

async fn document_detail(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Path((source, doc)): Path<(String, String)>,
) -> Response {
    let id = doc.clone();
    match locked(&st, move |home| data::document_detail(home, &source, &doc)).await {
        Ok(Some(d)) => html(
            "documents/detail.html",
            "documents",
            &csrf,
            json!({"document": d}),
        ),
        Ok(None) => hx_redirect(&format!("/documents?error=no such document: {id}")),
        Err(e) => error_response(&e),
    }
}

async fn search_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let q = p.get("q").cloned().unwrap_or_default();
    let mode = p.get("mode").cloned().unwrap_or_else(|| "lexical".into());
    let limit = p
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20);
    let (qq, mm) = (q.clone(), mode.clone());
    let results = if q.is_empty() {
        Ok(Value::Null)
    } else {
        locked(&st, move |home| data::run_search(home, &qq, &mm, limit)).await
    };
    match results {
        Ok(results) => html(
            "search/index.html",
            "search",
            &csrf,
            json!({"query": q, "mode": mode, "limit": limit, "results": results}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn knowledge_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let q = param(&p, "q").map(str::to_owned);
    let query = q.clone().unwrap_or_default();
    match locked(&st, move |home| data::list_symbols(home, q.as_deref(), 100)).await {
        Ok(symbols) => html(
            "knowledge/index.html",
            "knowledge",
            &csrf,
            json!({"query": query, "symbols": symbols}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn symbol_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let name = p.get("name").cloned().unwrap_or_default();
    let relation = p
        .get("relation")
        .cloned()
        .unwrap_or_else(|| "callers".into());
    let (n, r) = (name.clone(), relation.clone());
    let data = if name.is_empty() {
        Ok(Value::Null)
    } else {
        locked(&st, move |home| data::symbol_relation(home, &n, &r)).await
    };
    match data {
        Ok(data) => html(
            "knowledge/symbol.html",
            "knowledge",
            &csrf,
            json!({"name": name, "relation": relation, "data": data}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn ai_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let test = param(&p, "test").map(str::to_owned);
    let r = locked(&st, move |home| -> Result<Value, DocMonkError> {
        Ok(json!({
            "overview": data::ai_overview(home)?,
            "test_result": match &test {
                Some(t) => data::ai_test(home, t)?,
                None => Value::Null,
            },
        }))
    })
    .await;
    match r {
        Ok(ctx) => html("ai/index.html", "ai", &csrf, ctx),
        Err(e) => error_response(&e),
    }
}

async fn ai_test(Form(f): Form<Params>) -> Response {
    hx_redirect(&format!(
        "/ai?test={}",
        f.get("provider").cloned().unwrap_or_default()
    ))
}

async fn config_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    match locked(&st, config_form::describe_config).await {
        Ok(sections) => html(
            "config/index.html",
            "config",
            &csrf,
            json!({"sections": sections, "saved": p.get("saved"), "error": p.get("error")}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn save_config(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let form: Vec<(String, String)> = serde_urlencoded_pairs(&String::from_utf8_lossy(&body))
        .into_iter()
        .filter(|(k, _)| k != CSRF_FIELD)
        .collect();
    match locked(&st, move |home| config_form::apply_updates(home, &form)).await {
        Ok(()) => hx_redirect("/config?saved=1"),
        Err(e) => hx_redirect(&format!("/config?error={e}")),
    }
}

fn daemon_ctx(home: &Home) -> Value {
    let daemon = data::daemon_snapshot(home);
    let uptime = data::format_uptime(daemon["started_at"].as_str());
    json!({"daemon": daemon, "uptime": uptime})
}

async fn daemon_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let error = p.get("error").cloned();
    let mut ctx = locked(&st, |home| {
        let mut ctx = daemon_ctx(home);
        ctx["log_lines"] = json!(data::daemon_log_tail(home, 50));
        ctx
    })
    .await;
    ctx["error"] = json!(error);
    html("daemon/index.html", "daemon", &csrf, ctx)
}

async fn daemon_panel(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
) -> Response {
    let ctx = locked(&st, daemon_ctx).await;
    html("daemon/_panel.html", "daemon", &csrf, ctx)
}

async fn daemon_logs(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
) -> Response {
    let lines = locked(&st, |home| data::daemon_log_tail(home, 50)).await;
    html(
        "daemon/_logs.html",
        "daemon",
        &csrf,
        json!({"log_lines": lines}),
    )
}

async fn daemon_status_api(State(st): State<AppState>) -> Response {
    let ctx = locked(&st, daemon_ctx).await;
    let mut daemon = ctx["daemon"].clone();
    daemon["uptime"] = ctx["uptime"].clone();
    Json(daemon).into_response()
}

async fn daemon_action(State(st): State<AppState>, Path(action): Path<String>) -> Response {
    let r = unlocked(&st, move |home| -> Result<(), DocMonkError> {
        match action.as_str() {
            "start" => docmonk_service::daemon::start(home).map(drop),
            "stop" => docmonk_service::daemon::stop(home).map(drop),
            "restart" => {
                docmonk_service::daemon::stop(home)?;
                docmonk_service::daemon::start(home).map(drop)
            }
            _ => Err(DocMonkError::usage("unknown action")),
        }
    })
    .await;
    match r {
        Ok(()) => hx_redirect("/daemon"),
        Err(e) => hx_redirect(&format!("/daemon?error={}", e.message())),
    }
}

async fn health_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
) -> Response {
    match locked(&st, data::health_report).await {
        Ok(report) => html(
            "health/index.html",
            "health",
            &csrf,
            json!({"report": report}),
        ),
        Err(e) => error_response(&e),
    }
}

async fn backups_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let r = locked(&st, |home| -> Result<Value, DocMonkError> {
        Ok(json!({"backups": data::list_backups(home), "update": data::update_status(home)?}))
    })
    .await;
    match r {
        Ok(mut ctx) => {
            ctx["saved"] = json!(p.get("saved"));
            ctx["error"] = json!(p.get("error"));
            html("backups/index.html", "backups", &csrf, ctx)
        }
        Err(e) => error_response(&e),
    }
}

async fn create_backup(State(st): State<AppState>) -> Response {
    let r = locked(&st, |home| -> Result<(), DocMonkError> {
        let lock = docmonk_service::indexing::home_exclusive(home, "backup")?;
        let r = docmonk_ops::backup::create_backup(home, None);
        lock.release();
        r.map(|_| ())
    })
    .await;
    match r {
        Ok(()) => hx_redirect("/backups?saved=1"),
        Err(e) => hx_redirect(&format!("/backups?error={}", e.message())),
    }
}

async fn restore_backup(State(st): State<AppState>, Form(f): Form<Params>) -> Response {
    let name = f.get("name").cloned().unwrap_or_default();
    let r = locked(&st, move |home| -> Result<(), String> {
        let archive = data::backup_path(home, &name)?;
        docmonk_ops::backup::restore_archive(home, &archive)
            .map(|_| ())
            .map_err(|e| e.message().to_owned())
    })
    .await;
    match r {
        Ok(()) => hx_redirect("/backups?saved=restored"),
        Err(e) => hx_redirect(&format!("/backups?error={e}")),
    }
}

async fn download_backup(State(st): State<AppState>, Path(name): Path<String>) -> Response {
    let n = name.clone();
    match locked(&st, move |home| data::backup_path(home, &n)).await {
        Ok(path) => match std::fs::read(&path) {
            Ok(bytes) => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/gzip".to_owned()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{name}\""),
                    ),
                ],
                bytes,
            )
                .into_response(),
            Err(e) => plain(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        },
        Err(e) => hx_redirect(&format!("/backups?error={e}")),
    }
}

async fn logs_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
    Query(p): Query<Params>,
) -> Response {
    let level = param(&p, "level").map(str::to_owned);
    let component = param(&p, "component").map(str::to_owned);
    let q = param(&p, "q").map(str::to_owned);
    let errors_only = p
        .get("errors_only")
        .is_some_and(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "on" | "yes"));
    let filters = json!({
        "level": level.clone().unwrap_or_default(),
        "component": component.clone().unwrap_or_default(),
        "q": q.clone().unwrap_or_default(),
        "errors_only": errors_only,
    });
    let logs = locked(&st, move |home| {
        data::read_logs(
            home,
            level.as_deref(),
            component.as_deref(),
            q.as_deref(),
            errors_only,
        )
    })
    .await;
    html(
        "logs/index.html",
        "logs",
        &csrf,
        json!({"data": logs, "filters": filters}),
    )
}

async fn system_page(
    State(st): State<AppState>,
    axum::Extension(csrf): axum::Extension<CsrfToken>,
) -> Response {
    let r = locked(&st, |home| -> Result<Value, DocMonkError> {
        let cfg = docmonk_service::load(home)?;
        Ok(json!({
            "update": data::update_status(home)?,
            "tokenizer": docmonk_service::tokenizer_info(&cfg),
            "home": home.root().to_string_lossy(),
        }))
    })
    .await;
    match r {
        Ok(ctx) => html("system/index.html", "system", &csrf, ctx),
        Err(e) => error_response(&e),
    }
}

async fn static_file(Path(path): Path<String>) -> Response {
    match templates::static_asset(&path) {
        Some((ctype, bytes)) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, ctype)], bytes).into_response()
        }
        None => not_found().await,
    }
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response()
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_probe))
        .route("/ready", get(ready_probe))
        .route("/api/status", get(status_api))
        .route("/events/indexing", get(indexing_events))
        .route("/", get(dashboard))
        .route("/sources", get(sources_page).post(add_source))
        .route("/sources/{id}", get(source_detail))
        .route("/sources/{id}/{action}", post(source_action))
        .route("/indexing", get(indexing_page))
        .route("/indexing/failed", get(failed_files))
        .route("/indexing/start", post(start_indexing))
        .route("/indexing/rebuild", post(rebuild))
        .route("/documents", get(documents_page))
        .route("/documents/{source}/{doc}", get(document_detail))
        .route("/search", get(search_page))
        .route("/knowledge", get(knowledge_page))
        .route("/knowledge/symbol", get(symbol_page))
        .route("/ai", get(ai_page))
        .route("/ai/test", post(ai_test))
        .route("/config", get(config_page).post(save_config))
        .route("/daemon", get(daemon_page))
        .route("/daemon/panel", get(daemon_panel))
        .route("/daemon/logs", get(daemon_logs))
        .route("/daemon/api/status", get(daemon_status_api))
        .route("/daemon/{action}", post(daemon_action))
        .route("/health/", get(health_page))
        .route("/backups", get(backups_page).post(create_backup))
        .route("/backups/restore", post(restore_backup))
        .route("/backups/download/{name}", get(download_backup))
        .route("/logs", get(logs_page))
        .route("/system", get(system_page))
        .route("/static/{*path}", get(static_file))
        .fallback(not_found)
        .layer(middleware::from_fn(csrf_guard))
        .layer(middleware::from_fn_with_state(state.clone(), host_guard))
        .with_state(state)
}

fn open_browser(url: &str) {
    let url = url.to_owned();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut cmd = if cfg!(target_os = "macos") {
            std::process::Command::new("open")
        } else if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "start", ""]);
            c
        } else {
            std::process::Command::new("xdg-open")
        };
        // A headless host simply has no browser: the URL is printed.
        let _ = cmd
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    });
}

/// `docmonk ui [--host H] [--port P] [--no-browser]`: blocks until
/// Ctrl+C.
pub fn serve(host: &str, port: u16, no_browser: bool) -> Result<(), DocMonkError> {
    let home = docmonk_service::prepared_home()?;
    docmonk_service::load(&home)?;
    let display_host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let url = format!("http://{display_host}:{port}");
    println!("DocMonk Admin UI");
    println!("URL: {url}");
    if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
        println!(
            "Warning: binding to a non-localhost address exposes the admin interface on the \
             network. It has no authentication; only do this on a trusted network."
        );
    }
    let state = AppState {
        home,
        allowed_hosts: Arc::new(allowed_hosts(host, port)),
        indexer: Indexer::default(),
        db: Arc::new(Mutex::new(())),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| DocMonkError::new(ErrorKind::Generic, e.to_string()))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((host, port))
            .await
            .map_err(|e| {
                DocMonkError::new(ErrorKind::Generic, format!("cannot listen on {url}: {e}"))
            })?;
        if !no_browser {
            println!("Opening browser...");
            open_browser(&url);
        }
        println!("Press Ctrl+C to stop.");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| DocMonkError::new(ErrorKind::Generic, e.to_string()))
    })
}
