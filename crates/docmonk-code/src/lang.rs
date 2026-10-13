//! Language detection and Tree-sitter grammars. Queries are the bundled
//! `.scm` files,
//! compiled once per language.

use std::path::Path;
use std::sync::OnceLock;

use tree_sitter::{Language, Parser, Query, Tree};

/// Extension (lowercase, with dot) -> language name.
pub const LANGUAGE_BY_EXTENSION: &[(&str, &str)] = &[
    (".py", "python"),
    (".pyi", "python"),
    (".js", "javascript"),
    (".jsx", "javascript"),
    (".mjs", "javascript"),
    (".cjs", "javascript"),
    (".ts", "typescript"),
    (".mts", "typescript"),
    (".cts", "typescript"),
    (".tsx", "tsx"),
    (".go", "go"),
    (".rs", "rust"),
    (".java", "java"),
    (".cs", "csharp"),
];

pub const SUPPORTED_LANGUAGES: &[&str] = &[
    "csharp",
    "go",
    "java",
    "javascript",
    "python",
    "rust",
    "tsx",
    "typescript",
];

/// Lookup by the lowercased final extension.
pub fn detect_language(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    let suffix = path_suffix(name)?.to_lowercase();
    LANGUAGE_BY_EXTENSION
        .iter()
        .find(|(ext, _)| *ext == suffix)
        .map(|(_, lang)| *lang)
}

/// `PurePath(name).suffix`: from the last dot, unless the dot leads the
/// name or ends it.
pub fn path_suffix(name: &str) -> Option<&str> {
    let i = name.rfind('.')?;
    (i > 0 && i + 1 < name.len()).then(|| &name[i..])
}

fn grammar(language: &str) -> Option<Language> {
    Some(match language {
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        _ => return None,
    })
}

fn query_source(language: &str) -> Option<&'static str> {
    Some(match language {
        "python" => include_str!("../queries/python.scm"),
        "javascript" => include_str!("../queries/javascript.scm"),
        "typescript" | "tsx" => include_str!("../queries/typescript.scm"),
        "go" => include_str!("../queries/go.scm"),
        "rust" => include_str!("../queries/rust.scm"),
        "java" => include_str!("../queries/java.scm"),
        "csharp" => include_str!("../queries/csharp.scm"),
        _ => return None,
    })
}

/// Parses `source`; `None` for an unsupported language.
pub fn parse(source: &[u8], language: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(language)?).ok()?;
    parser.parse(source, None)
}

/// The compiled query for `language` (compiled once, shared by threads).
pub fn query(language: &str) -> Option<&'static Query> {
    macro_rules! cached {
        ($($lang:literal),+) => {
            match language {
                $($lang => {
                    static Q: OnceLock<Query> = OnceLock::new();
                    Some(Q.get_or_init(|| {
                        Query::new(&grammar($lang).expect("grammar"), query_source($lang).expect("query"))
                            .unwrap_or_else(|e| panic!("{} query does not compile: {e}", $lang))
                    }))
                })+
                _ => None,
            }
        };
    }
    cached!(
        "python",
        "javascript",
        "typescript",
        "tsx",
        "go",
        "rust",
        "java",
        "csharp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_like_the_reference() {
        assert_eq!(detect_language(Path::new("a/B.CS")), Some("csharp"));
        assert_eq!(detect_language(Path::new("x.tsx")), Some("tsx"));
        assert_eq!(detect_language(Path::new("x.rb")), None);
        assert_eq!(detect_language(Path::new(".py")), None);
        assert_eq!(detect_language(Path::new("Makefile")), None);
    }

    #[test]
    fn every_query_compiles() {
        for lang in SUPPORTED_LANGUAGES {
            assert!(query(lang).is_some(), "{lang}");
        }
    }
}
