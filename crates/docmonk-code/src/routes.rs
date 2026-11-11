//! HTTP routes registered by a call (`app.get("/x", handler)`), read
//! statically from the source text. Always HEURISTIC, like the decorator
//! and attribute routes of [`crate::framework`] and the message sites of
//! [`crate::messaging`]: a route is recorded when its path is a string
//! literal and the call is made on a receiver this file shows to be an
//! application or router. Paths built at runtime, routers imported from
//! another file and prefixes applied elsewhere are not seen.
//!
//! Rules (each only in files that mention the framework, which keeps
//! look-alike calls of other APIs, such as an HTTP client's `get`, out):
//!
//! * Express (JavaScript / TypeScript): `app.get|post|put|patch|delete|
//!   head|options|all(path, ..., handler)` on a receiver created by
//!   `express()` or `Router()` (or named `app` / `router`), and
//!   `app.use(prefix, router)` mounting a router of the same file.
//! * Fastify (JavaScript / TypeScript): the same shorthand methods on a
//!   receiver created by `fastify()` (or named `fastify` / `app` /
//!   `server`), `route({ method, url, handler })`, and
//!   `register(plugin, { prefix })` for a plugin function of the same
//!   file.
//!
//! The route's subject is the handler when it is named and defined in
//! the same file, else the function around the call, else the file.

use std::collections::HashMap;
use std::sync::OnceLock;

use docmonk_core::models::EntityType;
use regex::Regex;

use crate::extract::ExtractedEntity;

/// Route sites kept per file (bounds graph rows on generated code).
pub const MAX_ROUTE_SITES_PER_FILE: usize = 512;

/// One route registered by a call.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteSite {
    pub line: i64,
    /// The handler, the function around the call, or `None` (the file).
    pub subject_local_id: Option<usize>,
    /// `GET`, `POST`, ... or `ANY`.
    pub method: String,
    /// The full path as written, prefixes joined (`/api/users/:id`).
    pub path: String,
    /// [`docmonk_core::urls::url_match_key`] of the path, parameters of
    /// every style written `{}`.
    pub key: String,
    pub resolver: &'static str,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("regex"))
}

macro_rules! rule {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            re(&R, $pattern)
        }
    };
}

/// A JavaScript string literal without interpolation.
const JS_STR: &str = r#"(?:'([^'\\\n]*)'|"([^"\\\n]*)"|`([^`$\\\n]*)`)"#;

rule!(
    js_receiver,
    r#"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=\n]+)?=\s*(?:await\s+)?(?:(express)\s*\(|(?:express\s*\.\s*)?(Router)\s*\(|(?:require\s*\(\s*['"]fastify['"]\s*\)|[Ff]astify)\s*\()"#
);

fn js_verb() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(&format!(
            r#"\b([A-Za-z_$][\w$]*)\s*\.\s*(get|post|put|patch|delete|head|options|all)\s*(?:<[^>()]*>)?\s*\(\s*{JS_STR}\s*,"#
        ))
        .expect("regex")
    })
}

fn js_use() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(&format!(
            r#"\b([A-Za-z_$][\w$]*)\s*\.\s*use\s*\(\s*{JS_STR}\s*,\s*([A-Za-z_$][\w$]*)\s*\)"#
        ))
        .expect("regex")
    })
}

rule!(
    fastify_route,
    r#"\b([A-Za-z_$][\w$]*)\s*\.\s*route\s*(?:<[^>()]*>)?\s*\(\s*\{"#
);

fn fastify_register() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(&format!(
            r#"\b[A-Za-z_$][\w$]*\s*\.\s*register\s*\(\s*([A-Za-z_$][\w$]*)\s*,\s*\{{[^{{}}]*?\bprefix\s*:\s*{JS_STR}"#
        ))
        .expect("regex")
    })
}

