//! Coarse file classification by extension.

use std::path::Path;

use docmonk_core::models::FileKind;

pub const CODE_EXTENSIONS: &[&str] = &[
    ".py", ".pyi", ".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".go", ".rs", ".java", ".kt",
    ".kts", ".scala", ".c", ".h", ".cpp", ".cc", ".hpp", ".cs", ".rb", ".php", ".swift", ".m",
    ".sh", ".bash", ".zsh", ".ps1", ".sql", ".lua", ".pl", ".r",
];

pub const DOCUMENT_EXTENSIONS: &[&str] = &[
    ".pdf",
    ".doc",
    ".docx",
    ".ppt",
    ".pptx",
    ".xls",
    ".xlsx",
    ".md",
    ".markdown",
    ".txt",
    ".rst",
    ".csv",
    ".html",
    ".htm",
    ".odt",
    ".rtf",
    ".eml",
    ".ods",
    ".odp",
    ".epub",
    ".png",
    ".jpg",
    ".jpeg",
    ".tif",
    ".tiff",
];

/// The lowercased suffix: the last `.ext` of the final component,
/// empty for dotfiles like `.env` and names ending in a dot.
pub fn suffix_lower(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) if i + 1 == name.len() => String::new(),
        Some(i) => name[i..].to_lowercase(),
    }
}

pub fn classify(path: &Path) -> FileKind {
    let suffix = suffix_lower(path);
    if CODE_EXTENSIONS.contains(&suffix.as_str()) {
        FileKind::Code
    } else if DOCUMENT_EXTENSIONS.contains(&suffix.as_str()) {
        FileKind::Document
    } else {
        FileKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_suffix() {
        assert_eq!(classify(Path::new("a/B.PY")), FileKind::Code);
        assert_eq!(classify(Path::new("x.Eml")), FileKind::Document);
        assert_eq!(classify(Path::new(".env")), FileKind::Unknown);
        assert_eq!(classify(Path::new("archive.tar.gz")), FileKind::Unknown);
        assert_eq!(suffix_lower(Path::new("name.")), "");
    }
}
