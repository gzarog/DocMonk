//! YAML 1.2 (core schema) parsing into [`Value`] and emission from it.

use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use crate::value::Value;

/// Parses one YAML document. An empty document is [`Value::Null`].
pub fn parse(text: &str) -> Result<Value, String> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| e.to_string())?;
    match docs.as_slice() {
        [] => Ok(Value::Null),
        [doc] => convert(doc),
        _ => Err("expected a single YAML document".into()),
    }
}

fn convert(y: &Yaml) -> Result<Value, String> {
    Ok(match y {
        Yaml::Null => Value::Null,
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Integer(i) => Value::Int(*i),
        Yaml::Real(r) => Value::Float(y.as_f64().ok_or_else(|| format!("invalid number {r:?}"))?),
        Yaml::String(s) => Value::Str(s.clone()),
        Yaml::Array(items) => Value::List(items.iter().map(convert).collect::<Result<_, _>>()?),
        Yaml::Hash(h) => {
            let mut out = Value::empty_map();
            for (k, v) in h {
                let Yaml::String(key) = k else {
                    return Err(format!("mapping keys must be strings, found {k:?}"));
                };
                out.set(key, convert(v)?);
            }
            out
        }
        Yaml::Alias(_) => return Err("YAML aliases are not supported".into()),
        Yaml::BadValue => return Err("invalid YAML value".into()),
    })
}

fn to_yaml(v: &Value) -> Yaml {
    match v {
        Value::Null => Yaml::Null,
        Value::Bool(b) => Yaml::Boolean(*b),
        Value::Int(i) => Yaml::Integer(*i),
        Value::Float(f) => Yaml::Real(format!("{f:?}")),
        Value::Str(s) => Yaml::String(s.clone()),
        Value::List(items) => Yaml::Array(items.iter().map(to_yaml).collect()),
        Value::Map(m) => {
            let mut h = Hash::new();
            for (k, v) in m {
                h.insert(Yaml::String(k.clone()), to_yaml(v));
            }
            Yaml::Hash(h)
        }
    }
}

/// Block-style YAML for `value`, ending with a newline.
pub fn emit(value: &Value) -> String {
    let mut out = String::new();
    YamlEmitter::new(&mut out)
        .dump(&to_yaml(value))
        .expect("writing YAML to a String cannot fail");
    let body = out.strip_prefix("---\n").unwrap_or(&out);
    let body = body.strip_prefix("--- ").unwrap_or(body);
    format!("{body}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let text = "runtime:\n  log_level: debug\n  sqlite_cache_size_mb: 32\nsearch:\n  semantic: true\n  output:\n    fallback:\n      - json\n      - files\nai:\n  base_url: ~\n  timeout_seconds: 60.0\n  model: \"123\"\n";
        let v = parse(text).unwrap();
        assert_eq!(
            v.get("runtime").unwrap().get("sqlite_cache_size_mb"),
            Some(&Value::Int(32))
        );
        assert_eq!(v.get("ai").unwrap().get("model"), Some(&Value::str("123")));
        assert_eq!(v.get("ai").unwrap().get("base_url"), Some(&Value::Null));
        assert_eq!(parse(&emit(&v)).unwrap(), v);
    }

    #[test]
    fn yaml_1_2_scalars() {
        // `yes`/`on` are plain strings in YAML 1.2.
        let v = parse("a: yes\nb: on\nc: 012\n").unwrap();
        assert_eq!(v.get("a"), Some(&Value::str("yes")));
        assert_eq!(v.get("b"), Some(&Value::str("on")));
        assert_eq!(v.get("c"), Some(&Value::Int(12)));
        assert_eq!(parse("").unwrap(), Value::Null);
        assert!(parse("1: a\n").is_err());
        assert!(parse("a: [1\n").is_err());
    }
}
