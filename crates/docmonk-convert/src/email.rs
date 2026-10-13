//! MIME attachment enumeration for `.eml` files
//! with these safety
//! rules:
//!
//! - Filenames are display metadata only. They are decoded (RFC 2047 /
//!   RFC 2231), sanitized to a bare basename, and never used to name
//!   anything on disk.
//! - Size, count and total-size limits apply to decoded payloads, before
//!   any conversion is attempted.
//! - `message/rfc822` parts are reported and never recursed into.
//!   Unsupported types such as archives are skipped.
//! - Payload bytes are never logged.

use mail_parser::{MessageParser, MimeHeaders, PartType};

pub const SKIP_EMPTY: &str = "empty";
pub const SKIP_TOO_LARGE: &str = "too_large";
pub const SKIP_COUNT_LIMIT: &str = "count_limit";
pub const SKIP_TOTAL_LIMIT: &str = "total_limit";
pub const SKIP_DECODE_ERROR: &str = "decode_error";
pub const SKIP_NESTED_MESSAGE: &str = "nested_message";
pub const SKIP_UNSUPPORTED: &str = "unsupported_format";
pub const SKIP_IMAGE_OCR_DISABLED: &str = "image_ocr_disabled";
pub const SKIP_PAGE_LIMIT: &str = "page_limit";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSettings {
    pub enabled: bool,
    pub max_bytes: usize,
    pub max_count: usize,
    pub total_max_bytes: usize,
}

impl Default for AttachmentSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 25 * 1024 * 1024,
            max_count: 50,
            total_max_bytes: 100 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentPart {
    /// 0-based position among all attachment-like parts, skipped ones
    /// included; stable across re-parses.
    pub ordinal: usize,
    pub filename: Option<String>,
    pub content_type: String,
    pub content_id: Option<String>,
    pub payload: Vec<u8>,
}

impl AttachmentPart {
    pub fn display_name(&self) -> String {
        self.filename
            .clone()
            .unwrap_or_else(|| format!("attachment-{}", self.ordinal + 1))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedAttachment {
    pub ordinal: usize,
    pub filename: Option<String>,
    pub content_type: String,
    pub size: Option<usize>,
    pub reason: &'static str,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentExtraction {
    pub attachments: Vec<AttachmentPart>,
    pub skipped: Vec<SkippedAttachment>,
}

impl AttachmentExtraction {
    pub fn seen(&self) -> usize {
        self.attachments.len() + self.skipped.len()
    }
}

/// Unicode "other" (C*) characters stripped from names: controls,
/// formats, surrogates, private use and unassigned code points we can name.
fn is_other(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
            | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}' | '\u{e000}'..='\u{f8ff}'
            | '\u{f0000}'..='\u{ffffd}' | '\u{100000}'..='\u{10fffd}'
            | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}

/// A display-safe basename: directory components (POSIX or Windows), control
/// and format characters and surrounding whitespace removed. `None` when
/// nothing meaningful remains.
pub fn sanitize_filename(raw: Option<&str>) -> Option<String> {
    let text: String = raw?.chars().filter(|c| !is_other(*c)).collect();
    let text = text.replace('\\', "/");
    // PurePosixPath(...).name, then PureWindowsPath(...).name (drops "C:").
    let posix = text.rsplit('/').next().unwrap_or("");
    let windows = match posix.char_indices().nth(1) {
        Some((i, ':'))
            if posix
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic()) =>
        {
            &posix[i + 1..]
        }
        _ => posix,
    };
    let name = windows.trim();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(name.chars().take(255).collect())
}

fn content_type(part: &mail_parser::MessagePart<'_>) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_lowercase(),
            None => ct.ctype().to_lowercase(),
        },
        None => match &part.body {
            PartType::Message(_) => "message/rfc822".into(),
            _ => "text/plain".into(),
        },
    }
}

