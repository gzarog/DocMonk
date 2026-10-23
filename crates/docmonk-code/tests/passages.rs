//! Implementation passages per language (ADR 0037): bodies with their
//! documentation comments, nested declarations, huge functions split at
//! statements, and the plain-text fallback for malformed and unsupported
//! source.

use docmonk_code::passages::{MAX_PASSAGES_PER_FILE, MAX_PASSAGE_CHARS, MAX_PASSAGE_LINES};
use docmonk_code::process::{degraded_knowledge, file_knowledge, prepare_source};
use docmonk_storage::knowledge::{FileKnowledge, PassageRow};

fn knowledge(rel: &str, src: &str) -> FileKnowledge {
    match prepare_source(src.as_bytes(), rel) {
        Ok(p) => file_knowledge("f", rel, src.as_bytes(), &p),
        Err(f) => degraded_knowledge("f", rel, src.as_bytes(), &f),
    }
}

/// The passage owned by the entity named `name`.
fn owned<'a>(k: &'a FileKnowledge, name: &str) -> Vec<&'a PassageRow> {
    let id = &k
        .entities
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no entity {name}: {:?}", k.entities))
        .id;
    k.passages
        .iter()
        .filter(|p| p.entity_id.as_ref() == Some(id))
        .collect()
}

fn covers(p: &PassageRow, line: i64) -> bool {
    p.start_line <= line && line <= p.end_line
}

#[test]
fn python_bodies_carry_docstrings_comments_and_nested_functions() {
    let src = "\
import os

# Computes the fee owed.
@cached
def late_fee(invoice):
    \"\"\"Fees never exceed a quarter of the total.\"\"\"
    cap = invoice.total * 0.25

    def helper(x):
        return x * 2

    return min(helper(1), cap)


class Renderer:
    def render(self, doc):
        return to_pdf(doc, dpi=300)
";
    let k = knowledge("billing.py", src);
    let fee = owned(&k, "late_fee");
    assert_eq!(fee.len(), 1);
    let p = fee[0];
    assert_eq!((p.start_line, p.end_line), (3, 12), "{p:?}");
    assert!(p
        .text
        .starts_with("# Computes the fee owed.\n@cached\ndef late_fee"));
    assert!(p.text.contains("a quarter of the total"));
    assert!(p
        .header
        .starts_with("billing.late_fee def late_fee(invoice):"));
    assert_eq!((p.kind.as_str(), p.extraction.as_str()), ("body", "parsed"));
    // The nested function has its own passage inside the outer one.
    let helper = owned(&k, "helper");
    assert_eq!(helper.len(), 1);
    assert!(covers(p, helper[0].start_line) && covers(p, helper[0].end_line));
    let render = owned(&k, "render");
    assert!(render[0].text.contains("dpi=300"));
    assert!(render[0].header.starts_with("billing.Renderer.render "));
    // Classes and the module are entities, not passage owners.
    assert!(owned(&k, "Renderer").is_empty());
    // Passage ordinals are the file order.
    let ords: Vec<i64> = k.passages.iter().map(|p| p.ordinal).collect();
    assert_eq!(ords, (0..k.passages.len() as i64).collect::<Vec<_>>());
}

