//! Cross-domain linker (`link_touched_files`):
//! code entities <-> document chunks.
//!
//! | Resolver | Confidence | Link type | Signal |
//! |---|---|---|---|
//! | `linker:exact_identifier` | high | `mentioned_in` | bare name |
//! | `linker:qualified_identifier` | high | `documented_by` | dotted qualified name |
//! | `linker:alias` | medium | `mentioned_in` | final two segments (3+ segment names) |
//! | `linker:filename` | medium | `mentioned_in` | source filename/stem, on the file's namespace entity |
//! | `linker:route_heuristic` | heuristic | `related_to` | framework route path, plain substring |
//!
//! `exact` is reserved for manual links (`resolver = "user"`); this module
//! cannot produce it. Scoping: touched code files
//! are matched against every unit, touched documents against every entity
//! of code files not already covered, so cost tracks what changed.

use std::collections::{HashMap, HashSet};

use docmonk_core::ids::record;
use docmonk_core::models::{Confidence, EntityType, RelationshipType};
use docmonk_documents::table::render_table;
use docmonk_storage::knowledge::{EntityRow, LinkRow, LinkUnitRow, ProjectStore, RelationshipRow};
use docmonk_storage::StorageError;

use crate::matcher::UnitIndex;

pub const ROUTE_TARGET_PREFIX: &str = "http_endpoint:";
const MIN_ALIAS_SEGMENTS: usize = 3;
/// Bounds how many links are written per transaction.
pub const LINK_BATCH_SIZE: usize = 5000;

/// One chunk as matched text (tables are their row-aware rendering).
#[derive(Debug, Clone)]
pub struct Unit {
    pub id: String,
    pub document_id: String,
    pub file_id: String,
    pub text: String,
}

impl From<LinkUnitRow> for Unit {
    fn from(r: LinkUnitRow) -> Self {
        let text = if r.kind == "table" {
            render_table(r.table_rows.as_deref().unwrap_or(&[]), r.caption.as_deref())
        } else {
            r.text
        };
        Self {
            id: r.id,
            document_id: r.document_id,
            file_id: r.file_id,
            text,
        }
    }
}

/// A link before storage (ids are derived from the natural key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Candidate {
    pub entity_id: String,
    pub document_id: String,
    pub chunk_id: Option<String>,
    pub link_type: RelationshipType,
    pub resolver: &'static str,
    pub confidence: Confidence,
    pub evidence: String,
}

impl Candidate {
    pub fn row(&self) -> LinkRow {
        LinkRow {
            id: record::link_id(
                &self.entity_id,
                &self.document_id,
                self.chunk_id.as_deref(),
                self.link_type.as_str(),
                self.resolver,
            ),
            link_type: self.link_type.as_str().into(),
            entity_id: self.entity_id.clone(),
            document_id: self.document_id.clone(),
            chunk_id: self.chunk_id.clone(),
            resolver: self.resolver.into(),
            confidence: self.confidence.as_str().into(),
            evidence: Some(self.evidence.clone()),
        }
    }
}

fn candidate(
    entity_id: &str,
    unit: &Unit,
    link_type: RelationshipType,
    resolver: &'static str,
    confidence: Confidence,
    evidence: &str,
) -> Candidate {
    Candidate {
        entity_id: entity_id.to_owned(),
        document_id: unit.document_id.clone(),
        chunk_id: Some(unit.id.clone()),
        link_type,
        resolver,
        confidence,
        evidence: evidence.to_owned(),
    }
}

pub fn match_exact(
    entities: &[&EntityRow],
    units: &[&Unit],
    idx: &UnitIndex<'_>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for e in entities {
        for p in idx.matching(&e.name) {
            out.push(candidate(
                &e.id,
                units[p],
                RelationshipType::MentionedIn,
                "linker:exact_identifier",
                Confidence::High,
                &e.name,
            ));
        }
    }
    out
}

