//! Document formats and extension routing (`docling_adapter.EXTENSION_TO_FORMAT`).

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentFormat {
    Pdf,
    Docx,
    Pptx,
    Xlsx,
    Html,
    Markdown,
    Txt,
    Eml,
    Csv,
    Odt,
    Ods,
    Odp,
    Epub,
    Image,
}

impl DocumentFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Pptx => "pptx",
            Self::Xlsx => "xlsx",
            Self::Html => "html",
            Self::Markdown => "markdown",
            Self::Txt => "txt",
            Self::Eml => "eml",
            Self::Csv => "csv",
            Self::Odt => "odt",
            Self::Ods => "ods",
            Self::Odp => "odp",
            Self::Epub => "epub",
            Self::Image => "image",
        }
    }
}

const EXTENSION_TO_FORMAT: &[(&str, DocumentFormat)] = &[
    (".pdf", DocumentFormat::Pdf),
    (".docx", DocumentFormat::Docx),
    (".pptx", DocumentFormat::Pptx),
    (".xlsx", DocumentFormat::Xlsx),
    (".html", DocumentFormat::Html),
    (".htm", DocumentFormat::Html),
    (".md", DocumentFormat::Markdown),
    (".markdown", DocumentFormat::Markdown),
    (".txt", DocumentFormat::Txt),
    (".eml", DocumentFormat::Eml),
    (".csv", DocumentFormat::Csv),
    (".odt", DocumentFormat::Odt),
    (".ods", DocumentFormat::Ods),
    (".odp", DocumentFormat::Odp),
    (".epub", DocumentFormat::Epub),
    (".png", DocumentFormat::Image),
    (".jpg", DocumentFormat::Image),
    (".jpeg", DocumentFormat::Image),
    (".tif", DocumentFormat::Image),
    (".tiff", DocumentFormat::Image),
];

/// `Path.suffix.lower()` lookup; `None` = not a convertible document.
pub fn detect_format(path: &Path) -> Option<DocumentFormat> {
    let name = path.file_name()?.to_str()?;
    let i = name.rfind('.')?;
    if i == 0 || i + 1 == name.len() {
        return None;
    }
    let suffix = name[i..].to_lowercase();
    EXTENSION_TO_FORMAT
        .iter()
        .find(|(e, _)| *e == suffix)
        .map(|(_, f)| *f)
}