fn js_property(name: &str) -> Regex {
    Regex::new(&format!(r#"\b{name}\s*:\s*{JS_STR}"#)).expect("regex")
}

rule!(js_method_list, r#"\bmethod\s*:\s*\[([^\]]*)\]"#);
rule!(
    js_handler_property,
    r#"\bhandler\s*:\s*([A-Za-z_$][\w$.]*)\s*[,}\n]"#
);
rule!(js_quoted, r#"['"`]([A-Za-z]+)['"`]"#);

/// The first non-empty capture among `groups` (the quote alternatives).
fn text_of<'t>(c: &regex::Captures<'t>, groups: std::ops::Range<usize>) -> Option<&'t str> {
    groups.filter_map(|i| c.get(i)).map(|m| m.as_str()).next()
}

/// 1-based line of byte offset `at`.
fn line_of(text: &str, at: usize) -> i64 {
    text.as_bytes()[..at]
        .iter()
        .filter(|b| **b == b'\n')
        .count() as i64
        + 1
}

/// The innermost function, method or type spanning `line`.
fn owner(entities: &[ExtractedEntity], line: i64) -> Option<&ExtractedEntity> {
    entities
        .iter()
        .filter(|e| e.kind != EntityType::Namespace && e.start_line <= line && line <= e.end_line)
        .min_by_key(|e| (e.end_line - e.start_line, e.local_id))
}

/// The function or method named `name` (its last dotted segment).
fn named(entities: &[ExtractedEntity], name: &str) -> Option<usize> {
    let name = name.rsplit('.').next()?;
    entities
        .iter()
        .find(|e| matches!(e.kind, EntityType::Function | EntityType::Method) && e.name == name)
        .map(|e| e.local_id)
}

/// The text between the `open` bracket at byte `at` and its match
/// (strings and nested brackets skipped), or `None` when unbalanced.
fn balanced(text: &str, at: usize, open: u8, close: u8) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(at) != Some(&open) {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = at;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(_) if b == b'\\' => i += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if matches!(b, b'"' | b'\'' | b'`') => quote = Some(b),
            None if b == open => depth += 1,
            None if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[at + 1..i]);
                }
            }
            None => {}
        }
        i += 1;
    }
    None
}

/// The last top-level argument of an argument list, trimmed.
fn last_argument(args: &str) -> &str {
    let bytes = args.as_bytes();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(_) if b == b'\\' => i += 1,
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if matches!(b, b'"' | b'\'' | b'`') => quote = Some(b),
            None if matches!(b, b'(' | b'[' | b'{') => depth += 1,
            None if matches!(b, b')' | b']' | b'}') => depth -= 1,
            None if b == b',' && depth == 0 => start = i + 1,
            None => {}
        }
        i += 1;
    }
    args[start..].trim()
}

fn is_identifier_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('.').all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
                && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        })
}

/// `prefix` and `path` joined with exactly one `/` between them.
pub(crate) fn join(prefix: &str, path: &str) -> String {
    match (prefix.trim_end_matches('/'), path.trim_start_matches('/')) {
        ("", p) => format!("/{p}"),
        (pre, "") => pre.to_owned(),
        (pre, p) => format!("{pre}/{p}"),
    }
}

/// The exact-match key of a route path: `:name` and `*name` segments
/// (Express, Fastify, gin, echo) are read as parameters, like `{name}`.
pub(crate) fn route_key(path: &str) -> Option<String> {
    let braced: Vec<String> = path
        .split('/')
        .map(|segment| {
            let name = segment.trim_end_matches('?');
            match name.strip_prefix(':').or_else(|| name.strip_prefix('*')) {
                Some(rest) if segment.starts_with('*') || !rest.is_empty() => {
                    format!("{{{}}}", rest.split(['(', '.', '-']).next().unwrap_or(rest))
                }
                _ => segment.to_owned(),
            }
        })
        .collect();
    docmonk_core::urls::url_match_key(&braced.join("/"))
}

struct Sites<'a> {
    text: &'a str,
    entities: &'a [ExtractedEntity],
    out: Vec<RouteSite>,
}

