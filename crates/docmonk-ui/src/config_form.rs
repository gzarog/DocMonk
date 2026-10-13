//! The Configuration page: every config section as typed fields, with env
//! overrides read-only and sensitive values never shown, and form updates
//! validated exactly like `config set`.

use docmonk_config::model::DocMonkConfig;
use docmonk_config::value::{deep_merge, Value as Cfg};
use docmonk_core::paths::Home;
use serde_json::{json, Value};

use docmonk_service::load;

const ENV_PREFIX: &str = "DOCMONK_";
const SENSITIVE: &[&str] = &["key", "token", "secret", "password", "credential"];

fn is_sensitive(path: &str) -> bool {
    let lower = path.to_lowercase();
    SENSITIVE.iter().any(|t| lower.contains(t))
}

fn env_var_for(path: &str) -> String {
    format!("{ENV_PREFIX}{}", path.replace('.', "__").to_uppercase())
}

fn kind_of(v: &Cfg) -> Option<&'static str> {
    match v {
        Cfg::Bool(_) => Some("bool"),
        Cfg::Int(_) => Some("int"),
        Cfg::Float(_) => Some("float"),
        Cfg::Str(_) | Cfg::Null => Some("str"),
        Cfg::List(items) if items.iter().all(|i| matches!(i, Cfg::Str(_))) => Some("list"),
        _ => None,
    }
}

fn configured(v: &Cfg) -> bool {
    match v {
        Cfg::Null => false,
        Cfg::Str(s) => !s.is_empty(),
        Cfg::List(l) => !l.is_empty(),
        _ => true,
    }
}

fn describe(map: &[(String, Cfg)], prefix: &str, env: &dyn Fn(&str) -> bool) -> Vec<Value> {
    let mut fields = Vec::new();
    for (name, value) in map {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Cfg::Map(inner) = value {
            fields.push(json!({
                "name": name,
                "path": path,
                "kind": "group",
                "fields": describe(inner, &path, env),
            }));
            continue;
        }
        let Some(kind) = kind_of(value) else { continue };
        let env_var = env_var_for(&path);
        let overridden = env(&env_var);
        let sensitive = is_sensitive(&path);
        let display = if sensitive {
            if configured(value) {
                "Configured"
            } else {
                "Not configured"
            }
            .to_owned()
        } else {
            match value {
                Cfg::List(items) => items
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                Cfg::Null => String::new(),
                other => other.to_string(),
            }
        };
        fields.push(json!({
            "name": name,
            "path": path,
            "kind": kind,
            "value": if sensitive { Value::Null } else { value.to_json() },
            "display": display,
            "sensitive": sensitive,
            "env_overridden": overridden,
            "env_var": if overridden { json!(env_var) } else { Value::Null },
            "editable": !overridden && !sensitive,
        }));
    }
    fields
}

fn env_present(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// Every config section with its fields, for the Configuration page.
pub fn describe_config(home: &Home) -> Result<Vec<Value>, docmonk_core::errors::DocMonkError> {
    let cfg = load(home)?;
    let dumped = cfg.to_value();
    let mut sections = Vec::new();
    for (name, v) in dumped.as_map().into_iter().flatten() {
        if let Cfg::Map(inner) = v {
            sections.push(json!({"name": name, "fields": describe(inner, name, &env_present)}));
        }
    }
    Ok(sections)
}

fn kinds(sections: &[Value]) -> Vec<(String, String)> {
    fn walk(fields: &Value, out: &mut Vec<(String, String)>) {
        for f in fields.as_array().into_iter().flatten() {
            if f["kind"] == "group" {
                walk(&f["fields"], out);
            } else if let (Some(p), Some(k)) = (f["path"].as_str(), f["kind"].as_str()) {
                out.push((p.to_owned(), k.to_owned()));
            }
        }
    }
    let mut out = Vec::new();
    for s in sections {
        walk(&s["fields"], &mut out);
    }
    out
}

/// A form field as a config value: checkboxes are booleans, list text areas
/// are comma/newline separated, everything else is parsed by validation.
fn form_value(kind: &str, raw: &str) -> Cfg {
    let raw = raw.trim();
    match kind {
        "bool" => Cfg::Bool(docmonk_config::coerce::parse_bool(raw).unwrap_or(false)),
        "list" => Cfg::str(raw.replace('\n', ",")),
        _ => Cfg::str(raw),
    }
}

fn set_path(target: &mut Cfg, path: &str, value: Cfg) {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let mut cursor = target;
    for part in parts {
        if !matches!(cursor.get(part), Some(Cfg::Map(_))) {
            cursor.set(part, Cfg::empty_map());
        }
        let Some(next) = cursor.get_mut(part) else {
            return;
        };
        cursor = next;
    }
    cursor.set(last, value);
}

/// Validates the form, then writes the user config. Sensitive and
/// env-overridden fields are ignored. The error string is what the page
/// shows.
pub fn apply_updates(home: &Home, form: &[(String, String)]) -> Result<(), String> {
    let mut user = docmonk_config::loader::load_yaml_file(&home.user_config())
        .map_err(|e| e.message().to_owned())?;
    let sections = describe_config(home).map_err(|e| e.message().to_owned())?;
    let kinds = kinds(&sections);
    for (key, raw) in form {
        let path = key.replace("__", ".");
        let Some((_, kind)) = kinds.iter().find(|(p, _)| *p == path) else {
            continue;
        };
        if is_sensitive(&path) || env_present(&env_var_for(&path)) {
            continue;
        }
        set_path(&mut user, &path, form_value(kind, raw));
    }
    let merged = deep_merge(&DocMonkConfig::default().to_value(), &user);
    let validated = DocMonkConfig::validate(&merged).map_err(|e| e.render())?;
    docmonk_config::loader::write_user_config(&validated, home).map_err(|e| e.to_string())?;
    Ok(())
}
