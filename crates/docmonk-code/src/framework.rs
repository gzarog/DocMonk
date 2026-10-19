//! Framework heuristics: Flask/FastAPI
//! route decorators and ASP.NET route attributes. Always HEURISTIC.
//!
//! [`compose_routes`] additionally joins an ASP.NET controller's
//! `[Route]` prefix with each action's `[Route]`/`[Http*]` template into the
//! full static path (`route:<key>` findings). Composition is a static
//! reading of the attributes, never proof of runtime routing: conventional
//! routes, `MapControllerRoute` templates and middleware are not seen.

use std::sync::OnceLock;

use docmonk_core::models::EntityType;
use regex::Regex;

use crate::extract::{ExtractedDecorator, ExtractedEntity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkFinding {
    pub subject_local_id: usize,
    pub target_symbol: String,
    pub resolver: &'static str,
    pub evidence: String,
}

fn py_route() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        // Anchored like re.match; the backreference to the opening quote
        // is expanded into the two quote alternatives.
        Regex::new(
            r#"(?s)^@\s*[\w.]*\.(?P<verb>route|get|post|put|patch|delete|head|options)\s*\(\s*(?:'(?P<p1>[^'"]+)'|"(?P<p2>[^'"]+)")(?P<rest>.*)"#,
        )
        .expect("regex")
    })
}

fn py_methods() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"methods\s*=\s*\[\s*['"](?P<method>\w+)['"]"#).expect("regex"))
}

fn cs_route() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r#"^(?P<name>Http(?P<verb>Get|Post|Put|Patch|Delete)|Route)\s*(?:\(\s*(?:'(?P<p1>[^'"]*)'|"(?P<p2>[^'"]*)")\s*\))?"#,
        )
        .expect("regex")
    })
}

pub fn detect_python_route_decorators(decorators: &[ExtractedDecorator]) -> Vec<FrameworkFinding> {
    let mut out = Vec::new();
    for dec in decorators {
        let Some(m) = py_route().captures(&dec.text) else {
            continue;
        };
        let verb = &m["verb"];
        let path = m
            .name("p1")
            .or_else(|| m.name("p2"))
            .map_or("", |p| p.as_str());
        let method = if verb == "route" {
            py_methods()
                .captures(&m["rest"])
                .map_or_else(|| "GET".to_owned(), |mm| mm["method"].to_uppercase())
        } else {
            verb.to_uppercase()
        };
        out.push(FrameworkFinding {
            subject_local_id: dec.subject_local_id,
            target_symbol: format!("http_endpoint:{method}:{path}"),
            resolver: "framework:python_route_decorator",
            evidence: dec.text.clone(),
        });
    }
    out
}

pub fn detect_csharp_route_attributes(decorators: &[ExtractedDecorator]) -> Vec<FrameworkFinding> {
    let mut out = Vec::new();
    for dec in decorators {
        let Some(m) = cs_route().captures(&dec.text) else {
            continue;
        };
        let path = m
            .name("p1")
            .or_else(|| m.name("p2"))
            .map_or("", |p| p.as_str());
        let method = m
            .name("verb")
            .map_or_else(|| "ANY".to_owned(), |v| v.as_str().to_uppercase());
        out.push(FrameworkFinding {
            subject_local_id: dec.subject_local_id,
            target_symbol: format!("http_endpoint:{method}:{path}"),
            resolver: "framework:csharp_route_attribute",
            evidence: dec.text.clone(),
        });
    }
    out
}

/// One statically composed route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedRoute {
    pub subject_local_id: usize,
    /// `GET`, `POST`, ... or `ANY`.
    pub method: String,
    /// The composed template as written (`/api/content/groups/{id}`).
    pub path: String,
    /// [`docmonk_core::urls::url_match_key`] of `path`.
    pub key: String,
    pub resolver: &'static str,
    pub line: i64,
}

/// `(verb, template)` of one route attribute or decorator; `None` for
/// any other decorator.
fn route_template(language: &str, dec: &ExtractedDecorator) -> Option<(String, Option<String>)> {
    match language {
        "csharp" => cs_route().captures(&dec.text).map(|m| {
            let path = m
                .name("p1")
                .or_else(|| m.name("p2"))
                .map(|p| p.as_str().to_owned());
            let verb = m
                .name("verb")
                .map_or_else(|| "ANY".to_owned(), |v| v.as_str().to_uppercase());
            (verb, path)
        }),
        "python" => detect_python_route_decorators(std::slice::from_ref(dec))
            .pop()
            .and_then(|f| {
                let (method, path) = f
                    .target_symbol
                    .strip_prefix("http_endpoint:")?
                    .split_once(':')?;
                Some((method.to_owned(), Some(path.to_owned())))
            }),
        _ => None,
    }
}

