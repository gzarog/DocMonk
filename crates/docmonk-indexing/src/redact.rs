//! Applies content redaction (`docmonk_core::secrets`, ADR 0042) to
//! everything a processor extracted from one file, before it is stored or
//! embedded: chunk and passage text, the texts they are searched and
//! embedded by, entity signatures, relationship evidence and document
//! metadata. Line structure is preserved, so spans and citations stay
//! valid.

use docmonk_core::secrets::redact_in_place;
use docmonk_storage::knowledge::{FileKnowledge, RelationshipRow};

fn opt(text: &mut Option<String>) {
    if let Some(t) = text {
        redact_in_place(t);
    }
}

pub fn redact_knowledge(k: &mut FileKnowledge) {
    for e in &mut k.entities {
        opt(&mut e.signature);
    }
    redact_relationships(&mut k.relationships);
    for d in &mut k.documents {
        opt(&mut d.title);
        opt(&mut d.author);
    }
    for c in &mut k.chunks {
        redact_in_place(&mut c.text);
        redact_in_place(&mut c.search_text);
        opt(&mut c.embedding_text);
        opt(&mut c.caption);
        c.heading_path.iter_mut().for_each(redact_in_place);
        if let Some(rows) = &mut c.table_rows {
            rows.iter_mut().flatten().for_each(redact_in_place);
        }
    }
    for p in &mut k.passages {
        redact_in_place(&mut p.header);
        redact_in_place(&mut p.text);
    }
}

/// Relationship evidence quotes source text (string literals, decorator
/// arguments); it is redacted wherever relationships are derived.
pub fn redact_relationships(rows: &mut [RelationshipRow]) {
    for r in rows {
        opt(&mut r.evidence);
        opt(&mut r.reference_text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docmonk_storage::knowledge::{ChunkRow, PassageRow};

    #[test]
    fn every_stored_text_is_redacted() {
        let secret = "DB_PASSWORD=s3cr3tvalue";
        let mut k = FileKnowledge {
            chunks: vec![ChunkRow {
                text: format!("intro\n{secret}"),
                search_text: format!("Title\n{secret}"),
                embedding_text: Some(secret.into()),
                table_rows: Some(vec![vec![secret.into()]]),
                ..ChunkRow::default()
            }],
            passages: vec![PassageRow {
                text: format!("a\n{secret}\nb"),
                ..PassageRow::default()
            }],
            relationships: vec![RelationshipRow {
                evidence: Some(r#"api_key = "abcd1234efgh""#.into()),
                ..RelationshipRow::default()
            }],
            ..FileKnowledge::default()
        };
        redact_knowledge(&mut k);
        let dump = format!("{k:?}");
        assert!(!dump.contains("s3cr3tvalue"), "{dump}");
        assert!(!dump.contains("abcd1234efgh"), "{dump}");
        assert_eq!(k.passages[0].text.lines().count(), 3);
        assert!(k.chunks[0]
            .text
            .ends_with("DB_PASSWORD=[REDACTED:assignment]"));
    }
}
