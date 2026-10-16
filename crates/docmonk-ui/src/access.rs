//! Who may use the Admin UI.
//!
//! * **Local** (the default): bound to a loopback address only, no
//!   identity provider; every request acts as the local administrator.
//! * **Remote** (`ui.remote_enabled`): identity comes from an
//!   authenticating reverse proxy that terminates TLS. A request is
//!   accepted only from a configured proxy address, carrying the proxy's
//!   shared secret and `X-Forwarded-Proto: https`; the user and groups
//!   headers then map to the `admin` or `viewer` role.
//!
//! A non-loopback bind without a complete remote configuration is refused
//! at startup: the UI never serves privileged routes unauthenticated.

use std::net::IpAddr;

use axum::http::{HeaderMap, Method};
use docmonk_config::model::UiConfig;
use docmonk_core::errors::DocMonkError;

/// The header carrying the proxy's shared secret.
pub const PROXY_SECRET_HEADER: &str = "x-docmonk-proxy-secret";
/// Shortest accepted shared secret.
pub const MIN_SECRET_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Admin => "admin",
        }
    }
}

/// The authenticated caller of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub user: String,
    pub role: Role,
}

#[derive(Debug, Clone)]
pub struct Remote {
    pub allowed_hosts: Vec<String>,
    pub trusted_proxies: Vec<IpAddr>,
    pub secret: String,
    pub user_header: String,
    pub groups_header: String,
    pub admin_users: Vec<String>,
    pub admin_groups: Vec<String>,
    pub viewer_users: Vec<String>,
    pub viewer_groups: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Access {
    Local,
    Remote(Box<Remote>),
}

/// Whether `host` (as given to `--host`) is a loopback address.
pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn trimmed(v: &[String]) -> Vec<String> {
    v.iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

impl Access {
    /// The access mode for binding `host`, or why the UI must not start.
    pub fn for_bind(
        host: &str,
        cfg: &UiConfig,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, DocMonkError> {
        if !cfg.remote_enabled {
            if is_loopback(host) {
                return Ok(Access::Local);
            }
            return Err(DocMonkError::config(format!(
                "refusing to serve the Admin UI on non-loopback address {host}: it would be \
                 reachable from the network without authentication. Bind to 127.0.0.1, or put \
                 an authenticating TLS reverse proxy in front and set ui.remote_enabled (see \
                 docs/ui.md)"
            )));
        }
        let mut missing = Vec::new();
        let allowed_hosts = trimmed(&cfg.allowed_hosts);
        if allowed_hosts.is_empty() {
            missing.push("ui.allowed_hosts (the public host names the proxy serves)".to_owned());
        }
        let trusted_proxies: Vec<IpAddr> = trimmed(&cfg.trusted_proxies)
            .iter()
            .filter_map(|p| p.parse().ok())
            .collect();
        if trusted_proxies.is_empty() {
            missing.push("ui.trusted_proxies (the proxy IP addresses)".to_owned());
        }
        let secret = env(&cfg.proxy_secret_env).unwrap_or_default();
        if secret.len() < MIN_SECRET_LEN {
            missing.push(format!(
                "environment variable {} (the proxy's shared secret, at least {MIN_SECRET_LEN} characters)",
                cfg.proxy_secret_env
            ));
        }
        let (admin_users, admin_groups) = (trimmed(&cfg.admin_users), trimmed(&cfg.admin_groups));
        if admin_users.is_empty() && admin_groups.is_empty() {
            missing.push("ui.admin_users or ui.admin_groups".to_owned());
        }
        let header = |h: &str| h.trim().to_ascii_lowercase();
        let (user_header, groups_header) = (header(&cfg.user_header), header(&cfg.groups_header));
        if user_header.is_empty() || axum::http::HeaderName::try_from(&user_header).is_err() {
            missing.push("ui.user_header (a valid header name)".to_owned());
        }
        if missing.is_empty() {
            return Ok(Access::Remote(Box::new(Remote {
                allowed_hosts,
                trusted_proxies,
                secret,
                user_header,
                groups_header,
                admin_users,
                admin_groups,
                viewer_users: trimmed(&cfg.viewer_users),
                viewer_groups: trimmed(&cfg.viewer_groups),
            })));
        }
        Err(DocMonkError::config(format!(
            "ui.remote_enabled is set but remote access is not fully configured; refusing to \
             start. Missing: {}",
            missing.join("; ")
        )))
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Access::Remote(_))
    }
}

/// Routes only an admin may use: every state change, plus the pages that
/// expose configuration, logs or backup archives.
pub fn is_privileged(method: &Method, path: &str) -> bool {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return true;
    }
    ["/config", "/logs", "/daemon/logs", "/backups"]
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

/// Routes the proxy itself may probe without a user.
pub fn is_probe(path: &str) -> bool {
    matches!(path, "/health" | "/ready")
}

/// Why a remote request is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    /// Not from a trusted proxy, or without its secret or TLS: the
    /// identity headers cannot be trusted.
    Untrusted(&'static str),
    /// Trusted proxy, but no user identity.
    Unauthenticated,
    /// Authenticated, without the needed role.
    Forbidden,
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

impl Remote {
    /// Authenticates and authorizes one request (`None` identity for an
    /// authorized proxy probe).
    pub fn check(
        &self,
        peer: Option<IpAddr>,
        headers: &HeaderMap,
        method: &Method,
        path: &str,
    ) -> Result<Option<Identity>, Denied> {
        let peer = peer.map(|ip| match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        });
        if !peer.is_some_and(|ip| self.trusted_proxies.contains(&ip)) {
            return Err(Denied::Untrusted("direct connections are not accepted"));
        }
        if !header(headers, PROXY_SECRET_HEADER).is_some_and(|s| constant_eq(s, &self.secret)) {
            return Err(Denied::Untrusted("proxy secret missing or wrong"));
        }
        if !header(headers, "x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https")) {
            return Err(Denied::Untrusted("TLS is required at the proxy"));
        }
        let Some(user) = header(headers, &self.user_header) else {
            return if is_probe(path) {
                Ok(None)
            } else {
                Err(Denied::Unauthenticated)
            };
        };
        let groups: Vec<&str> = header(headers, &self.groups_header)
            .map(|g| {
                g.split(',')
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let member = |users: &[String], wanted: &[String]| {
            users.iter().any(|u| u == user) || groups.iter().any(|g| wanted.iter().any(|w| w == g))
        };
        let role = if member(&self.admin_users, &self.admin_groups) {
            Role::Admin
        } else if member(&self.viewer_users, &self.viewer_groups) {
            Role::Viewer
        } else {
            return Err(Denied::Forbidden);
        };
        if role < Role::Admin && is_privileged(method, path) {
            return Err(Denied::Forbidden);
        }
        Ok(Some(Identity {
            user: user.to_owned(),
            role,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> UiConfig {
        UiConfig {
            remote_enabled: true,
            allowed_hosts: vec!["docmonk.example.com".into()],
            trusted_proxies: vec!["10.0.0.5".into()],
            admin_groups: vec!["ops".into()],
            viewer_users: vec!["val".into()],
            ..UiConfig::default()
        }
    }

    fn secret(k: &str) -> Option<String> {
        (k == "DOCMONK_UI_PROXY_SECRET").then(|| "s".repeat(40))
    }

    #[test]
    fn loopback_detection() {
        for h in ["127.0.0.1", "localhost", "::1", "[::1]", "127.0.0.2"] {
            assert!(is_loopback(h), "{h}");
        }
        for h in ["0.0.0.0", "::", "192.168.1.4", "example.com"] {
            assert!(!is_loopback(h), "{h}");
        }
    }

    #[test]
    fn remote_bind_fails_closed_until_fully_configured() {
        let none = |_: &str| None;
        let local = UiConfig::default();
        assert!(matches!(
            Access::for_bind("127.0.0.1", &local, &none),
            Ok(Access::Local)
        ));
        for host in ["0.0.0.0", "::", "192.168.1.4"] {
            let e = Access::for_bind(host, &local, &none).unwrap_err();
            assert!(e.message().contains("refusing"), "{}", e.message());
        }
        // Enabled but no secret in the environment.
        let e = Access::for_bind("0.0.0.0", &cfg(), &none).unwrap_err();
        assert!(e.message().contains("DOCMONK_UI_PROXY_SECRET"), "{e:?}");
        // A short secret is refused too.
        let short = |_: &str| Some("short".to_owned());
        assert!(Access::for_bind("0.0.0.0", &cfg(), &short).is_err());
        // No admin mapping.
        let mut c = cfg();
        c.admin_groups.clear();
        assert!(Access::for_bind("0.0.0.0", &c, &secret).is_err());
        assert!(Access::for_bind("0.0.0.0", &cfg(), &secret)
            .unwrap()
            .is_remote());
        // Remote mode also applies behind a proxy on the same host.
        assert!(Access::for_bind("127.0.0.1", &cfg(), &secret)
            .unwrap()
            .is_remote());
    }
}