/// Enumerates `raw`'s attachments under `settings`' limits. Deterministic.
pub fn extract_attachments(
    raw: &[u8],
    settings: &AttachmentSettings,
) -> Result<AttachmentExtraction, String> {
    let message = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| "unparseable email".to_owned())?;
    let mut out = AttachmentExtraction::default();
    let root = message.root_part();
    if !matches!(root.body, PartType::Multipart(_)) {
        return Ok(out);
    }
    // The readable body leaves (first text/plain, first text/html), as a
    // standard body lookup selects them.
    let mut body_parts: Vec<u32> = Vec::new();
    for id in [message.text_body.first(), message.html_body.first()]
        .into_iter()
        .flatten()
    {
        let disposition_attachment = message
            .part(*id)
            .and_then(|p| p.content_disposition())
            .is_some_and(|d| d.ctype().eq_ignore_ascii_case("attachment"));
        if !disposition_attachment {
            body_parts.push(*id);
        }
    }

    fn leaves(message: &mail_parser::Message<'_>, id: u32, out: &mut Vec<u32>) {
        let Some(part) = message.part(id) else { return };
        match &part.body {
            PartType::Multipart(children) => {
                for c in children {
                    leaves(message, *c, out);
                }
            }
            _ => out.push(id),
        }
    }
    let mut ids = Vec::new();
    leaves(&message, 0, &mut ids);

    let mut total = 0usize;
    let mut ordinal = 0usize;
    for id in ids {
        let Some(part) = message.part(id) else {
            continue;
        };
        let ctype = content_type(part);
        let nested = matches!(part.body, PartType::Message(_)) || ctype == "message/rfc822";
        let filename = sanitize_filename(part.attachment_name());
        let is_attachment = part
            .content_disposition()
            .is_some_and(|d| d.ctype().eq_ignore_ascii_case("attachment"))
            || (!body_parts.contains(&id) && filename.is_some());
        if !nested && !is_attachment {
            continue;
        }
        let current = ordinal;
        ordinal += 1;
        let skip = |reason, size| SkippedAttachment {
            ordinal: current,
            filename: filename.clone(),
            content_type: ctype.clone(),
            size,
            reason,
            detail: None,
        };
        if nested {
            out.skipped.push(skip(SKIP_NESTED_MESSAGE, None));
            continue;
        }
        if out.attachments.len() >= settings.max_count {
            out.skipped.push(skip(SKIP_COUNT_LIMIT, None));
            continue;
        }
        let payload = part.contents().to_vec();
        let size = payload.len();
        if size == 0 {
            out.skipped.push(skip(SKIP_EMPTY, Some(0)));
            continue;
        }
        if size > settings.max_bytes {
            out.skipped.push(skip(SKIP_TOO_LARGE, Some(size)));
            continue;
        }
        if total + size > settings.total_max_bytes {
            out.skipped.push(skip(SKIP_TOTAL_LIMIT, Some(size)));
            continue;
        }
        total += size;
        out.attachments.push(AttachmentPart {
            ordinal: current,
            filename,
            content_type: ctype,
            content_id: part
                .content_id()
                .map(|c| {
                    c.trim()
                        .trim_matches(|c| c == '<' || c == '>')
                        .trim()
                        .to_owned()
                })
                .filter(|c| !c.is_empty()),
            payload,
        });
    }
    Ok(out)
}

/// Conservative MIME fallback when the filename has no extension.
const MIME_TO_EXTENSION: &[(&str, &str)] = &[
    ("application/pdf", ".pdf"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ".docx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ".pptx",
    ),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ".xlsx",
    ),
    ("application/vnd.oasis.opendocument.text", ".odt"),
    ("application/vnd.oasis.opendocument.spreadsheet", ".ods"),
    ("application/vnd.oasis.opendocument.presentation", ".odp"),
    ("application/epub+zip", ".epub"),
    ("text/html", ".html"),
    ("text/markdown", ".md"),
    ("text/csv", ".csv"),
    ("text/plain", ".txt"),
    ("image/png", ".png"),
    ("image/jpeg", ".jpg"),
    ("image/tiff", ".tif"),
];

/// The supported extension for `part`: its filename's suffix first (an
/// explicit unsupported suffix wins), then the MIME fallback.
pub fn attachment_extension(part: &AttachmentPart) -> Option<String> {
    if let Some(name) = &part.filename {
        if let Some(i) = name.rfind('.').filter(|i| *i > 0 && i + 1 < name.len()) {
            let suffix = name[i..].to_lowercase();
            let probe = format!("x{suffix}");
            return crate::format::detect_format(std::path::Path::new(&probe)).map(|_| suffix);
        }
    }
    MIME_TO_EXTENSION
        .iter()
        .find(|(m, _)| m.eq_ignore_ascii_case(&part.content_type))
        .map(|(_, e)| (*e).to_owned())
}
