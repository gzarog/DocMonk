//! The configuration value tree: YAML scalars, sequences and string-keyed,
//! insertion-ordered mappings.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    /// Insertion-ordered mapping; keys are unique.
    Map(Vec<(String, Value)>),
}

impl Value {
    pub fn str(s: impl Into<String>) -> Self {
        Value::Str(s.into())
    }

    pub fn empty_map() -> Self {
        Value::Map(Vec::new())
    }

    pub fn as_map(&self) -> Option<&Vec<(String, Value)>> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_map()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Map(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Inserts or replaces `key` in a mapping (no-op on other values).
    pub fn set(&mut self, key: &str, value: Value) {
        if let Value::Map(m) = self {
            match m.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = value,
                None => m.push((key.to_owned(), value)),
            }
        }
    }

    /// The JSON form (for the Admin UI and `--json` output).
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => (*b).into(),
            Value::Int(i) => (*i).into(),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map_or(serde_json::Value::Null, serde_json::Value::Number),
            Value::Str(s) => s.clone().into(),
            Value::List(items) => items.iter().map(Value::to_json).collect(),
            Value::Map(m) => m
                .iter()
                .map(|(k, v)| (k.clone(), v.to_json()))
                .collect::<serde_json::Map<_, _>>()
                .into(),
        }
    }
}

/// Scalars print as plain text (`true`, `30.0`, `info`); sequences and
/// mappings print as YAML.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x:?}"),
            Value::Str(s) => f.write_str(s),
            Value::List(_) | Value::Map(_) => f.write_str(crate::yaml::emit(self).trim_end()),
        }
    }
}

/// Deep merge: mappings merge key by key, anything else in `over` wins.
pub fn deep_merge(base: &Value, over: &Value) -> Value {
    let (Value::Map(b), Value::Map(o)) = (base, over) else {
        return over.clone();
    };
    let mut result = Value::Map(b.clone());
    for (key, value) in o {
        let merged = match (value, result.get(key)) {
            (Value::Map(_), Some(prev @ Value::Map(_))) => deep_merge(prev, value),
            _ => value.clone(),
        };
        result.set(key, merged);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_merge() {
        assert_eq!(Value::Float(30.0).to_string(), "30.0");
        assert_eq!(Value::Bool(true).to_string(), "true");
        let base = Value::Map(vec![(
            "a".into(),
            Value::Map(vec![
                ("x".into(), Value::Int(1)),
                ("y".into(), Value::Int(2)),
            ]),
        )]);
        let over = Value::Map(vec![(
            "a".into(),
            Value::Map(vec![("y".into(), Value::Int(3))]),
        )]);
        let m = deep_merge(&base, &over);
        assert_eq!(m.get("a").unwrap().get("x"), Some(&Value::Int(1)));
        assert_eq!(m.get("a").unwrap().get("y"), Some(&Value::Int(3)));
    }
}
