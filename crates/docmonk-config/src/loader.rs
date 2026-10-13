//! Layered configuration loading.
//!
//! Precedence, highest wins: CLI overrides > environment variables
//! (`DOCMONK_` prefix, `__` nesting) > project config (`./.docmonk.yaml`)
//! > user config (`<home>/config.yaml`) > built-in defaults.

use std::path::{Path, PathBuf};

use docmonk_core::errors::DocMonkError;
use docmonk_core::paths::{project_config_path, Home};

use crate::model::{DocMonkConfig, KNOWN_SECTIONS};
use crate::value::{deep_merge, Value};

const ENV_PREFIX: &str = "DOCMONK_";

/// Inputs to [`load_config`]; `None` fields mean "use the process value".
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    pub home: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    /// Ordered `(name, value)` pairs; `None` reads the process environment.
    pub environ: Option<Vec<(String, String)>>,
    pub cli_overrides: Option<Value>,
}

/// Reads a YAML config file; a missing or empty file is an empty mapping.
pub fn load_yaml_file(path: &Path) -> Result<Value, DocMonkError> {
    if !path.is_file() {
        return Ok(Value::empty_map());
    }
    let read_err = |e: &dyn std::fmt::Display| {
        DocMonkError::config(format!(
            "failed to read config file {}: {e}",
            path.display()
        ))
    };
    let raw = std::fs::read_to_string(path).map_err(|e| read_err(&e))?;
    match crate::yaml::parse(&raw).map_err(|e| read_err(&e))? {
        Value::Null => Ok(Value::empty_map()),
        map @ Value::Map(_) => Ok(map),
        _ => Err(DocMonkError::config(format!(
            "config file {} must contain a mapping at the top level",
            path.display()
        ))),
    }
}

/// `DOCMONK_<SECTION>__<KEY>[__<KEY>...]=value` variables as a value tree.
/// Values stay strings; validation parses them into each field's type.
pub fn env_overrides(environ: &[(String, String)]) -> Value {
    let mut overrides = Value::empty_map();
    for (key, value) in environ {
        let Some(remainder) = key.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        if !remainder.contains("__") {
            continue;
        }
        let segments: Vec<String> = remainder
            .split("__")
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .collect();
        let Some(first) = segments.first() else {
            continue;
        };
        if !KNOWN_SECTIONS.contains(&first.as_str()) {
            continue;
        }
        set_nested(&mut overrides, &segments, Value::str(value.as_str()));
    }
    overrides
}

fn set_nested(root: &mut Value, segments: &[String], value: Value) {
    if segments.len() == 1 {
        root.set(&segments[0], value);
        return;
    }
    if !matches!(root.get(&segments[0]), Some(Value::Map(_))) {
        root.set(&segments[0], Value::empty_map());
    }
    if let Some(child) = root.get_mut(&segments[0]) {
        set_nested(child, &segments[1..], value);
    }
}

/// The merged, not-yet-validated layers.
pub fn merged_layers(opts: &LoadOptions) -> Result<Value, DocMonkError> {
    let home = opts
        .home
        .clone()
        .map(Home::new)
        .unwrap_or_else(Home::discover);
    let cwd = match &opts.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir().map_err(|e| DocMonkError::config(e.to_string()))?,
    };
    let environ = opts.environ.clone().unwrap_or_else(|| {
        std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect()
    });
    let defaults = DocMonkConfig::default().to_value();
    let user = load_yaml_file(&home.user_config())?;
    let project = load_yaml_file(&project_config_path(&cwd))?;
    let env = env_overrides(&environ);
    let mut merged = deep_merge(&defaults, &user);
    merged = deep_merge(&merged, &project);
    merged = deep_merge(&merged, &env);
    if let Some(cli) = &opts.cli_overrides {
        merged = deep_merge(&merged, cli);
    }
    Ok(merged)
}

/// Loads and validates the layered configuration. Validation errors are a
/// config error (exit code 3) whose message has URL user-info scrubbed.
pub fn load_config(opts: &LoadOptions) -> Result<DocMonkConfig, DocMonkError> {
    let merged = merged_layers(opts)?;
    DocMonkConfig::validate(&merged).map_err(|errors| {
        let message = scrub_url_userinfo(&errors.render());
        DocMonkError::config(format!("invalid configuration: {message}"))
    })
}

fn scrub_url_userinfo(text: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"([a-zA-Z][a-zA-Z0-9+.\-]*://)[^\s/@]+@").expect("static regex")
    })
    .replace_all(text, "$1")
    .into_owned()
}

/// The configuration as YAML (`config show`, `config.yaml`).
pub fn dump_yaml(config: &DocMonkConfig) -> String {
    crate::yaml::emit(&config.to_value())
}

/// Writes the full configuration to `<home>/config.yaml`.
pub fn write_user_config(config: &DocMonkConfig, home: &Home) -> std::io::Result<PathBuf> {
    let path = home.user_config();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, dump_yaml(config))?;
    Ok(path)
}

/// `config get`: the value at a dotted key.
pub fn get_path(config: &DocMonkConfig, dotted: &str) -> Result<Value, DocMonkError> {
    let unknown = || DocMonkError::usage(format!("unknown config key: {dotted}"));
    let mut cursor = config.to_value();
    for segment in dotted.split('.') {
        cursor = cursor.get(segment).cloned().ok_or_else(unknown)?;
    }
    Ok(cursor)
}