/// ASP.NET token replacement: `[controller]` (class name without its
/// `Controller` suffix) and `[action]` (method name).
fn replace_tokens(template: &str, class: Option<&str>, action: &str) -> String {
    let controller = class.map(|c| c.strip_suffix("Controller").unwrap_or(c));
    let mut t = template.to_owned();
    if let Some(c) = controller {
        t = t.replace("[controller]", c);
    }
    t.replace("[action]", action)
}

/// Statically composed routes of every routed method (see the module
/// docs): class-level `[Route]` prefixes x action templates; an action
/// template starting with `/` or `~/` ignores the prefix. Python
/// decorators compose with no prefix (blueprint prefixes are not seen).
pub fn compose_routes(
    language: &str,
    entities: &[ExtractedEntity],
    decorators: &[ExtractedDecorator],
) -> Vec<ComposedRoute> {
    if !matches!(language, "csharp" | "python") {
        return Vec::new();
    }
    let resolver = if language == "csharp" {
        "framework:csharp_composed_route"
    } else {
        "framework:python_composed_route"
    };
    let mut out = Vec::new();
    for e in entities {
        if !matches!(e.kind, EntityType::Method | EntityType::Function) {
            continue;
        }
        let own: Vec<(&ExtractedDecorator, (String, Option<String>))> = decorators
            .iter()
            .filter(|d| d.subject_local_id == e.local_id)
            .filter_map(|d| route_template(language, d).map(|t| (d, t)))
            .collect();
        if own.is_empty() {
            continue;
        }
        let class = e.parent_local_id.and_then(|p| entities.get(p));
        let class_name = class.map(|c| c.name.as_str());
        let prefixes: Vec<String> = match class {
            Some(c) if language == "csharp" => {
                let p: Vec<String> = decorators
                    .iter()
                    .filter(|d| d.subject_local_id == c.local_id)
                    .filter_map(|d| route_template(language, d))
                    .filter(|(verb, _)| verb == "ANY")
                    .filter_map(|(_, path)| path)
                    .collect();
                if p.is_empty() {
                    vec![String::new()]
                } else {
                    p
                }
            }
            _ => vec![String::new()],
        };
        // Verbs from [HttpX]; templates from any attribute carrying one
        // ([Route("x")] on the action pairs with the verbs).
        let verbs: Vec<&str> = own
            .iter()
            .map(|(_, (v, _))| v.as_str())
            .filter(|v| *v != "ANY")
            .collect();
        let mut templates: Vec<(String, String, i64)> = Vec::new();
        for (d, (verb, path)) in &own {
            match (verb.as_str(), path) {
                ("ANY", Some(p)) if !verbs.is_empty() => {
                    for v in &verbs {
                        templates.push(((*v).to_owned(), p.clone(), d.line));
                    }
                }
                (_, Some(p)) => templates.push((verb.clone(), p.clone(), d.line)),
                (_, None) => templates.push((verb.clone(), String::new(), d.line)),
            }
        }
        let has_own_path = own.iter().any(|(_, (_, p))| p.is_some());
        templates.retain(|(_, p, _)| !p.is_empty() || !has_own_path);
        for (verb, template, line) in templates {
            let template = replace_tokens(&template, class_name, &e.name);
            let absolute = template.starts_with('/') || template.starts_with("~/");
            for prefix in &prefixes {
                let prefix = replace_tokens(prefix, class_name, &e.name);
                let joined = if absolute || prefix.is_empty() {
                    template.clone()
                } else if template.is_empty() {
                    prefix.clone()
                } else {
                    format!("{}/{}", prefix.trim_end_matches('/'), template)
                };
                let path = docmonk_core::urls::url_path(&joined);
                let Some(key) = docmonk_core::urls::url_match_key(&path) else {
                    continue;
                };
                let route = ComposedRoute {
                    subject_local_id: e.local_id,
                    method: verb.clone(),
                    path,
                    key,
                    resolver,
                    line,
                };
                if !out.contains(&route) {
                    out.push(route);
                }
            }
        }
    }
    out
}