#[test]
fn typescript_csharp_and_rust_bodies_include_doc_comments() {
    let ts = "\
/**
 * Applies a coupon.
 */
export function applyCoupon(cart: Cart): Cart {
  if (total > MAX) {
    throw new Error(\"too much\");
  }
  return cart;
}
";
    let k = knowledge("cart.ts", ts);
    let p = owned(&k, "applyCoupon")[0];
    assert_eq!((p.start_line, p.end_line), (1, 9));
    assert!(p.text.contains("Applies a coupon.") && p.text.contains("too much"));

    let cs = "\
namespace Shop
{
    public class RefundService
    {
        /// <summary>Issues a refund.</summary>
        [HttpPost]
        public Refund Issue(Order order)
        {
            request.Headers.Add(\"X-Idempotency-Key\", order.Id);
            return null;
        }
    }
}
";
    let k = knowledge("RefundService.cs", cs);
    let p = owned(&k, "Issue")[0];
    assert_eq!(p.start_line, 5, "{p:?}");
    assert!(covers(p, 9));
    assert!(p.text.contains("X-Idempotency-Key"));

    let rs = "\
/// Pairs bank lines with entries.
#[must_use]
pub fn match_lines(a: &[i64]) -> usize {
    a.iter().filter(|x| x.abs() <= 1).count()
}

impl M {
    /// Exact cents score double.
    fn score(&self) -> f64 {
        2.0
    }
}
";
    let k = knowledge("reconcile.rs", rs);
    let p = owned(&k, "match_lines")[0];
    assert_eq!((p.start_line, p.end_line), (1, 5));
    let s = owned(&k, "score")[0];
    assert_eq!((s.start_line, s.end_line), (8, 11));
    assert!(s.text.contains("Exact cents score double."));
}

#[test]
fn huge_functions_split_at_statements_and_stay_bounded() {
    let mut src = String::from("def reconcile(entries):\n    \"\"\"Match entries.\"\"\"\n");
    for i in 0..150 {
        src.push_str(&format!("    total_{i} = entries[{i}] * 2\n"));
    }
    src.push_str("    if currency == 'JPY':\n        amount = round(amount)\n");
    src.push_str("    return total_0\n");
    let k = knowledge("big.py", &src);
    let parts = owned(&k, "reconcile");
    assert!(parts.len() >= 4, "{}", parts.len());
    for (i, p) in parts.iter().enumerate() {
        assert_eq!(p.part, i as i64);
        assert_eq!(p.parts, parts.len() as i64);
        assert!((p.end_line - p.start_line + 1) as usize <= MAX_PASSAGE_LINES);
        assert!(p.text.chars().count() <= MAX_PASSAGE_CHARS);
        assert!(p
            .header
            .starts_with("big.reconcile def reconcile(entries):"));
    }
    // Statement-aligned parts tile the body: no gaps, no overlap.
    assert_eq!(parts[0].start_line, 1);
    for w in parts.windows(2) {
        assert_eq!(w[0].end_line + 1, w[1].start_line);
    }
    let jpy = 153;
    assert!(parts
        .iter()
        .any(|p| covers(p, jpy) && p.text.contains("JPY")));
    assert_eq!(parts.last().unwrap().end_line, 155);
}

#[test]
fn malformed_and_unsupported_files_are_text_without_symbols() {
    let broken = "def export(q):\n    upload(\"sftp-host\", rows\n    return 1\n";
    let k = knowledge("tax.py", broken);
    assert!(k.entities.is_empty());
    let notice = k.notice.as_ref().expect("a degraded notice");
    assert_eq!(notice.code, "parse_degraded");
    assert!(notice.message.contains("syntax error"), "{notice:?}");
    assert_eq!(k.passages.len(), 1);
    let p = &k.passages[0];
    assert_eq!(
        (
            p.kind.as_str(),
            p.extraction.as_str(),
            p.language.as_deref()
        ),
        ("text", "parse_failed", Some("python"))
    );
    assert!(p.entity_id.is_none() && p.text.contains("sftp-host"));
    assert_eq!(p.header, "tax.py");

    let rb = "def purge(redis)\n  redis.scan_each(match: \"sess:*\")\nend\n";
    let k = knowledge("scripts/cleanup.rb", rb);
    assert!(k.notice.is_none() && k.entities.is_empty());
    assert_eq!(k.passages[0].extraction, "unsupported_language");
    assert_eq!((k.passages[0].start_line, k.passages[0].end_line), (1, 3));

    // A generated file never yields more than the per-file cap.
    let huge: String = (0..50_000).map(|i| format!("SELECT {i};\n")).collect();
    assert_eq!(
        knowledge("dump.sql", &huge).passages.len(),
        MAX_PASSAGES_PER_FILE
    );
    // An empty file has nothing to index.
    assert!(knowledge("empty.rb", "").passages.is_empty());
}

#[test]
fn passage_ids_are_stable_for_the_same_bytes() {
    let src = "def a():\n    return 1\n\ndef b():\n    return 2\n";
    let one = knowledge("x.py", src);
    let two = knowledge("x.py", src);
    assert_eq!(one.passages, two.passages);
    let ids: std::collections::HashSet<&String> = one.passages.iter().map(|p| &p.id).collect();
    assert_eq!(ids.len(), one.passages.len());
}
