//! The Admin UI's Jinja templates, embedded and rendered with minijinja.
//!
//! Output matches Jinja2 + MarkupSafe: `None`/`True`/`False` print as
//! such, whole floats keep their `.0`, and autoescaping uses MarkupSafe's
//! entities (`&#34;`, `&#39;`). String/dict methods (`.get`,
//! `.split`, `.replace`) come from minijinja-contrib's pycompat.

use std::sync::OnceLock;

use minijinja::value::{Value, ValueKind};
use minijinja::{AutoEscape, Environment, Error, Output, State};

macro_rules! embedded {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../assets/templates/", $name)))),*]
    };
}

const TEMPLATES: &[(&str, &str)] = embedded!(
    "base.html",
    "dashboard.html",
    "sources/list.html",
    "sources/detail.html",
    "indexing/index.html",
    "indexing/failed.html",
    "documents/list.html",
    "documents/detail.html",
    "search/index.html",
    "knowledge/index.html",
    "knowledge/symbol.html",
    "ai/index.html",
    "config/index.html",
    "daemon/index.html",
    "daemon/_panel.html",
    "daemon/_logs.html",
    "health/index.html",
    "backups/index.html",
    "logs/index.html",
    "system/index.html",
);

/// `(content type, bytes)` of an embedded static asset.
pub fn static_asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "css/app.css" => Some((
            "text/css; charset=utf-8",
            include_bytes!("../assets/static/css/app.css"),
        )),
        "js/htmx.min.js" => Some((
            "text/javascript; charset=utf-8",
            include_bytes!("../assets/static/js/htmx.min.js"),
        )),
        _ => None,
    }
}

/// A template value as display text: empty for none, `true`/`false`, and
/// integral floats with one decimal.
pub fn display(v: &Value) -> String {
    match v.kind() {
        ValueKind::Undefined | ValueKind::None => String::new(),
        ValueKind::Bool => if v.is_true() { "true" } else { "false" }.into(),
        ValueKind::Number => match f64::try_from(v.clone()) {
            Ok(f) if v.as_i64().is_none() => {
                if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
                    format!("{f:.1}")
                } else {
                    v.to_string()
                }
            }
            _ => v.to_string(),
        },
        _ => v.to_string(),
    }
}

/// MarkupSafe's `escape`.
pub fn markup_escape(s: &str, out: &mut impl std::fmt::Write) -> std::fmt::Result {
    for c in s.chars() {
        match c {
            '&' => out.write_str("&amp;")?,
            '<' => out.write_str("&lt;")?,
            '>' => out.write_str("&gt;")?,
            '"' => out.write_str("&#34;")?,
            '\'' => out.write_str("&#39;")?,
            c => out.write_char(c)?,
        }
    }
    Ok(())
}

fn formatter(out: &mut Output, state: &State, value: &Value) -> Result<(), Error> {
    let text = display(value);
    let escape = !matches!(state.auto_escape(), AutoEscape::None) && !value.is_safe();
    if escape {
        markup_escape(&text, out).map_err(Error::from)
    } else {
        out.write_str(&text).map_err(Error::from)
    }
}

pub fn env() -> &'static Environment<'static> {
    static ENV: OnceLock<Environment<'static>> = OnceLock::new();
    ENV.get_or_init(|| {
        let mut env = Environment::new();
        for (name, source) in TEMPLATES {
            env.add_template(name, source).expect("valid template");
        }
        env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
        env.set_formatter(formatter);
        // Jinja2 keeps the single trailing newline of a template.
        env.set_keep_trailing_newline(true);
        env
    })
}

pub fn render(name: &str, ctx: &serde_json::Value) -> Result<String, Error> {
    env().get_template(name)?.render(Value::from_serialize(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_compiles() {
        for (name, _) in TEMPLATES {
            env().get_template(name).unwrap();
        }
    }

    #[test]
    fn formats_values_plainly() {
        let out = env()
            .render_str(
                "{{ a }}|{{ b }}|{{ c }}|{{ d }}|{{ e }}|{{ f.get('x', 'y') }}|{{ g.split('/')[-1] }}",
                serde_json::json!({
                    "a": null, "b": true, "c": 2.0, "d": "<'\">&", "e": 3,
                    "f": {}, "g": "a/b/c",
                }),
            )
            .unwrap();
        assert_eq!(out, "|true|2.0|<'\">&|3|y|c");
        let html = render(
            "daemon/_logs.html",
            &serde_json::json!({"log_lines": ["<x> 'q'"]}),
        )
        .unwrap();
        assert!(html.contains("&lt;x&gt; &#39;q&#39;"), "{html}");
    }
}