impl Sites<'_> {
    /// Records one route; `handler` is the handler expression, if any.
    fn push(
        &mut self,
        at: usize,
        method: &str,
        path: &str,
        handler: Option<&str>,
        resolver: &'static str,
    ) {
        let path = docmonk_core::urls::url_path(path);
        let path = if path.is_empty() {
            "/".to_owned()
        } else {
            path
        };
        let Some(key) = route_key(&path) else {
            return;
        };
        let line = line_of(self.text, at);
        let subject_local_id = handler
            .filter(|h| is_identifier_path(h))
            .and_then(|h| named(self.entities, h))
            .or_else(|| owner(self.entities, line).map(|e| e.local_id));
        let site = RouteSite {
            line,
            subject_local_id,
            method: method.to_ascii_uppercase(),
            path,
            key,
            resolver,
        };
        if !self.out.contains(&site) {
            self.out.push(site);
        }
    }
}

/// Which framework a receiver variable belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Js {
    Express,
    Fastify,
}

fn javascript(text: &str, sites: &mut Sites<'_>) {
    let entities = sites.entities;
    let express = text.contains("express");
    let fastify = text.contains("fastify") || text.contains("Fastify");
    if !express && !fastify {
        return;
    }
    // Receivers: variables this file creates, and the conventional names
    // of the framework the file mentions.
    let mut receivers: HashMap<&str, Js> = HashMap::new();
    if express {
        receivers.insert("app", Js::Express);
        receivers.insert("router", Js::Express);
    }
    if fastify {
        for name in ["fastify", "server"] {
            receivers.insert(name, Js::Fastify);
        }
        if !express {
            receivers.insert("app", Js::Fastify);
        }
    }
    for c in js_receiver().captures_iter(text) {
        let Some(name) = c.get(1).map(|m| m.as_str()) else {
            continue;
        };
        let kind = if c.get(2).is_some() || c.get(3).is_some() {
            Js::Express
        } else {
            Js::Fastify
        };
        if (kind == Js::Express && express) || (kind == Js::Fastify && fastify) {
            receivers.insert(name, kind);
        }
    }
    // Express: `parent.use('/prefix', child)`, followed through nested
    // mounts (a router mounted on a mounted router).
    let mut mounts: HashMap<&str, (&str, &str)> = HashMap::new();
    for c in js_use().captures_iter(text) {
        let (Some(parent), Some(prefix), Some(child)) = (
            c.get(1).map(|m| m.as_str()),
            text_of(&c, 2..5),
            c.get(5).map(|m| m.as_str()),
        ) else {
            continue;
        };
        if receivers.get(parent) == Some(&Js::Express)
            && receivers.get(child) == Some(&Js::Express)
            && parent != child
        {
            mounts.entry(child).or_insert((parent, prefix));
        }
    }
    let express_prefix = |receiver: &str| -> String {
        let mut prefix = String::new();
        let mut at = receiver;
        for _ in 0..8 {
            let Some((parent, p)) = mounts.get(at) else {
                break;
            };
            prefix = join(p, &prefix);
            at = parent;
        }
        prefix
    };
    // Fastify: `register(plugin, { prefix })` applies to the routes
    // registered inside the function `plugin`.
    let mut plugin_prefix: HashMap<&str, &str> = HashMap::new();
    if fastify {
        for c in fastify_register().captures_iter(text) {
            if let (Some(plugin), Some(prefix)) = (c.get(1).map(|m| m.as_str()), text_of(&c, 2..5))
            {
                plugin_prefix.entry(plugin).or_insert(prefix);
            }
        }
    }
    let prefix_of = |receiver: &str, kind: Js, line: i64| -> String {
        match kind {
            Js::Express => express_prefix(receiver),
            Js::Fastify => functions_around(entities, line)
                .into_iter()
                .find_map(|name| plugin_prefix.get(name).map(|p| (*p).to_owned()))
                .unwrap_or_default(),
        }
    };
    let mut found: Vec<(usize, String, String, Option<String>, &'static str)> = Vec::new();
    for c in js_verb().captures_iter(text) {
        let (Some(m), Some(receiver), Some(verb), Some(path)) = (
            c.get(0),
            c.get(1).map(|m| m.as_str()),
            c.get(2).map(|m| m.as_str()),
            text_of(&c, 3..6),
        ) else {
            continue;
        };
        let Some(kind) = receivers.get(receiver).copied() else {
            continue;
        };
        // The handler is the last argument of the call.
        let open = m.start() + m.as_str().find('(').unwrap_or(0);
        let handler = balanced(text, open, b'(', b')').map(last_argument);
        let method = if verb == "all" { "ANY" } else { verb };
        let line = line_of(text, m.start());
        let full = join(&prefix_of(receiver, kind, line), path);
        let resolver = match kind {
            Js::Express => "framework:express_route",
            Js::Fastify => "framework:fastify_route",
        };
        found.push((
            m.start(),
            method.to_owned(),
            full,
            handler.map(str::to_owned),
            resolver,
        ));
    }
    if fastify {
        for c in fastify_route().captures_iter(text) {
            let (Some(m), Some(receiver)) = (c.get(0), c.get(1).map(|m| m.as_str())) else {
                continue;
            };
            if receivers.get(receiver) != Some(&Js::Fastify) {
                continue;
            }
            let Some(body) = balanced(text, m.end() - 1, b'{', b'}') else {
                continue;
            };
            let Some(path) = js_property("url")
                .captures(body)
                .or_else(|| js_property("path").captures(body))
                .and_then(|c| text_of(&c, 1..4).map(str::to_owned))
            else {
                continue;
            };
            let mut methods: Vec<String> = match js_method_list().captures(body) {
                Some(list) => js_quoted()
                    .captures_iter(&list[1])
                    .map(|q| q[1].to_owned())
                    .collect(),
                None => js_property("method")
                    .captures(body)
                    .and_then(|c| text_of(&c, 1..4).map(str::to_owned))
                    .into_iter()
                    .collect(),
            };
            if methods.is_empty() {
                methods.push("ANY".into());
            }
            let handler = js_handler_property()
                .captures(body)
                .map(|c| c[1].to_owned());
            let line = line_of(text, m.start());
            let full = join(&prefix_of(receiver, Js::Fastify, line), &path);
            for method in methods {
                found.push((
                    m.start(),
                    method,
                    full.clone(),
                    handler.clone(),
                    "framework:fastify_route",
                ));
            }
        }
    }
    for (at, method, path, handler, resolver) in found {
        sites.push(at, &method, &path, handler.as_deref(), resolver);
    }
}

/// Names of the functions around `line`, innermost first.
fn functions_around(entities: &[ExtractedEntity], line: i64) -> Vec<&str> {
    let mut around: Vec<&ExtractedEntity> = entities
        .iter()
        .filter(|e| {
            matches!(e.kind, EntityType::Function | EntityType::Method)
                && e.start_line <= line
                && line <= e.end_line
        })
        .collect();
    around.sort_by_key(|e| (e.end_line - e.start_line, e.local_id));
    around.into_iter().map(|e| e.name.as_str()).collect()
}

/// Every route a call registers in one file, in a stable order (line,
/// method, path).
pub fn detect(language: &str, text: &str, entities: &[ExtractedEntity]) -> Vec<RouteSite> {
    let mut sites = Sites {
        text,
        entities,
        out: Vec::new(),
    };
    match language {
        "javascript" | "typescript" => javascript(text, &mut sites),
        _ => {}
    }
    let mut out = sites.out;
    out.sort();
    out.truncate(MAX_ROUTE_SITES_PER_FILE);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/endpoints/src")
                .join(rel),
        )
        .unwrap()
    }

    /// `(method, path, key, subject name)` of every route, in file order.
    fn routes(language: &str, text: &str) -> Vec<(String, String, String, String)> {
        let x = crate::extract::extract(text.as_bytes(), language, "f", "f").unwrap();
        detect(language, text, &x.entities)
            .into_iter()
            .map(|r| {
                let subject = r
                    .subject_local_id
                    .map_or("<file>".to_owned(), |i| x.entities[i].name.clone());
                (r.method, r.path, r.key, subject)
            })
            .collect()
    }

    fn row(method: &str, path: &str, key: &str, subject: &str) -> (String, String, String, String) {
        (method.into(), path.into(), key.into(), subject.into())
    }

    #[test]
    fn express_routes_with_mounted_routers() {
        assert_eq!(
            routes("typescript", &fixture("web/server.ts")),
            [
                row("GET", "/health/live", "health/live", "<file>"),
                row("GET", "/api/web/orders", "api/web/orders", "listOrders"),
                row(
                    "GET",
                    "/api/web/orders/:orderId",
                    "api/web/orders/{}",
                    "getOrder"
                ),
                row("POST", "/api/web/orders", "api/web/orders", "createOrder"),
                row(
                    "DELETE",
                    "/api/web/orders/admin/orders",
                    "api/web/orders/admin/orders",
                    "purgeOrders"
                ),
                row(
                    "ANY",
                    "/api/web/orders/admin/ping",
                    "api/web/orders/admin/ping",
                    "<file>"
                ),
            ]
        );
    }

    #[test]
    fn fastify_routes_with_a_prefixed_plugin() {
        assert_eq!(
            routes("typescript", &fixture("web/plugin.ts")),
            [
                row(
                    "GET",
                    "/api/web/invoices/:invoiceId",
                    "api/web/invoices/{}",
                    "getInvoice"
                ),
                row(
                    "PATCH",
                    "/api/web/invoices/:invoiceId",
                    "api/web/invoices/{}",
                    "replaceInvoice"
                ),
                row(
                    "PUT",
                    "/api/web/invoices/:invoiceId",
                    "api/web/invoices/{}",
                    "replaceInvoice"
                ),
                row(
                    "POST",
                    "/api/web/invoices",
                    "api/web/invoices",
                    "invoiceRoutes"
                ),
                row("GET", "/ready", "ready", "<file>"),
            ]
        );
    }

    #[test]
    fn calls_that_only_look_like_routes_are_not_routes() {
        // No framework in the file: nothing, whatever the receiver is named.
        let plain = "const app = make();\napp.get('/api/x/y', handler);\n";
        assert_eq!(routes("javascript", plain), []);
        // With the framework: only its receivers, only with a handler.
        let text = "const express = require('express');\n\
                    const app = express();\n\
                    const http = require('axios');\n\
                    http.get('/api/remote/items', { timeout: 5 });\n\
                    store.delete('/api/cache/items', now);\n\
                    app.get('/api/settings/theme');\n\
                    const where = '/api/not/a/route';\n\
                    app.get(`/api/${version}/items`, list);\n\
                    app.get('/api/items', list);\n";
        assert_eq!(
            routes("javascript", text),
            [row("GET", "/api/items", "api/items", "<file>")]
        );
        // Other languages have their own rules.
        assert_eq!(routes("python", "app.get('/api/items', list)\n"), []);
    }

    #[test]
    fn parameters_of_every_style_share_a_key() {
        for path in [
            "/users/:id/files/*rest",
            "/users/{id}/files/{rest}",
            "/users/:id?/files/*",
        ] {
            assert_eq!(
                route_key(path).as_deref(),
                Some("users/{}/files/{}"),
                "{path}"
            );
        }
        // A colon inside a segment is not a parameter.
        assert_eq!(route_key("/a:b/c").as_deref(), Some("a:b/c"));
        assert_eq!(join("/api/", "/x"), "/api/x");
        assert_eq!(join("", "x"), "/x");
        assert_eq!(join("/api", "/"), "/api");
    }
}
