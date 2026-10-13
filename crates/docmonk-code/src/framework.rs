//! Framework heuristics: Flask/FastAPI
//! route decorators and ASP.NET route attributes. Always HEURISTIC.

use std::sync::OnceLock;

use regex::Regex;

use crate::extract::ExtractedDecorator;

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
