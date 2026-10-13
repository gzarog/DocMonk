//! Email conversion against fixtures/expected/email.json: parent
//! body, attachment enumeration under default and tight limits, and each
//! attachment's conversion outcome (format, title, chunks / skip / failure).

use std::path::Path;
use std::sync::Arc;

use docmonk_convert::converter::{DoclingConverter, DocumentConverter};
use docmonk_convert::email::{extract_attachments, sanitize_filename, AttachmentSettings};
use docmonk_convert::format::detect_format;
use docmonk_convert::normalize::normalize;
use docmonk_convert::process::DocumentProcessor;
use serde_json::{json, Value};

fn root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn limits(name: &str) -> AttachmentSettings {
    match name {
        "tight" => AttachmentSettings {
            enabled: true,
            max_bytes: 2000,
            max_count: 2,
            total_max_bytes: 3000,
        },
        _ => AttachmentSettings::default(),
    }
}

#[test]
fn emails_convert_as_expected() {
    let g: Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("fixtures/expected/email.json")).unwrap(),
    )
    .unwrap();
    let mut failures = Vec::new();
    for (name, entry) in g.as_object().unwrap() {
        let path = root().join("fixtures/documents").join(name);
        // Parent body.
        let conv = DoclingConverter::default();
        let parent = normalize(
            &conv.convert(&path, detect_format(&path).unwrap()).unwrap(),
            false,
        );
        if serde_json::to_value(&parent).unwrap() != entry["normalized"] {
            failures.push(format!("{name}: parent differs"));
        }
        let raw = std::fs::read(&path).unwrap();
        for lim in ["default", "tight"] {
            let settings = limits(lim);
            let x = extract_attachments(&raw, &settings).unwrap();
            let skipped: Vec<Value> = x
                .skipped
                .iter()
                .map(|s| json!({"ordinal": s.ordinal, "filename": s.filename, "content_type": s.content_type, "size": s.size, "reason": s.reason}))
                .collect();
            if serde_json::to_value(&skipped).unwrap() != entry[lim]["skipped"] {
                failures.push(format!(
                    "{name}/{lim}: skipped {skipped:?} vs {}",
                    entry[lim]["skipped"]
                ));
            }
            let processor = DocumentProcessor {
                converter: Arc::new(DoclingConverter::default()),
                chunking: Default::default(),
                max_pages: Some(1000),
                attachments: Some(settings.clone()),
                image_ocr: false,
            };
            let k = processor.knowledge(&path, "fid", None).unwrap();
            let want = entry[lim]["attachments"].as_array().unwrap();
            if x.attachments.len() != want.len() {
                failures.push(format!(
                    "{name}/{lim}: {} vs {} attachments",
                    x.attachments.len(),
                    want.len()
                ));
                continue;
            }
            for (a, w) in x.attachments.iter().zip(want) {
                let got = json!({"ordinal": a.ordinal, "filename": a.filename, "content_type": a.content_type, "content_id": a.content_id, "size": a.payload.len()});
                for key in ["ordinal", "filename", "content_type", "content_id", "size"] {
                    if got[key] != w[key] {
                        failures.push(format!(
                            "{name}/{lim}#{}: {key} {} vs {}",
                            a.ordinal, got[key], w[key]
                        ));
                    }
                }
                let child = k.documents.iter().find(|d| {
                    d.attachment
                        .as_ref()
                        .is_some_and(|p| p.index == a.ordinal as i64)
                });
                let r = &w["result"];
                match (child, r.get("format")) {
                    (Some(d), Some(fmt)) => {
                        let chunks: Vec<&str> = k
                            .chunks
                            .iter()
                            .filter(|c| c.document_id == d.id)
                            .map(|c| c.text.as_str())
                            .collect();
                        if d.format != fmt.as_str().unwrap()
                            || d.title.as_deref() != r["title"].as_str()
                            || serde_json::to_value(&chunks).unwrap() != r["chunks"]
                        {
                            failures.push(format!(
                                "{name}/{lim}#{}: {} {:?} {chunks:?} vs {r}",
                                a.ordinal, d.format, d.title
                            ));
                        }
                        let p = d.attachment.as_ref().unwrap();
                        assert_eq!(
                            p.parent_document_id,
                            docmonk_core::ids::record::document_id("fid", None)
                        );
                        assert_eq!(p.name.as_deref(), Some(a.display_name().as_str()));
                    }
                    (None, None) => {} // skipped or failed in both
                    (c, _) => failures.push(format!(
                        "{name}/{lim}#{}: rust child {:?} vs {r}",
                        a.ordinal,
                        c.map(|d| &d.format)
                    )),
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn filenames_are_display_only_and_sanitized() {
    assert_eq!(
        sanitize_filename(Some("../../etc/passwd")).as_deref(),
        Some("passwd")
    );
    assert_eq!(
        sanitize_filename(Some("C:\\Users\\x\\report.docx")).as_deref(),
        Some("report.docx")
    );
    assert_eq!(
        sanitize_filename(Some("D:evil.txt")).as_deref(),
        Some("evil.txt")
    );
    assert_eq!(
        sanitize_filename(Some("a\u{0}b\u{202e}c.txt")).as_deref(),
        Some("abc.txt")
    );
    assert_eq!(sanitize_filename(Some("..")), None);
    assert_eq!(sanitize_filename(Some("  ")), None);
    assert_eq!(
        sanitize_filename(Some(&"x".repeat(400)))
            .unwrap()
            .chars()
            .count(),
        255
    );
}

#[test]
fn corrupt_and_non_multipart_emails_never_fail() {
    let s = AttachmentSettings::default();
    assert!(extract_attachments(b"Subject: hi\r\n\r\nbody", &s)
        .unwrap()
        .attachments
        .is_empty());
    let processor = DocumentProcessor {
        converter: Arc::new(DoclingConverter::default()),
        chunking: Default::default(),
        max_pages: None,
        attachments: Some(s),
        image_ocr: false,
    };
    let p = root().join("fixtures/documents/email_with_corrupt_attachment.eml");
    let k = processor.knowledge(&p, "fid", None).unwrap();
    assert_eq!(k.documents.len(), 2, "parent + ok.txt; broken.docx dropped");
}

#[test]
fn nested_messages_and_unsupported_parts_are_skipped_with_stable_ordinals() {
    let raw = b"Subject: outer\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B\r\n\r\n\
--B\r\nContent-Type: text/plain\r\n\r\nbody text\r\n\
--B\r\nContent-Type: message/rfc822\r\nContent-Disposition: attachment; filename=fwd.eml\r\n\r\n\
Subject: inner\r\n\r\ninner body\r\n\
--B\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=blob.bin\r\n\r\nxyz\r\n\
--B\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=empty.txt\r\n\r\n\r\n\
--B\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=\"../../x.txt\"\r\nContent-ID: <cid-1>\r\n\r\nhello\r\n--B--\r\n";
    let x = extract_attachments(raw, &AttachmentSettings::default()).unwrap();
    let skipped: Vec<_> = x.skipped.iter().map(|s| (s.ordinal, s.reason)).collect();
    assert_eq!(skipped, [(0, "nested_message"), (2, "empty")]);
    let kept: Vec<_> = x
        .attachments
        .iter()
        .map(|a| (a.ordinal, a.display_name(), a.content_id.clone()))
        .collect();
    assert_eq!(kept[0].0, 1);
    assert_eq!(kept[1], (3, "x.txt".to_owned(), Some("cid-1".to_owned())));
}