/// `config set`: parses `raw` into the key's type and validates the
/// result. Returns the validated config and the stored value. An unknown
/// key is a usage error (exit code 2); an unparsable or invalid value is a
/// generic failure (exit code 1).
pub fn set_path(
    config: &DocMonkConfig,
    dotted: &str,
    raw: &str,
) -> Result<(DocMonkConfig, Value), DocMonkError> {
    let unknown = || DocMonkError::usage(format!("unknown config key: {dotted}"));
    let mut data = config.to_value();
    let mut cursor = &mut data;
    for segment in dotted.split('.') {
        cursor = cursor.get_mut(segment).ok_or_else(unknown)?;
    }
    if matches!(cursor, Value::Map(_)) {
        return Err(unknown());
    }
    *cursor = Value::str(raw);
    let updated = DocMonkConfig::validate(&data).map_err(|errors| {
        DocMonkError::new(
            docmonk_core::ErrorKind::Generic,
            format!(
                "invalid configuration: {}",
                scrub_url_userinfo(&errors.render())
            ),
        )
    })?;
    let stored = get_path(&updated, dotted)?;
    Ok((updated, stored))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_layer_is_typed_by_validation() {
        let env = vec![
            ("DOCMONK_SEARCH__SEMANTIC".to_owned(), " TrUe ".to_owned()),
            (
                "DOCMONK_RUNTIME__SQLITE_CACHE_SIZE_MB".to_owned(),
                "16".to_owned(),
            ),
            ("DOCMONK_NOT_A_SECTION__X".to_owned(), "1".to_owned()),
            ("DOCMONK_HOME".to_owned(), "/x".to_owned()),
        ];
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_config(&LoadOptions {
            home: Some(tmp.path().to_path_buf()),
            cwd: Some(tmp.path().to_path_buf()),
            environ: Some(env),
            cli_overrides: None,
        })
        .unwrap();
        assert!(cfg.search.semantic);
        assert_eq!(cfg.runtime.sqlite_cache_size_mb, 16);
    }

    #[test]
    fn layers_and_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = |env: Vec<(String, String)>| LoadOptions {
            home: Some(tmp.path().join("home")),
            cwd: Some(tmp.path().to_path_buf()),
            environ: Some(env),
            cli_overrides: None,
        };
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        std::fs::write(
            tmp.path().join("home/config.yaml"),
            "runtime:\n  log_level: debug\nsearch:\n  lexical: false\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".docmonk.yaml"),
            "runtime:\n  log_level: warning\n",
        )
        .unwrap();
        let cfg = load_config(&opts(vec![])).unwrap();
        assert_eq!(cfg.runtime.log_level, "warning");
        assert!(!cfg.search.lexical);
        std::fs::write(
            tmp.path().join(".docmonk.yaml"),
            "runtime:\n  max_workers: 2\n",
        )
        .unwrap();
        let err = load_config(&opts(vec![])).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(
            err.to_string()
                .contains("runtime.max_workers: unknown setting"),
            "{err}"
        );
    }

    #[test]
    fn set_and_get() {
        let cfg = DocMonkConfig::default();
        let (cfg, v) = set_path(&cfg, "runtime.sqlite_cache_size_mb", "32").unwrap();
        assert_eq!(v, Value::Int(32));
        assert_eq!(cfg.runtime.sqlite_cache_size_mb, 32);
        let (cfg, _) = set_path(&cfg, "search.output.fallback", "json, files,").unwrap();
        assert_eq!(cfg.search.output.fallback, vec!["json", "files"]);
        let (cfg, v) = set_path(&cfg, "indexing.watch", "off").unwrap();
        assert_eq!(v, Value::Bool(false));
        let (cfg, _) = set_path(&cfg, "ai.base_url", "http://x").unwrap();
        assert_eq!(cfg.ai.base_url.as_deref(), Some("http://x"));
        assert_eq!(set_path(&cfg, "no.such", "1").unwrap_err().exit_code(), 2);
        assert_eq!(set_path(&cfg, "search", "1").unwrap_err().exit_code(), 2);
        assert_eq!(
            set_path(&cfg, "runtime.sqlite_cache_size_mb", "x")
                .unwrap_err()
                .exit_code(),
            1
        );
        assert_eq!(
            set_path(&cfg, "storage.mode", "bogus")
                .unwrap_err()
                .exit_code(),
            1
        );
        assert_eq!(
            get_path(&cfg, "runtime.sqlite_cache_size_mb")
                .unwrap()
                .to_string(),
            "32"
        );
        assert_eq!(get_path(&cfg, "runtime.nope").unwrap_err().exit_code(), 2);
    }

    #[test]
    fn dump_is_loadable() {
        let cfg = DocMonkConfig::default();
        let text = dump_yaml(&cfg);
        assert!(text.starts_with("runtime:\n"), "{text}");
        let back = crate::yaml::parse(&text).unwrap();
        assert_eq!(DocMonkConfig::validate(&back).unwrap(), cfg);
    }
}