/// Whether an attribute or decorator (as written, with or without `@`)
/// declares a server route (`[Route]`, `[Http*]`, `@app.get(...)`).
pub fn is_route_decorator(text: &str) -> bool {
    let t = text.trim();
    cs_route().is_match(t) || py_route().is_match(t)
}

pub fn detect(language: &str, decorators: &[ExtractedDecorator]) -> Vec<FrameworkFinding> {
    match language {
        "python" => detect_python_route_decorators(decorators),
        "csharp" => detect_csharp_route_attributes(decorators),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> ExtractedDecorator {
        ExtractedDecorator {
            subject_local_id: 1,
            text: text.into(),
            line: 1,
        }
    }

    #[test]
    fn python_routes() {
        let f = detect(
            "python",
            &[
                dec("@app.route(\"/dogs\", methods=[\"post\"])"),
                dec("@app.get('/x')"),
                dec("@app.route('/y')"),
                dec("@login_required"),
                dec("@app.route(\"/bad')"),
            ],
        );
        let t: Vec<_> = f.iter().map(|f| f.target_symbol.as_str()).collect();
        assert_eq!(
            t,
            [
                "http_endpoint:POST:/dogs",
                "http_endpoint:GET:/x",
                "http_endpoint:GET:/y"
            ]
        );
    }

    // clean-slate-audit: allow-start (API URL paths with a version segment are user data)
    #[test]
    fn csharp_controller_and_action_routes_compose() {
        let src = br#"namespace Shop {
  [ApiController]
  [Route("sb/v1")]
  public class ContentController : ControllerBase {
    [HttpGet("content/groups/image")]
    public IActionResult GetImage() { return Ok(); }
    [HttpPost]
    [Route("content/groups/{id:int}")]
    public IActionResult Post(int id) { return Ok(); }
    [HttpGet("/health")]
    public IActionResult Health() { return Ok(); }
    public void NotRouted() {}
  }
  [Route("api/[controller]")]
  public class OrdersController {
    [HttpDelete("[action]/{id}")]
    public void Cancel(int id) {}
  }
}
"#;
        let x = crate::extract::extract(src, "csharp", "f", "f").unwrap();
        let got: Vec<(String, String, String)> =
            compose_routes("csharp", &x.entities, &x.decorators)
                .into_iter()
                .map(|r| (r.method, r.path, r.key))
                .collect();
        assert_eq!(
            got,
            [
                (
                    "GET".into(),
                    "/sb/v1/content/groups/image".into(),
                    "sb/v1/content/groups/image".into()
                ),
                (
                    "POST".into(),
                    "/sb/v1/content/groups/{id:int}".into(),
                    "sb/v1/content/groups/{}".into()
                ),
                ("GET".into(), "/health".into(), "health".into()),
                (
                    "DELETE".into(),
                    "/api/Orders/Cancel/{id}".into(),
                    "api/orders/cancel/{}".into()
                ),
            ]
        );
    }
    // clean-slate-audit: allow-end

    #[test]
    fn route_decorators_are_recognized() {
        assert!(is_route_decorator("HttpGet(\"x\")"));
        assert!(is_route_decorator("Route(\"api\")"));
        assert!(is_route_decorator("@app.get('/x')"));
        assert!(!is_route_decorator("Get(\"/api/orders\")"));
        assert!(!is_route_decorator("Authorize"));
    }

    #[test]
    fn refit_client_attributes_keep_their_literals() {
        let src = br#"namespace Shop {
  public interface IImageApi {
    [Get("/api/content/groups/image")]
    Task<byte[]> GetImage();
  }
}
"#;
        let x = crate::extract::extract(src, "csharp", "f", "f").unwrap();
        let lits: Vec<(&str, i64)> = x.literals.iter().map(|l| (l.text.as_str(), l.line)).collect();
        assert_eq!(lits, [("/api/content/groups/image", 3)]);
    }

    #[test]
    fn csharp_routes() {
        let f = detect(
            "csharp",
            &[
                dec("HttpGet"),
                dec("Route(\"api/x\")"),
                dec("HttpPost(\"p\")"),
                dec("Authorize"),
            ],
        );
        let t: Vec<_> = f.iter().map(|f| f.target_symbol.as_str()).collect();
        assert_eq!(
            t,
            [
                "http_endpoint:GET:",
                "http_endpoint:ANY:api/x",
                "http_endpoint:POST:p"
            ]
        );
    }
}
