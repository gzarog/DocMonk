//! Query-driven fact extraction.
//!
//! Interprets one capture-name contract identically for every
//! language: `<kind>.definition`/`<kind>.name` entities (with optional
//! `<kind>.parent_name`), `extends`/`implements` objects (optionally with a
//! `.subject`), `call.expression`/`call.callee`, `import.module` and
//! `decorator.raw`/`decorator.subject`. Containment is structural (nearest
//! already-known ancestor entity) unless a query names the parent.

use std::collections::HashMap;

use docmonk_core::models::{EntityType, RelationshipType};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, QueryCursor};

use crate::lang;

const KIND_BY_PREFIX: &[(&str, EntityType)] = &[
    ("namespace", EntityType::Namespace),
    ("class", EntityType::Class),
    ("interface", EntityType::Interface),
    ("struct", EntityType::Struct),
    ("enum", EntityType::Enum),
    ("function", EntityType::Function),
    ("property", EntityType::Property),
    ("field", EntityType::Field),
];

/// Parents before children; namespace (everyone's fallback) first.
const TIER_ORDER: &[&[&str]] = &[
    &["namespace"],
    &["class", "enum", "interface", "struct"],
    &["field", "function", "property"],
];

const MAX_SIGNATURE_CHARS: usize = 200;

