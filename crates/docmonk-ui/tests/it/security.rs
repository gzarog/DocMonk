//! Admin UI access control over real HTTP against a bound server (audit
//! PH07, F11): remote access only through a trusted authenticating proxy,
//! role checks on privileged routes, CSRF and same-origin checks, cookie
//! flags, and unchanged local behavior.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use docmonk_config::model::UiConfig;
use docmonk_config::DocMonkConfig;
use docmonk_core::paths::Home;
use docmonk_ui::access::Access;

const SECRET: &str = "0123456789abcdef0123456789abcdef-proxy";
const PUBLIC: &str = "docmonk.example.com";

struct Ui {
    _dir: tempfile::TempDir,
    port: u16,
}

fn start(access: Access) -> Ui {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let mut cfg = DocMonkConfig::default();
    cfg.updates.enabled = false;
    docmonk_config::write_user_config(&cfg, &home).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let app = docmonk_ui::app(home, access, "127.0.0.1", port);
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
    Ui { _dir: dir, port }
}

fn remote(trusted: &str) -> Ui {
    let cfg = UiConfig {
        remote_enabled: true,
        allowed_hosts: vec![PUBLIC.into()],
        trusted_proxies: vec![trusted.into()],
        admin_groups: vec!["ops".into()],
        viewer_users: vec!["val".into()],
        ..UiConfig::default()
    };
    start(Access::for_bind("127.0.0.1", &cfg, &|_| Some(SECRET.to_owned())).unwrap())
}

struct Resp {
    status: u16,
    head: String,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Vec<String> {
        self.head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_owned())
            .collect()
    }
}

fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nconnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if method == "POST" {
        req.push_str("content-type: application/x-www-form-urlencoded\r\n");
        req.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Resp {
        status,
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

/// Headers the proxy adds for `user` (in `groups`).
fn proxied<'a>(user: Option<&'a str>, groups: &'a str) -> Vec<(&'a str, &'a str)> {
    let mut h = vec![
        ("host", PUBLIC),
        ("x-docmonk-proxy-secret", SECRET),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-groups", groups),
    ];
    if let Some(u) = user {
        h.push(("x-forwarded-user", u));
    }
    h
}

/// The CSRF token from the cookie the first page sets.
fn csrf(port: u16, headers: &[(&str, &str)], cookie: &str) -> String {
    let r = send(port, "GET", "/", headers, "");
    assert_eq!(r.status, 200, "{}", r.body);
    let set = r.header("set-cookie").join("\n");
    set.split(';')
        .next()
        .and_then(|kv| kv.strip_prefix(&format!("{cookie}=")))
        .unwrap_or_else(|| panic!("no {cookie} cookie in {set}"))
        .to_owned()
}

#[test]
fn unauthenticated_and_spoofed_remote_requests_are_denied() {
    let ui = remote("127.0.0.1");
    let p = ui.port;
    // No proxy secret: the identity headers are not trusted.
    let spoofed = [
        ("host", PUBLIC),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-user", "root"),
        ("x-forwarded-groups", "ops"),
    ];
    for (m, path) in [("GET", "/"), ("GET", "/config"), ("POST", "/backups")] {
        assert_eq!(send(p, m, path, &spoofed, "").status, 403, "{m} {path}");
    }
    let wrong = [
        ("host", PUBLIC),
        (
            "x-docmonk-proxy-secret",
            "0123456789abcdef0123456789abcdef-wrong",
        ),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-user", "root"),
        ("x-forwarded-groups", "ops"),
    ];
    assert_eq!(send(p, "GET", "/", &wrong, "").status, 403);
    // Plain HTTP at the proxy is refused.
    let mut http = proxied(Some("ann"), "ops");
    http[2] = ("x-forwarded-proto", "http");
    assert_eq!(send(p, "GET", "/", &http, "").status, 403);
    // Trusted proxy, no user: GET and POST need authentication.
    let anon = proxied(None, "");
    assert_eq!(send(p, "GET", "/", &anon, "").status, 401);
    assert_eq!(send(p, "POST", "/indexing/start", &anon, "").status, 401);
    // The proxy may probe liveness without a user, never without its secret.
    assert_eq!(send(p, "GET", "/health", &anon, "").status, 200);
    assert_eq!(
        send(p, "GET", "/health", &[("host", PUBLIC)], "").status,
        403
    );
    // A user with no role.
    assert_eq!(
        send(p, "GET", "/", &proxied(Some("eve"), "staff"), "").status,
        403
    );
    // The proxy forwarding the wrong host.
    let mut host = proxied(Some("ann"), "ops");
    host[0] = ("host", "127.0.0.1");
    assert_eq!(send(p, "GET", "/", &host, "").status, 421);
}

#[test]
fn direct_connections_cannot_spoof_the_proxy() {
    // The only trusted proxy is elsewhere: even a correct secret and
    // identity headers from this address are refused.
    let ui = remote("10.0.0.5");
    let r = send(ui.port, "GET", "/", &proxied(Some("ann"), "ops"), "");
    assert_eq!(r.status, 403);
    assert!(r.body.contains("direct connections"), "{}", r.body);
    assert_eq!(
        send(ui.port, "GET", "/health", &proxied(None, ""), "").status,
        403
    );
}

#[test]
fn viewers_read_and_admins_administer() {
    let ui = remote("127.0.0.1");
    let p = ui.port;
    let viewer = proxied(Some("val"), "");
    assert_eq!(send(p, "GET", "/", &viewer, "").status, 200);
    assert_eq!(send(p, "GET", "/sources", &viewer, "").status, 200);
    for path in [
        "/config",
        "/logs",
        "/daemon/logs",
        "/backups",
        "/backups/download/x.tar.gz",
    ] {
        assert_eq!(send(p, "GET", path, &viewer, "").status, 403, "{path}");
    }
    let token = csrf(p, &viewer, "__Host-docmonk_csrf");
    let cookie = format!("__Host-docmonk_csrf={token}");
    let mut post = viewer.clone();
    post.extend([
        ("cookie", cookie.as_str()),
        ("x-csrf-token", token.as_str()),
        ("origin", "https://docmonk.example.com"),
    ]);
    for path in ["/backups", "/indexing/rebuild", "/daemon/stop", "/config"] {
        assert_eq!(send(p, "POST", path, &post, "").status, 403, "{path}");
    }

    let admin = proxied(Some("ann"), "dev, ops");
    assert_eq!(send(p, "GET", "/config", &admin, "").status, 200);
    assert_eq!(send(p, "GET", "/backups", &admin, "").status, 200);
}

#[test]
fn cookies_csrf_and_origin_are_enforced_remotely() {
    let ui = remote("127.0.0.1");
    let p = ui.port;
    let admin = proxied(Some("ann"), "ops");
    let first = send(p, "GET", "/", &admin, "");
    let set = first.header("set-cookie").join("\n");
    for flag in [
        "__Host-docmonk_csrf=",
        "Secure",
        "HttpOnly",
        "SameSite=Strict",
        "Max-Age=",
        "Path=/",
    ] {
        assert!(set.contains(flag), "{flag}: {set}");
    }
    assert_eq!(first.header("x-frame-options"), ["DENY"]);
    assert_eq!(first.header("strict-transport-security").len(), 1);
    let token = csrf(p, &admin, "__Host-docmonk_csrf");
    let cookie = format!("__Host-docmonk_csrf={token}");
    let with = |extra: &[(&'static str, &'static str)]| {
        let mut h: Vec<(&str, &str)> = admin.clone();
        h.push(("cookie", cookie.as_str()));
        h.extend_from_slice(extra);
        h
    };
    let path = "/backups/restore";
    let body = "name=..%2Fconfig.yaml&confirm=yes";
    // Cross-site POST: foreign origin, or no origin at all.
    let mut h = with(&[("origin", "https://evil.example")]);
    h.push(("x-csrf-token", &token));
    assert_eq!(send(p, "POST", path, &h, body).status, 403);
    let mut h = with(&[]);
    h.push(("x-csrf-token", &token));
    assert_eq!(send(p, "POST", path, &h, body).status, 403);
    // Same origin, but no token, a wrong one, or one in the URL only.
    let same = with(&[("origin", "https://docmonk.example.com")]);
    assert_eq!(send(p, "POST", path, &same, body).status, 403);
    let mut h = same.clone();
    h.push(("x-csrf-token", "replayed-or-guessed"));
    assert_eq!(send(p, "POST", path, &h, body).status, 403);
    let q = format!("{path}?csrf_token={token}");
    assert_eq!(send(p, "POST", &q, &same, body).status, 403);
    // A valid request; path traversal still finds nothing.
    let mut ok = same.clone();
    ok.push(("x-csrf-token", &token));
    let r = send(p, "POST", path, &ok, body);
    assert_eq!(r.status, 204);
    assert_eq!(
        r.header("hx-redirect"),
        ["/backups?error=no such backup: ../config.yaml"]
    );
    // Referer stands in for Origin.
    let mut h = with(&[("referer", "https://docmonk.example.com/backups")]);
    h.push(("x-csrf-token", &token));
    assert_eq!(send(p, "POST", path, &h, body).status, 204);
    // Destructive actions need explicit confirmation.
    let r = send(p, "POST", path, &ok, "name=x.tar.gz");
    assert_eq!(
        r.header("hx-redirect"),
        ["/backups?error=restore+not+confirmed"]
    );
    let r = send(p, "POST", "/indexing/rebuild", &ok, "fresh=1");
    assert_eq!(
        r.header("hx-redirect"),
        ["/indexing?error=rebuild+not+confirmed"]
    );
    let r = send(p, "POST", "/sources/src_x/remove", &ok, "");
    assert_eq!(
        r.header("hx-redirect"),
        ["/sources?error=removal+not+confirmed"]
    );
}

#[test]
fn local_loopback_stays_usable_without_an_identity_provider() {
    let ui = start(Access::for_bind("127.0.0.1", &UiConfig::default(), &|_| None).unwrap());
    let p = ui.port;
    let host = format!("127.0.0.1:{p}");
    let base = [("host", host.as_str())];
    let r = send(p, "GET", "/", &base, "");
    assert_eq!(r.status, 200);
    let set = r.header("set-cookie").join("\n");
    assert!(
        set.contains("docmonk_csrf=") && set.contains("HttpOnly"),
        "{set}"
    );
    assert!(!set.contains("Secure"), "{set}");
    let token = csrf(p, &base, "docmonk_csrf");
    let cookie = format!("docmonk_csrf={token}");
    let mut h = vec![
        ("host", host.as_str()),
        ("cookie", cookie.as_str()),
        ("x-csrf-token", token.as_str()),
    ];
    // Non-browser clients (no Origin) keep working locally.
    let r = send(p, "POST", "/sources/src_nope/enable", &h, "");
    assert_eq!(r.status, 204);
    // A page on another site cannot drive the local UI.
    h.push(("origin", "http://evil.example"));
    assert_eq!(
        send(p, "POST", "/sources/src_nope/enable", &h, "").status,
        403
    );
    // DNS rebinding.
    assert_eq!(
        send(p, "GET", "/", &[("host", "evil.example")], "").status,
        421
    );
}