pub fn match_qualified(
    entities: &[&EntityRow],
    units: &[&Unit],
    idx: &UnitIndex<'_>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for e in entities.iter().filter(|e| e.qualified_name.contains('.')) {
        for p in idx.matching(&e.qualified_name) {
            out.push(candidate(
                &e.id,
                units[p],
                RelationshipType::DocumentedBy,
                "linker:qualified_identifier",
                Confidence::High,
                &e.qualified_name,
            ));
        }
    }
    out
}

pub fn match_alias(
    entities: &[&EntityRow],
    units: &[&Unit],
    idx: &UnitIndex<'_>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for e in entities {
        let segments: Vec<&str> = e.qualified_name.split('.').collect();
        if segments.len() < MIN_ALIAS_SEGMENTS {
            continue;
        }
        let alias = segments[segments.len() - 2..].join(".");
        for p in idx.matching(&alias) {
            out.push(candidate(
                &e.id,
                units[p],
                RelationshipType::MentionedIn,
                "linker:alias",
                Confidence::Medium,
                &alias,
            ));
        }
    }
    out
}

pub fn match_filename(
    namespace: Option<&EntityRow>,
    filenames: &[String],
    units: &[&Unit],
    idx: &UnitIndex<'_>,
) -> Vec<Candidate> {
    let Some(ns) = namespace else {
        return Vec::new();
    };
    let mut positions: Vec<usize> = filenames
        .iter()
        .filter(|f| !f.is_empty())
        .flat_map(|f| idx.candidates(f))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    positions.sort_unstable();
    let mut out = Vec::new();
    for p in positions {
        let unit = units[p];
        if let Some(f) = filenames
            .iter()
            .find(|f| crate::matcher::contains_identifier(&unit.text, f))
        {
            out.push(candidate(
                &ns.id,
                unit,
                RelationshipType::MentionedIn,
                "linker:filename",
                Confidence::Medium,
                f,
            ));
        }
    }
    out
}

pub fn match_routes(routes: &[&RelationshipRow], units: &[&Unit]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for r in routes {
        let Some(rest) = r
            .target_symbol
            .as_deref()
            .and_then(|t| t.strip_prefix(ROUTE_TARGET_PREFIX))
        else {
            continue;
        };
        let (method, path) = rest.split_once(':').unwrap_or((rest, ""));
        if path.is_empty() {
            continue;
        }
        let evidence = format!("{method} {path}");
        for u in units.iter().filter(|u| u.text.contains(path)) {
            out.push(candidate(
                &r.source_entity_id,
                u,
                RelationshipType::RelatedTo,
                "linker:route_heuristic",
                Confidence::Heuristic,
                &evidence,
            ));
        }
    }
    out
}

/// `[name, stem]` of a path's final component (`[name]` without suffix).
pub fn filename_candidates(rel_path: &str) -> Vec<String> {
    let name = rel_path.rsplit(['/', '\\']).next().unwrap_or(rel_path);
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => vec![name.to_owned(), name[..i].to_owned()],
        _ => vec![name.to_owned()],
    }
}

