//! MCP over HTTP (roadmap P8) against a bound server: JSON-RPC over
//! `POST /mcp`, notifications accepted with 202, no server streams, Host
//! and Origin checks, a non-loopback bind refused without remote mode,
//! and in remote mode only proxy-authenticated admins get through.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use docmonk_config::model::UiConfig;
use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_ui::access::Access;
use serde_json::Value;

const SECRET: &str = "0123456789abcdef0123456789abcdef-proxy";
const PUBLIC: &str = "docmonk.example.com";

fn start(home: &Home, access: Access) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let app = docmonk_ui::mcp_http::router(home.clone(), access, "127.0.0.1", port, 1 << 20);
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let l = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
                    .await
                    .unwrap();
            });
    });
    port
}

fn send(port: u16, method: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut req = format!("{method} /mcp HTTP/1.1\r\nconnection: close\r\n");
    if !headers.iter().any(|(k, _)| *k == "host") {
        req.push_str(&format!("host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if method == "POST" {
        req.push_str("content-type: application/json\r\n");
        req.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    (
        head.split(' ').nth(1).unwrap().parse().unwrap(),
        body.to_owned(),
    )
}

/// The JSON-RPC reply in a (possibly chunked) response body.
fn json_of(body: &str) -> Value {
    let start = body.find('{').expect(body);
    let end = body.rfind('}').expect(body);
    serde_json::from_str(&body[start..=end]).expect(body)
}

#[test]
fn mcp_over_http_is_json_rpc_behind_the_ui_access_model() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    // The only test in this binary: the JSON-RPC handler reads its home
    // from the environment, like the real server process.
    std::env::set_var("DOCMONK_HOME", home.root());

    let port = start(&home, Access::Local);
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
    let (status, body) = send(port, "POST", &[], init);
    assert_eq!(status, 200, "{body}");
    assert!(
        json_of(&body)["result"]["serverInfo"]["name"]
            .as_str()
            .unwrap()
            .starts_with("docmonk"),
        "{body}"
    );

    let (status, body) = send(
        port,
        "POST",
        &[],
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    assert_eq!(status, 200);
    let names: Vec<String> = json_of(&body)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    for t in [
        "docmonk_read",
        "docmonk_references",
        "docmonk_links",
        "docmonk_evidence",
    ] {
        assert!(names.contains(&t.to_owned()), "{names:?}");
    }

    // A notification: accepted, no body.
    let (status, _) = send(
        port,
        "POST",
        &[],
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    assert_eq!(status, 202);
    // No server-initiated stream.
    assert_eq!(send(port, "GET", &[], "").0, 405);

    // Cross-site and DNS-rebinding requests are refused.
    let same = format!("http://127.0.0.1:{port}");
    assert_eq!(
        send(port, "POST", &[("origin", same.as_str())], init).0,
        200
    );
    assert_eq!(
        send(port, "POST", &[("origin", "https://evil.example")], init).0,
        403
    );
    assert_eq!(send(port, "POST", &[("host", "evil.example")], init).0, 421);

    // A network bind without remote mode never starts.
    assert!(Access::for_bind("0.0.0.0", &UiConfig::default(), &|_| None).is_err());

    // Remote mode: only an authenticated admin through the proxy.
    let ui = UiConfig {
        remote_enabled: true,
        allowed_hosts: vec![PUBLIC.into()],
        trusted_proxies: vec!["127.0.0.1".into()],
        admin_groups: vec!["ops".into()],
        viewer_users: vec!["val".into()],
        ..UiConfig::default()
    };
    let remote = start(
        &home,
        Access::for_bind("127.0.0.1", &ui, &|_| Some(SECRET.to_owned())).unwrap(),
    );
    let proxied = |user: &'static str, groups: &'static str| {
        vec![
            ("host", PUBLIC),
            ("x-docmonk-proxy-secret", SECRET),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-user", user),
            ("x-forwarded-groups", groups),
        ]
    };
    assert_eq!(send(remote, "POST", &[("host", PUBLIC)], init).0, 403);
    assert_eq!(send(remote, "POST", &proxied("val", ""), init).0, 403);
    let (status, body) = send(remote, "POST", &proxied("ada", "ops"), init);
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["id"], 1);
}
