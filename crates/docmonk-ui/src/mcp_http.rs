//! MCP over HTTP (`docmonk serve --mcp --http`, roadmap P8): the same
//! JSON-RPC handler as the stdio server, one message per `POST /mcp`
//! (the streamable-HTTP transport without server-initiated streams).
//!
//! It runs under the Admin UI's access model: bound to loopback by
//! default, where every request acts as the local user; a non-loopback
//! bind is refused unless remote access through an authenticating TLS
//! reverse proxy is configured (`ui.remote_enabled`, docs/ui.md), and then
//! a tool call needs the proxy-asserted `admin` role. Host and Origin are
//! checked against the allowed hosts (DNS-rebinding and cross-site
//! protection): a browser page on another origin cannot call it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use docmonk_core::errors::{DocMonkError, ErrorKind};
use docmonk_core::paths::Home;

use super::{allowed_hosts, auth_guard, host_guard, plain, security_headers, AppState};
use crate::access::Access;
use crate::indexer::Indexer;

/// The host part of an `Origin` header (`scheme://host[:port]`).
fn origin_host(origin: &str) -> Option<&str> {
    origin
        .split_once("://")
        .map(|(_, rest)| rest.trim_end_matches('/'))
}

async fn origin_guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .ok()
            .and_then(origin_host)
            .is_some_and(|h| st.allowed_hosts.iter().any(|a| a == h));
        if !ok {
            return plain(StatusCode::FORBIDDEN, "Origin not allowed.");
        }
    }
    next.run(req).await
}

async fn mcp_post(body: String) -> Response {
    let reply = tokio::task::spawn_blocking(move || docmonk_mcp::handle_line(&body))
        .await
        .unwrap_or(None);
    match reply {
        // A notification or a client response: accepted, nothing to say.
        None => StatusCode::ACCEPTED.into_response(),
        Some(v) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            serde_json::to_string(&v).unwrap_or_default(),
        )
            .into_response(),
    }
}

async fn no_stream() -> Response {
    plain(
        StatusCode::METHOD_NOT_ALLOWED,
        "This server sends no server-initiated messages; POST JSON-RPC messages to /mcp.",
    )
}

/// The MCP router for `home` on `host:port` under `access` (tests bind it
/// themselves; serve it with connect info, as the UI).
pub fn router(
    home: Home,
    access: Access,
    host: &str,
    port: u16,
    max_request_bytes: usize,
) -> Router {
    let state = AppState {
        home,
        allowed_hosts: Arc::new(allowed_hosts(host, port, &access)),
        access: Arc::new(access),
        indexer: Indexer::default(),
        db: Arc::new(Mutex::new(())),
    };
    Router::new()
        .route("/mcp", post(mcp_post).get(no_stream).delete(no_stream))
        .layer(middleware::from_fn_with_state(state.clone(), auth_guard))
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .layer(middleware::from_fn_with_state(state.clone(), host_guard))
        .layer(middleware::from_fn(security_headers))
        .layer(DefaultBodyLimit::max(max_request_bytes))
        .with_state(state)
}

/// `docmonk serve --mcp --http [--host H] [--port P]`: blocks until Ctrl+C.
pub fn serve(host: &str, port: u16) -> Result<(), DocMonkError> {
    let home = docmonk_service::prepared_home()?;
    docmonk_service::query::enable_store_pool();
    let cfg = docmonk_service::load(&home)?;
    if !cfg.mcp.enabled {
        return Err(DocMonkError::new(
            ErrorKind::Config,
            "the MCP server is disabled (mcp.enabled=false in config); run `docmonk config set \
             mcp.enabled true` to enable it.",
        ));
    }
    let access = Access::for_bind(host, &cfg.ui, &|k| std::env::var(k).ok())?;
    let limit = usize::try_from(cfg.mcp.max_request_bytes).unwrap_or(1 << 20);
    let display = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let url = format!("http://{display}:{port}/mcp");
    let remote = access.is_remote();
    let app = router(home, access, host, port, limit);
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
        eprintln!("DocMonk MCP server listening on {url}");
        if remote {
            eprintln!(
                "Remote mode: authenticated admin requests through the configured proxy only."
            );
        }
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| DocMonkError::new(ErrorKind::Generic, e.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_hosts_are_parsed() {
        assert_eq!(origin_host("http://localhost:8766"), Some("localhost:8766"));
        assert_eq!(origin_host("https://evil.example/"), Some("evil.example"));
        assert_eq!(origin_host("null"), None);
    }
}