/// Links for the files written in this build, deduplicated on the natural
/// key (entity, document, chunk, resolver), first occurrence winning.
pub fn link_touched_files(
    store: &ProjectStore,
    build_id: &str,
    touched: &[String],
) -> Result<Vec<Candidate>, StorageError> {
    if touched.is_empty() {
        return Ok(Vec::new());
    }
    let entities = store.all_entities(build_id)?;
    let units: Vec<Unit> = store
        .link_units(build_id)?
        .into_iter()
        .map(Unit::from)
        .collect();
    let routes = store.relationships_with_symbol_prefix(build_id, ROUTE_TARGET_PREFIX)?;
    let paths: HashMap<String, String> = store
        .files(build_id)?
        .into_iter()
        .map(|f| (f.id, f.rel_path))
        .collect();

    let mut entities_by_file: HashMap<&str, Vec<&EntityRow>> = HashMap::new();
    for e in &entities {
        entities_by_file
            .entry(e.file_id.as_str())
            .or_default()
            .push(e);
    }
    let mut units_by_file: HashMap<&str, Vec<&Unit>> = HashMap::new();
    for u in &units {
        units_by_file.entry(u.file_id.as_str()).or_default().push(u);
    }
    fn namespace_of<'e>(ents: &[&'e EntityRow]) -> Option<&'e EntityRow> {
        ents.iter()
            .find(|e| e.kind == EntityType::Namespace.as_str())
            .copied()
    }
    let all_units: Vec<&Unit> = units.iter().collect();
    let all_index = UnitIndex::new(all_units.iter().map(|u| u.text.as_str()).collect());

    let mut out = Vec::new();
    let mut covered: HashSet<&str> = HashSet::new();
    for file_id in touched {
        let Some(file_entities) = entities_by_file.get(file_id.as_str()) else {
            continue;
        };
        covered.insert(file_id.as_str());
        let filenames = paths
            .get(file_id)
            .map(|p| filename_candidates(p))
            .unwrap_or_default();
        let ids: HashSet<&str> = file_entities.iter().map(|e| e.id.as_str()).collect();
        let file_routes: Vec<&RelationshipRow> = routes
            .iter()
            .filter(|r| ids.contains(r.source_entity_id.as_str()))
            .collect();
        out.extend(match_exact(file_entities, &all_units, &all_index));
        out.extend(match_qualified(file_entities, &all_units, &all_index));
        out.extend(match_alias(file_entities, &all_units, &all_index));
        out.extend(match_filename(
            namespace_of(file_entities),
            &filenames,
            &all_units,
            &all_index,
        ));
        out.extend(match_routes(&file_routes, &all_units));
    }

    let touched_docs: Vec<&str> = touched
        .iter()
        .map(String::as_str)
        .filter(|f| units_by_file.contains_key(f))
        .collect();
    if !touched_docs.is_empty() {
        let doc_side_entities: Vec<&EntityRow> = entities
            .iter()
            .filter(|e| !covered.contains(e.file_id.as_str()))
            .collect();
        let covered_ids: HashSet<&str> = entities
            .iter()
            .filter(|e| covered.contains(e.file_id.as_str()))
            .map(|e| e.id.as_str())
            .collect();
        let doc_side_routes: Vec<&RelationshipRow> = routes
            .iter()
            .filter(|r| !covered_ids.contains(r.source_entity_id.as_str()))
            .collect();
        let mut namespaces: Vec<(Option<&EntityRow>, Vec<String>)> = Vec::new();
        let mut files: Vec<&&str> = entities_by_file
            .keys()
            .filter(|f| !covered.contains(**f))
            .collect();
        files.sort();
        for f in files {
            let ents = &entities_by_file[*f];
            let filenames = paths
                .get(*f)
                .map(|p| filename_candidates(p))
                .unwrap_or_default();
            namespaces.push((namespace_of(ents), filenames));
        }
        for doc in touched_docs {
            let file_units = &units_by_file[doc];
            let idx = UnitIndex::new(file_units.iter().map(|u| u.text.as_str()).collect());
            out.extend(match_exact(&doc_side_entities, file_units, &idx));
            out.extend(match_qualified(&doc_side_entities, file_units, &idx));
            out.extend(match_alias(&doc_side_entities, file_units, &idx));
            out.extend(match_routes(&doc_side_routes, file_units));
            for (ns, filenames) in &namespaces {
                out.extend(match_filename(*ns, filenames, file_units, &idx));
            }
        }
    }

    let mut seen = HashSet::new();
    out.retain(|c| {
        seen.insert((
            c.entity_id.clone(),
            c.document_id.clone(),
            c.chunk_id.clone(),
            c.resolver,
        ))
    });
    Ok(out)
}
