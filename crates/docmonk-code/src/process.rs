//! The code processor for the indexer, and per-file graph derivation.
//!
//! `prepare` runs on worker threads with no storage access: parse, reject
//! files with syntax errors (isolated per-file failures reported as
//! `CodeParseError`), extract and assign stable entity IDs. The primary
//! index carries entities only: relationships are never derived while
//! indexing. The post-index graph stage ([`crate::graph_stage`]) derives
//! each file's relationships with [`file_relationships`] (references that
//! need other files are stored as `pending` with their raw text) and then
//! resolves them against the complete published build with
//! [`resolve_build`], so the outcome never depends on processing order.

use std::collections::HashMap;
use std::path::Path;

use docmonk_core::ids::record;
use docmonk_core::models::{Confidence, RelationshipType};
use docmonk_indexing::coordinator::{PrepareInput, ProcessError, Processor};
use docmonk_storage::knowledge::{
    EntityRow, FileKnowledge, ProjectStore, RelationshipRow, Resolution,
};

use crate::extract::{default_namespace_for_path, extract_tree, Extraction};
use crate::framework;
use crate::lang;
use crate::resolve::{self, Candidate, ResolvedTarget};

/// Derivation identity of code extraction; part of `parser_version`, so a
/// change forces a full rebuild of every source. (2: an entity without a
/// signature is indexed in code FTS under its name.)
pub const CODE_DERIVATION_VERSION: &str = "rust-code-2";

/// Derivation identity of the relationship graph (per-file edges, cross-file
/// resolution and linking). Independent of the base index identity: a
/// change recomputes graphs only, never reindexes content.
pub const GRAPH_DERIVATION_VERSION: &str = "graph-1";

/// The full graph identity recorded with every graph generation (it
/// includes the code derivation, whose entity ids the graph points at).
pub fn graph_derivation() -> String {
    format!("{CODE_DERIVATION_VERSION}+{GRAPH_DERIVATION_VERSION}")
}

/// Resolver label of a reference awaiting whole-build resolution.
pub const PENDING: &str = "pending";