fn is_type_like(kind: EntityType) -> bool {
    matches!(
        kind,
        EntityType::Class | EntityType::Interface | EntityType::Struct | EntityType::Enum
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedEntity {
    pub local_id: usize,
    pub kind: EntityType,
    pub name: String,
    pub qualified_name: String,
    pub parent_local_id: Option<usize>,
    pub signature: Option<String>,
    pub start_line: i64,
    pub end_line: i64,
    pub start_col: i64,
    pub end_col: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedImport {
    pub module: String,
    pub line: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedCall {
    pub caller_local_id: Option<usize>,
    pub callee_text: String,
    pub line: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedInherit {
    pub relationship_type: RelationshipType,
    pub subject_local_id: Option<usize>,
    pub subject_name: Option<String>,
    pub object_name: String,
    pub line: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedDecorator {
    pub subject_local_id: usize,
    pub text: String,
    pub line: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extraction {
    pub entities: Vec<ExtractedEntity>,
    pub imports: Vec<ExtractedImport>,
    pub calls: Vec<ExtractedCall>,
    pub inherits: Vec<ExtractedInherit>,
    pub decorators: Vec<ExtractedDecorator>,
}

impl Extraction {
    pub fn namespace_local_id(&self) -> Option<usize> {
        self.entities
            .iter()
            .position(|e| e.kind == EntityType::Namespace)
    }
}

/// Dotted module path from a project-relative POSIX path
/// (`PurePosixPath.with_suffix("").parts`): `(name, qualified_name)`.
pub fn default_namespace_for_path(rel_posix: &str) -> (String, String) {
    let mut parts: Vec<&str> = rel_posix
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let Some(last) = parts.pop() else {
        return ("root".into(), "root".into());
    };
    let stem = match lang::path_suffix(last) {
        Some(suffix) => &last[..last.len() - suffix.len()],
        None => last,
    };
    parts.push(stem);
    (stem.to_owned(), parts.join("."))
}

fn strip_quotes(text: &str) -> &str {
    let b = text.as_bytes();
    if b.len() >= 2 && b[0] == b[b.len() - 1] && (b[0] == b'\'' || b[0] == b'"') {
        &text[1..text.len() - 1]
    } else {
        text
    }
}

fn decode(source: &[u8], node: Node<'_>) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).into_owned()
}

/// Universal line boundaries: `\n`, `\r\n`, `\r`, `\x0b`, `\x0c`,
/// `\x1c`..`\x1e`, `\x85`, U+2028 and U+2029.
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

fn signature(source: &[u8], node: Node<'_>) -> String {
    let text = decode(source, node);
    let first = text.split(is_line_break).next().unwrap_or("");
    first.chars().take(MAX_SIGNATURE_CHARS).collect()
}

fn nearest_ancestor_entity(node: Node<'_>, known: &HashMap<usize, usize>) -> Option<usize> {
    let mut current = node.parent();
    while let Some(n) = current {
        if let Some(local) = known.get(&n.id()) {
            return Some(*local);
        }
        current = n.parent();
    }
    None
}

type Captures<'t> = Vec<(String, Vec<Node<'t>>)>;

fn get<'a, 't>(captures: &'a Captures<'t>, name: &str) -> Option<&'a [Node<'t>]> {
    captures
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, nodes)| nodes.as_slice())
        .filter(|nodes| !nodes.is_empty())
}

/// Extracts facts; `None` for an unsupported language.
pub fn extract(
    source: &[u8],
    language: &str,
    default_namespace_name: &str,
    default_namespace_qualified_name: &str,
) -> Option<Extraction> {
    let tree = lang::parse(source, language)?;
    let query = lang::query(language)?;
    Some(extract_tree(
        source,
        &tree,
        query,
        default_namespace_name,
        default_namespace_qualified_name,
    ))
}

pub(crate) fn extract_tree(
    source: &[u8],
    tree: &tree_sitter::Tree,
    query: &tree_sitter::Query,
    default_namespace_name: &str,
    default_namespace_qualified_name: &str,
) -> Extraction {
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches: Vec<Captures<'_>> = Vec::new();
    let mut it = cursor.matches(query, tree.root_node(), source);
    while let Some(m) = it.next() {
        // Capture name -> nodes, in first-appearance order.
        let mut caps: Captures<'_> = Vec::new();
        for c in m.captures {
            let name = names[c.index as usize];
            match caps.iter_mut().find(|(n, _)| n == name) {
                Some((_, nodes)) => nodes.push(c.node),
                None => caps.push((name.to_owned(), vec![c.node])),
            }
        }
        matches.push(caps);
    }

    // kind prefix -> (node id -> match index), first match wins.
    let mut raw_by_kind: HashMap<&str, Vec<(usize, Node<'_>, usize)>> = HashMap::new();
    for (mi, caps) in matches.iter().enumerate() {
        for (name, nodes) in caps {
            let Some(prefix) = name.strip_suffix(".definition") else {
                continue;
            };
            let Some((prefix, _)) = KIND_BY_PREFIX.iter().find(|(p, _)| *p == prefix) else {
                continue;
            };
            let bucket = raw_by_kind.entry(prefix).or_default();
            for node in nodes {
                if !bucket.iter().any(|(id, _, _)| *id == node.id()) {
                    bucket.push((node.id(), *node, mi));
                }
            }
        }
    }

    let mut entities: Vec<ExtractedEntity> = Vec::new();
    let mut known: HashMap<usize, usize> = HashMap::new();
    let mut namespace_local_id: Option<usize> = None;

    if raw_by_kind.get("namespace").is_none_or(|v| v.is_empty()) {
        namespace_local_id = Some(0);
        let line_count = source.iter().filter(|b| **b == b'\n').count() as i64 + 1;
        entities.push(ExtractedEntity {
            local_id: 0,
            kind: EntityType::Namespace,
            name: default_namespace_name.to_owned(),
            qualified_name: default_namespace_qualified_name.to_owned(),
            parent_local_id: None,
            signature: None,
            start_line: 1,
            end_line: line_count.max(1),
            start_col: 0,
            end_col: 0,
        });
    }

    for tier in TIER_ORDER {
        let mut pending: Vec<(&str, Node<'_>, usize)> = Vec::new();
        for prefix in *tier {
            for (_, node, mi) in raw_by_kind.get(prefix).into_iter().flatten() {
                pending.push((prefix, *node, *mi));
            }
        }
        // Stable sort by start byte, so ties keep match order.
        pending.sort_by_key(|(_, node, _)| node.start_byte());

        for (prefix, def_node, mi) in pending {
            let caps = &matches[mi];
            let Some(name_nodes) = get(caps, &format!("{prefix}.name")) else {
                continue;
            };
            let name = decode(source, name_nodes[0]);
            let mut kind = KIND_BY_PREFIX
                .iter()
                .find(|(p, _)| *p == prefix)
                .map(|(_, k)| *k)
                .expect("known prefix");
            let mut parent = match get(caps, &format!("{prefix}.parent_name")) {
                Some(parent_nodes) => {
                    let parent_name = decode(source, parent_nodes[0]);
                    entities
                        .iter()
                        .find(|e| is_type_like(e.kind) && e.name == parent_name)
                        .map(|e| e.local_id)
                }
                None => nearest_ancestor_entity(def_node, &known),
            };
            if parent.is_none() {
                parent = namespace_local_id;
            }
            if kind == EntityType::Function {
                if let Some(p) = parent {
                    if is_type_like(entities[p].kind) {
                        kind = EntityType::Method;
                    }
                }
            }
            let qualified_name = match parent.map(|p| entities[p].qualified_name.as_str()) {
                Some(pq) if !pq.is_empty() => format!("{pq}.{name}"),
                _ => name.clone(),
            };
            let local_id = entities.len();
            let start = def_node.start_position();
            let end = def_node.end_position();
            entities.push(ExtractedEntity {
                local_id,
                kind,
                name,
                qualified_name,
                parent_local_id: parent,
                signature: Some(signature(source, def_node)),
                start_line: start.row as i64 + 1,
                end_line: end.row as i64 + 1,
                start_col: start.column as i64,
                end_col: end.column as i64,
            });
            known.insert(def_node.id(), local_id);
            if prefix == "namespace" && namespace_local_id.is_none() {
                namespace_local_id = Some(local_id);
            }
        }
    }

    let mut out = Extraction {
        entities,
        ..Extraction::default()
    };

    let line_of = |n: Node<'_>| n.start_position().row as i64 + 1;
    for caps in &matches {
        if let Some(module) = get(caps, "import.module") {
            out.imports.push(ExtractedImport {
                module: strip_quotes(&decode(source, module[0])).to_owned(),
                line: line_of(module[0]),
            });
        }
        if let (Some(callee), Some(call)) = (get(caps, "call.callee"), get(caps, "call.expression"))
        {
            out.calls.push(ExtractedCall {
                caller_local_id: nearest_ancestor_entity(call[0], &known),
                callee_text: decode(source, callee[0]),
                line: line_of(call[0]),
            });
        }
        for (verb, rel_type) in [
            ("extends", RelationshipType::Extends),
            ("implements", RelationshipType::Implements),
        ] {
            let Some(object) = get(caps, &format!("{verb}.object")) else {
                continue;
            };
            let object = object[0];
            match get(caps, &format!("{verb}.subject")) {
                Some(subject) => out.inherits.push(ExtractedInherit {
                    relationship_type: rel_type,
                    subject_local_id: None,
                    subject_name: Some(decode(source, subject[0])),
                    object_name: decode(source, object),
                    line: line_of(object),
                }),
                None => {
                    if let Some(subject) = nearest_ancestor_entity(object, &known) {
                        out.inherits.push(ExtractedInherit {
                            relationship_type: rel_type,
                            subject_local_id: Some(subject),
                            subject_name: None,
                            object_name: decode(source, object),
                            line: line_of(object),
                        });
                    }
                }
            }
        }
        if let (Some(raw), Some(subject)) =
            (get(caps, "decorator.raw"), get(caps, "decorator.subject"))
        {
            if let Some(subject_local_id) = known.get(&subject[0].id()) {
                out.decorators.push(ExtractedDecorator {
                    subject_local_id: *subject_local_id,
                    text: decode(source, raw[0]),
                    line: line_of(raw[0]),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_namespace_matches_pathlib() {
        assert_eq!(
            default_namespace_for_path("python/pkg/service.py"),
            ("service".into(), "python.pkg.service".into())
        );
        assert_eq!(
            default_namespace_for_path("a/b.tar.gz"),
            ("b.tar".into(), "a.b.tar".into())
        );
        assert_eq!(
            default_namespace_for_path("Makefile"),
            ("Makefile".into(), "Makefile".into())
        );
        assert_eq!(
            default_namespace_for_path(""),
            ("root".into(), "root".into())
        );
    }

    #[test]
    fn signature_is_first_python_line_capped() {
        assert_eq!(strip_quotes("\"x\""), "x");
        assert_eq!(strip_quotes("'x"), "'x");
    }
}
