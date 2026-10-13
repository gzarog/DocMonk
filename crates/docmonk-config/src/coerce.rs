//! Conversion of loaded values to typed fields.
//!
//! YAML values convert when the types match; strings (environment
//! variables, `config set`, the Admin UI form) are also parsed into the
//! field's type, so `RAGMONK_SEARCH__SEMANTIC=true` and `semantic: true`
//! mean the same thing.

use crate::value::Value;

pub const MSG_INT: &str = "expected an integer";
pub const MSG_FLOAT: &str = "expected a number";
pub const MSG_BOOL: &str = "expected true or false";
pub const MSG_STR: &str = "expected a string";
pub const MSG_LIST: &str = "expected a list of strings";
pub const MSG_MAP: &str = "expected a mapping";

pub type Coerced<T> = Result<T, &'static str>;

/// Parses an integer string (surrounding whitespace allowed).
pub fn parse_int(raw: &str) -> Option<i64> {
    raw.trim().parse().ok()
}

/// Parses a finite number string (surrounding whitespace allowed).
pub fn parse_float(raw: &str) -> Option<f64> {
    raw.trim().parse::<f64>().ok().filter(|f| f.is_finite())
}

/// Parses a boolean string: `true`/`false`, `yes`/`no`, `on`/`off`, `1`/`0`.
pub fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

pub fn to_int(v: &Value) -> Coerced<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18 => Ok(*f as i64),
        Value::Str(s) => parse_int(s).ok_or(MSG_INT),
        _ => Err(MSG_INT),
    }
}

pub fn to_float(v: &Value) -> Coerced<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) if f.is_finite() => Ok(*f),
        Value::Str(s) => parse_float(s).ok_or(MSG_FLOAT),
        _ => Err(MSG_FLOAT),
    }
}

pub fn to_bool(v: &Value) -> Coerced<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Str(s) => parse_bool(s).ok_or(MSG_BOOL),
        _ => Err(MSG_BOOL),
    }
}

pub fn to_str(v: &Value) -> Coerced<String> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Int(i) => Ok(i.to_string()),
        Value::Float(f) => Ok(format!("{f:?}")),
        _ => Err(MSG_STR),
    }
}

/// `null` (or an empty string) is `None`.
pub fn to_opt_str(v: &Value) -> Coerced<Option<String>> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) if s.is_empty() => Ok(None),
        other => to_str(other).map(Some),
    }
}

/// A sequence of strings; a string is split on commas.
pub fn to_str_list(v: &Value) -> Coerced<Vec<String>> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|i| match i {
                Value::Str(s) => Ok(s.clone()),
                _ => Err(MSG_LIST),
            })
            .collect(),
        Value::Str(s) => Ok(s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()),
        _ => Err(MSG_LIST),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_parse_into_the_field_type() {
        assert_eq!(to_int(&Value::str(" 5 ")), Ok(5));
        assert_eq!(to_int(&Value::Float(5.0)), Ok(5));
        assert_eq!(to_int(&Value::Float(5.5)), Err(MSG_INT));
        assert_eq!(to_int(&Value::Bool(true)), Err(MSG_INT));
        assert_eq!(to_float(&Value::str("1e3")), Ok(1000.0));
        assert_eq!(to_float(&Value::str("nan")), Err(MSG_FLOAT));
        assert_eq!(to_bool(&Value::str("Yes")), Ok(true));
        assert_eq!(to_bool(&Value::Int(1)), Err(MSG_BOOL));
        assert_eq!(to_opt_str(&Value::Null), Ok(None));
        assert_eq!(
            to_str_list(&Value::str("json, files,")),
            Ok(vec!["json".to_owned(), "files".to_owned()])
        );
    }
}