/// Outcome of preparing one code file.
#[derive(Debug)]
pub enum PreparedCode {
    /// A code extension without a grammar (`.rb`, `.sql`, ...): indexed
    /// with no entities.
    Unsupported,
    Parsed {
        language: &'static str,
        extraction: Extraction,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailure {
    pub language: &'static str,
}

/// Parses and extracts; `Err` when the tree contains syntax errors.
pub fn prepare_source(source: &[u8], rel_path: &str) -> Result<PreparedCode, ParseFailure> {
    let Some(language) = lang::detect_language(Path::new(rel_path)) else {
        return Ok(PreparedCode::Unsupported);
    };
    let (Some(tree), Some(query)) = (lang::parse(source, language), lang::query(language)) else {
        return Ok(PreparedCode::Unsupported);
    };
    if tree.root_node().has_error() {
        return Err(ParseFailure { language });
    }
    let (name, qualified) = default_namespace_for_path(rel_path);
    let extraction = extract_tree(source, &tree, query, &name, &qualified);
    Ok(PreparedCode::Parsed {
        language,
        extraction,
    })
}

/// `'@app.route("/x")'` -> `'app.route'`.
pub fn decorator_head(text: &str) -> String {
    let stripped = text.trim_start_matches('@').trim();
    stripped.split('(').next().unwrap_or("").trim().to_owned()
}

/// Stable entity IDs for an extraction, in local-id order.
pub fn entity_ids(file_id: &str, extraction: &Extraction) -> Vec<String> {
    let mut seen: HashMap<(String, String), u64> = HashMap::new();
    extraction
        .entities
        .iter()
        .map(|e| {
            let n = seen
                .entry((e.kind.as_str().to_owned(), e.qualified_name.clone()))
                .or_insert(0);
            let id = record::entity_id(file_id, e.kind.as_str(), &e.qualified_name, *n);
            *n += 1;
            id
        })
        .collect()
}

pub fn entity_rows(
    file_id: &str,
    language: &str,
    extraction: &Extraction,
    ids: &[String],
) -> Vec<EntityRow> {
    extraction
        .entities
        .iter()
        .map(|e| EntityRow {
            id: ids[e.local_id].clone(),
            file_id: file_id.to_owned(),
            kind: e.kind.as_str().to_owned(),
            name: e.name.clone(),
            qualified_name: e.qualified_name.clone(),
            language: language.to_owned(),
            parent_id: e.parent_local_id.map(|p| ids[p].clone()),
            signature: e.signature.clone(),
            start_line: e.start_line,
            end_line: e.end_line,
            start_col: e.start_col,
            end_col: e.end_col,
        })
        .collect()
}

/// Builds the file's relationships in a stable, documented order; `resolve`
/// maps reference text to a target.
pub fn build_relationships(
    rel_path: &str,
    file_id: &str,
    language: &str,
    extraction: &Extraction,
    ids: &[String],
    resolve: &mut dyn FnMut(&str) -> ResolvedTarget,
) -> Vec<RelationshipRow> {
    let mut out = Vec::new();
    let mut ordinals: HashMap<(String, &'static str, String), u64> = HashMap::new();
    let mut push = |out: &mut Vec<RelationshipRow>,
                    rel_type: RelationshipType,
                    source: &str,
                    key: &str,
                    row: RelationshipRow| {
        let n = ordinals
            .entry((source.to_owned(), rel_type.as_str(), key.to_owned()))
            .or_insert(0);
        let id = record::relationship_id(source, rel_type.as_str(), key, *n);
        *n += 1;
        out.push(RelationshipRow { id, ..row });
    };
    let location = |line: i64| Some(format!("{rel_path}:{line}"));

    for e in &extraction.entities {
        let Some(parent) = e.parent_local_id else {
            continue;
        };
        let (child, parent) = (&ids[e.local_id], &ids[parent]);
        for (rel_type, source, target) in [
            (RelationshipType::Contains, parent, child),
            (RelationshipType::DefinedIn, child, parent),
        ] {
            push(
                &mut out,
                rel_type,
                source,
                target,
                RelationshipRow {
                    file_id: file_id.to_owned(),
                    relationship_type: rel_type.as_str().to_owned(),
                    source_entity_id: source.clone(),
                    target_entity_id: Some(target.clone()),
                    resolver: "structural".into(),
                    confidence: Confidence::Exact.as_str().into(),
                    source_location: location(e.start_line),
                    ..RelationshipRow::default()
                },
            );
        }
    }

    let mut reference = |out: &mut Vec<RelationshipRow>,
                         rel_type: RelationshipType,
                         source: &str,
                         text: &str,
                         line: i64,
                         evidence: &str| {
        let target = resolve(text);
        push(
            out,
            rel_type,
            source,
            text,
            RelationshipRow {
                file_id: file_id.to_owned(),
                relationship_type: rel_type.as_str().to_owned(),
                source_entity_id: source.to_owned(),
                target_symbol: target.entity_id.is_none().then(|| target.symbol.clone()),
                target_entity_id: target.entity_id,
                resolver: target.resolver.to_owned(),
                confidence: target.confidence.as_str().to_owned(),
                source_location: location(line),
                evidence: Some(evidence.to_owned()),
                reference_text: Some(text.to_owned()),
                ..RelationshipRow::default()
            },
        );
    };

    let Some(namespace) = extraction.namespace_local_id().map(|i| ids[i].clone()) else {
        return out;
    };
    for imp in &extraction.imports {
        reference(
            &mut out,
            RelationshipType::Imports,
            &namespace,
            &imp.module,
            imp.line,
            &imp.module,
        );
    }
    for call in &extraction.calls {
        let caller = call
            .caller_local_id
            .map_or(namespace.as_str(), |c| ids[c].as_str());
        reference(
            &mut out,
            RelationshipType::Calls,
            caller,
            &call.callee_text,
            call.line,
            &call.callee_text,
        );
    }
    for inherit in &extraction.inherits {
        let subject = match (&inherit.subject_local_id, &inherit.subject_name) {
            (Some(local), _) => Some(ids[*local].as_str()),
            (None, Some(name)) => extraction
                .entities
                .iter()
                .find(|e| &e.name == name)
                .map(|e| ids[e.local_id].as_str()),
            (None, None) => None,
        };
        let Some(subject) = subject else {
            continue;
        };
        reference(
            &mut out,
            inherit.relationship_type,
            subject,
            &inherit.object_name,
            inherit.line,
            &inherit.object_name,
        );
    }
    for dec in &extraction.decorators {
        reference(
            &mut out,
            RelationshipType::References,
            &ids[dec.subject_local_id],
            &decorator_head(&dec.text),
            dec.line,
            &dec.text,
        );
    }
    for finding in framework::detect(language, &extraction.decorators) {
        let source = &ids[finding.subject_local_id];
        push(
            &mut out,
            RelationshipType::References,
            source,
            &format!("{}#{}", finding.resolver, finding.target_symbol),
            RelationshipRow {
                file_id: file_id.to_owned(),
                relationship_type: RelationshipType::References.as_str().into(),
                source_entity_id: source.clone(),
                target_symbol: Some(finding.target_symbol),
                resolver: finding.resolver.into(),
                confidence: Confidence::Heuristic.as_str().into(),
                source_location: Some(rel_path.to_owned()),
                evidence: Some(finding.evidence),
                ..RelationshipRow::default()
            },
        );
    }
    out
}

pub fn candidates<'a>(rows: &'a [EntityRow]) -> Vec<Candidate<'a>> {
    rows.iter()
        .map(|e| Candidate {
            id: &e.id,
            file_id: &e.file_id,
            name: &e.name,
            qualified_name: &e.qualified_name,
            start_line: e.start_line,
        })
        .collect()
}

/// Same-file resolution; anything else is left pending.
fn resolve_locally(text: &str, same_file: &[Candidate<'_>]) -> ResolvedTarget {
    resolve::resolve_same_file(text, same_file).unwrap_or_else(|| ResolvedTarget {
        entity_id: None,
        symbol: resolve::bare_name(text).to_owned(),
        confidence: Confidence::Medium,
        resolver: PENDING,
    })
}

/// Pure per-file knowledge of the primary index: entities only.
pub fn file_knowledge(file_id: &str, prepared: &PreparedCode) -> FileKnowledge {
    let PreparedCode::Parsed {
        language,
        extraction,
    } = prepared
    else {
        return FileKnowledge::default();
    };
    let ids = entity_ids(file_id, extraction);
    FileKnowledge {
        entities: entity_rows(file_id, language, extraction, &ids),
        ..FileKnowledge::default()
    }
}

/// A file's relationships (structural edges, references resolved within
/// the file, the rest `pending`), derived by the graph stage from the
/// exact bytes the base index published.
pub fn file_relationships(
    rel_path: &str,
    file_id: &str,
    source: &[u8],
) -> Result<Vec<RelationshipRow>, ParseFailure> {
    let PreparedCode::Parsed {
        language,
        extraction,
    } = prepare_source(source, rel_path)?
    else {
        return Ok(Vec::new());
    };
    let ids = entity_ids(file_id, &extraction);
    let entities = entity_rows(file_id, language, &extraction, &ids);
    let same_file = candidates(&entities);
    Ok(build_relationships(
        rel_path,
        file_id,
        language,
        &extraction,
        &ids,
        &mut |t| resolve_locally(t, &same_file),
    ))
}

/// The registered `FileKind::Code` processor (entities only, whatever the
/// relationship setting).
#[derive(Debug, Default)]
pub struct CodeProcessor;

impl Processor for CodeProcessor {
    fn prepare(&self, input: &PrepareInput) -> Result<FileKnowledge, ProcessError> {
        let source = std::fs::read(&input.path).map_err(|e| ProcessError {
            code: "io_error".into(),
            message: e.to_string(),
            transient: true,
        })?;
        let prepared = prepare_source(&source, &input.rel_path).map_err(|f| ProcessError {
            code: "parse_error".into(),
            message: format!(
                "{}: syntax error(s) in a {} file",
                input.rel_path, f.language
            ),
            transient: false,
        })?;
        Ok(file_knowledge(&input.file_id, &prepared))
    }
}

/// Re-resolves cross-file references; returns the number of rows changed.
pub fn resolve_build(
    store: &mut ProjectStore,
    build_id: &str,
) -> Result<usize, docmonk_storage::StorageError> {
    let entities = store.all_entities(build_id)?;
    let all = candidates(&entities);
    let mut by_qn: HashMap<&str, Vec<Candidate<'_>>> = HashMap::new();
    let mut by_name: HashMap<&str, Vec<Candidate<'_>>> = HashMap::new();
    for c in &all {
        by_qn.entry(c.qualified_name).or_default().push(*c);
        by_name.entry(c.name).or_default().push(*c);
    }
    // Presort once in resolution order so each reference only needs the
    // first (qualified: first two) candidates outside its own file.
    for v in by_qn.values_mut() {
        v.sort_by(|a, b| (a.file_id, a.start_line).cmp(&(b.file_id, b.start_line)));
    }
    for v in by_name.values_mut() {
        v.sort_by(|a, b| (a.qualified_name, a.file_id).cmp(&(b.qualified_name, b.file_id)));
    }
    fn lookup<'a>(
        map: &HashMap<&str, Vec<Candidate<'a>>>,
        key: &str,
        file: &str,
        take: usize,
    ) -> Vec<Candidate<'a>> {
        map.get(key)
            .map(|v| {
                v.iter()
                    .filter(|c| c.file_id != file)
                    .take(take)
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }
    let mut outcomes = Vec::new();
    for r in store.cross_file_references(build_id)? {
        let Some(text) = r.reference_text.as_deref() else {
            continue;
        };
        let t = resolve::resolve_cross_file(
            text,
            &lookup(&by_qn, text, &r.file_id, 2),
            &lookup(&by_name, resolve::bare_name(text), &r.file_id, 1),
        );
        let target_symbol = t.entity_id.is_none().then(|| t.symbol.clone());
        if r.target_entity_id == t.entity_id
            && r.target_symbol == target_symbol
            && r.resolver == t.resolver
            && r.confidence == t.confidence.as_str()
        {
            continue;
        }
        outcomes.push(Resolution {
            id: r.id,
            target_entity_id: t.entity_id,
            target_symbol,
            resolver: t.resolver.into(),
            confidence: t.confidence.as_str().into(),
        });
    }
    store.apply_resolutions(build_id, &outcomes)
}

/// Re-resolves only the cross-file references recorded by `files`, looking
/// candidates up by symbol (indexed queries) instead of loading the whole
/// build. Produces exactly what [`resolve_build`] would for those rows.
/// Returns the number of rows changed.
pub fn resolve_files(
    store: &mut ProjectStore,
    build_id: &str,
    files: &std::collections::BTreeSet<String>,
) -> Result<usize, docmonk_storage::StorageError> {
    let mut by_qn: HashMap<String, Vec<EntityRow>> = HashMap::new();
    let mut by_name: HashMap<String, Vec<EntityRow>> = HashMap::new();
    let mut outcomes = Vec::new();
    for file in files {
        for r in store.file_cross_file_references(build_id, file)? {
            let Some(text) = r.reference_text.as_deref() else {
                continue;
            };
            let bare = resolve::bare_name(text);
            if !by_qn.contains_key(text) {
                let rows = store.entities_by_qualified_name(build_id, text)?;
                by_qn.insert(text.to_owned(), rows);
            }
            if !by_name.contains_key(bare) {
                let rows = store.entities_by_bare_name(build_id, bare)?;
                by_name.insert(bare.to_owned(), rows);
            }
            let pick = |rows: &[EntityRow], take: usize| -> Vec<EntityRow> {
                rows.iter()
                    .filter(|e| e.file_id != r.file_id)
                    .take(take)
                    .cloned()
                    .collect()
            };
            let qn_rows = pick(&by_qn[text], 2);
            let name_rows = pick(&by_name[bare], 1);
            let t =
                resolve::resolve_cross_file(text, &candidates(&qn_rows), &candidates(&name_rows));
            let target_symbol = t.entity_id.is_none().then(|| t.symbol.clone());
            if r.target_entity_id == t.entity_id
                && r.target_symbol == target_symbol
                && r.resolver == t.resolver
                && r.confidence == t.confidence.as_str()
            {
                continue;
            }
            outcomes.push(Resolution {
                id: r.id,
                target_entity_id: t.entity_id,
                target_symbol,
                resolver: t.resolver.into(),
                confidence: t.confidence.as_str().into(),
            });
        }
    }
    store.apply_resolutions(build_id, &outcomes)
}
