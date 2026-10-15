//! Cross-domain linker (`link_touched_files`):
//! code entities <-> document chunks.
//!
//! | Resolver | Confidence | Link type | Signal |
//! |---|---|---|---|
//! | `linker:exact_identifier` | high | `mentioned_in` | bare name (not a [`COMMON_NAMES`] word, 3+ chars) |
//! | `linker:qualified_identifier` | high | `documented_by` | dotted qualified name |
//! | `linker:alias` | medium | `mentioned_in` | final two segments (3+ segment names) |
//! | `linker:filename` | medium | `mentioned_in` | source filename/stem, on the file's namespace entity |
//! | `linker:route_heuristic` | heuristic | `related_to` | framework route path, not followed by a word character |
//!
//! An entity links to at most [`MAX_LINKS_PER_DOCUMENT`] chunks of one
//! document per resolver.
//!
//! `exact` is reserved for manual links (`resolver = "user"`); this module
//! cannot produce it. Scoping: touched code files
//! are matched against every unit, touched documents against every entity
//! of code files not already covered, so cost tracks what changed. A pass
//! that touches only code files (up to [`NARROW_LINK_FILES`]) loads only
//! their entities and routes; identifier matches are remembered per needle
//! within a pass, and route paths are found in each unit with one
//! automaton pass rather than one substring search per route.

use std::collections::{HashMap, HashSet};

use std::time::Instant;

use docmonk_code::graph_stage::LinkStats;
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

/// Names too generic to link on their own (compared case-insensitively):
/// a document saying "find the order, then save it" is not about every
/// `Find` and `Save` method. Such entities still link through their
/// qualified name, alias, file name or route.
pub const COMMON_NAMES: &[&str] = &[
    "add",
    "apply",
    "build",
    "call",
    "check",
    "clear",
    "clone",
    "close",
    "compare",
    "config",
    "copy",
    "count",
    "create",
    "data",
    "debug",
    "default",
    "delete",
    "dispose",
    "equals",
    "error",
    "execute",
    "find",
    "format",
    "get",
    "handle",
    "hash",
    "id",
    "index",
    "info",
    "init",
    "item",
    "items",
    "key",
    "keys",
    "len",
    "length",
    "list",
    "load",
    "log",
    "main",
    "map",
    "name",
    "new",
    "next",
    "open",
    "options",
    "parse",
    "process",
    "read",
    "remove",
    "reset",
    "result",
    "run",
    "save",
    "send",
    "set",
    "size",
    "start",
    "stop",
    "test",
    "to_string",
    "tostring",
    "type",
    "update",
    "validate",
    "value",
    "warn",
    "write",
];

/// Shortest name that links on its own (bare-name resolver).
pub const MIN_BARE_NAME_CHARS: usize = 3;

/// Most chunks of one document a single entity links to per resolver (the
/// first ones, in document order): a long document repeating a name does
/// not flood the graph.
pub const MAX_LINKS_PER_DOCUMENT: usize = 5;

/// Whether a bare name may link on its own.
pub fn links_by_bare_name(name: &str) -> bool {
    name.chars().count() >= MIN_BARE_NAME_CHARS
        && !COMMON_NAMES.contains(&name.to_lowercase().as_str())
}

pub fn match_exact(
    entities: &[&EntityRow],
    units: &[&Unit],
    idx: &UnitIndex<'_>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for e in entities.iter().filter(|e| links_by_bare_name(&e.name)) {
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

/// `(method, path)` of a framework route edge with a non-empty path.
fn route_of(r: &RelationshipRow) -> Option<(&str, &str)> {
    let rest = r
        .target_symbol
        .as_deref()
        .and_then(|t| t.strip_prefix(ROUTE_TARGET_PREFIX))?;
    let (method, path) = rest.split_once(':').unwrap_or((rest, ""));
    (!path.is_empty()).then_some((method, path))
}

pub fn match_routes(routes: &[&RelationshipRow], units: &[&Unit]) -> Vec<Candidate> {
    match RouteMatcher::new(routes) {
        Some(m) => match_routes_with(routes, units, &m.positions(units)),
        None => match_routes_naive(routes, units),
    }
}

/// Whether `text` mentions route `path`: an occurrence not followed by a
/// word character (`api/orders1` is not mentioned by `api/orders12/{id}`).
pub fn mentions_route(text: &str, path: &str) -> bool {
    text.match_indices(path).any(|(i, m)| {
        text[i + m.len()..]
            .chars()
            .next()
            .is_none_or(|c| !crate::matcher::is_word(c))
    })
}

/// The reference route matcher: one search per (route, unit) pair.
pub fn match_routes_naive(routes: &[&RelationshipRow], units: &[&Unit]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for r in routes {
        let Some((method, path)) = route_of(r) else {
            continue;
        };
        let evidence = format!("{method} {path}");
        for u in units.iter().filter(|u| mentions_route(&u.text, path)) {
            out.push(route_candidate(r, u, &evidence));
        }
    }
    out
}

fn route_candidate(r: &RelationshipRow, u: &Unit, evidence: &str) -> Candidate {
    candidate(
        &r.source_entity_id,
        u,
        RelationshipType::RelatedTo,
        "linker:route_heuristic",
        Confidence::Heuristic,
        evidence,
    )
}

/// [`match_routes_naive`] given which units contain each path.
fn match_routes_with(
    routes: &[&RelationshipRow],
    units: &[&Unit],
    positions: &HashMap<&str, Vec<usize>>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for r in routes {
        let Some((method, path)) = route_of(r) else {
            continue;
        };
        let evidence = format!("{method} {path}");
        for p in positions.get(path).into_iter().flatten() {
            out.push(route_candidate(r, units[*p], &evidence));
        }
    }
    out
}

/// Every route path at once (Aho-Corasick, overlapping matches): one pass
/// over each unit finds exactly the units that mention a path, as
/// [`mentions_route`] would, instead of one search per (route, unit) pair.
pub struct RouteMatcher<'r> {
    paths: Vec<&'r str>,
    automaton: aho_corasick::AhoCorasick,
}

impl<'r> RouteMatcher<'r> {
    /// `None` when there is no route path (or the automaton cannot be
    /// built; callers then use the plain search).
    pub fn new(routes: &[&'r RelationshipRow]) -> Option<Self> {
        let mut paths: Vec<&str> = routes
            .iter()
            .filter_map(|r| route_of(r))
            .map(|(_, p)| p)
            .collect();
        paths.sort_unstable();
        paths.dedup();
        if paths.is_empty() {
            return None;
        }
        let automaton = aho_corasick::AhoCorasick::builder()
            .match_kind(aho_corasick::MatchKind::Standard)
            .build(&paths)
            .ok()?;
        Some(Self { paths, automaton })
    }

    /// For each path, the ascending positions of the units containing it.
    pub fn positions(&self, units: &[&Unit]) -> HashMap<&'r str, Vec<usize>> {
        let mut out: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut seen: HashSet<usize> = HashSet::new();
        for (pos, u) in units.iter().enumerate() {
            seen.clear();
            for m in self.automaton.find_overlapping_iter(u.text.as_str()) {
                let i = m.pattern().as_usize();
                // Only a mention that ends the path (see `mentions_route`).
                let ends = u.text[m.end()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !crate::matcher::is_word(c));
                if ends && seen.insert(i) {
                    out.entry(self.paths[i]).or_default().push(pos);
                }
            }
        }
        out
    }
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
    link_touched_files_with_stats(store, build_id, touched).map(|(c, _)| c)
}

/// Most touched code files linked from their own entities and routes
/// rather than from the whole build's (when no touched file has document
/// units, nothing else is matched).
pub const NARROW_LINK_FILES: usize = 256;

/// What the linker loads from the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkLoad {
    /// Only the touched files' entities and routes when that is all the
    /// pass needs (code-only changes); everything otherwise.
    Auto,
    /// Every entity and route of the build (the reference behaviour).
    Build,
}

/// [`link_touched_files`], also reporting what was loaded and how long
/// each step took (`links`, `manual` and `write_seconds` are left to the
/// caller that writes them).
pub fn link_touched_files_with_stats(
    store: &ProjectStore,
    build_id: &str,
    touched: &[String],
) -> Result<(Vec<Candidate>, LinkStats), StorageError> {
    link_touched_files_with(store, build_id, touched, LinkLoad::Auto)
}

pub fn link_touched_files_with(
    store: &ProjectStore,
    build_id: &str,
    touched: &[String],
    load: LinkLoad,
) -> Result<(Vec<Candidate>, LinkStats), StorageError> {
    let mut stats = LinkStats::default();
    if touched.is_empty() {
        return Ok((Vec::new(), stats));
    }
    let mut clock = Instant::now();
    let mut lap = |slot: &mut f64| {
        *slot += clock.elapsed().as_secs_f64();
        clock = Instant::now();
    };
    let units: Vec<Unit> = store
        .link_units(build_id)?
        .into_iter()
        .map(Unit::from)
        .collect();
    let mut units_by_file: HashMap<&str, Vec<&Unit>> = HashMap::new();
    for u in &units {
        units_by_file.entry(u.file_id.as_str()).or_default().push(u);
    }
    // Document-side matching needs every entity and route of the build;
    // code-side matching only the touched files' own.
    let narrow = load == LinkLoad::Auto
        && touched.len() <= NARROW_LINK_FILES
        && !touched
            .iter()
            .any(|f| units_by_file.contains_key(f.as_str()));
    // A pass that touches documents matches them against the entities of
    // the build; only entities a touched unit can match are loaded: those
    // whose name is one of the units' words (a qualified or alias needle
    // ends in the name, and every needle match is a whole-word match),
    // plus every entity of files whose name or stem is spelled out there
    // (filename links).
    let doc_narrow = !narrow && load == LinkLoad::Auto;
    let (entities, routes) = if doc_narrow {
        let mut words: HashSet<&str> = HashSet::new();
        for f in touched {
            for u in units_by_file.get(f.as_str()).into_iter().flatten() {
                words.extend(crate::matcher::words(&u.text));
            }
        }
        let mut by_id: HashMap<String, EntityRow> = HashMap::new();
        let names: Vec<&str> = words.iter().copied().collect();
        for rows in store
            .entities_by_bare_names(build_id, &names)?
            .into_values()
        {
            for e in rows {
                by_id.entry(e.id.clone()).or_insert(e);
            }
        }
        let mut files: Vec<&str> = touched.iter().map(String::as_str).collect();
        let all_files = store.files(build_id)?;
        for f in &all_files {
            let spelled = filename_candidates(&f.rel_path).iter().any(|c| {
                let w = crate::matcher::words(c);
                !w.is_empty() && w.iter().all(|w| words.contains(w))
            });
            if spelled {
                files.push(f.id.as_str());
            }
        }
        files.sort_unstable();
        files.dedup();
        for f in files {
            for e in store.file_entities(build_id, f)? {
                by_id.entry(e.id.clone()).or_insert(e);
            }
        }
        let mut entities: Vec<EntityRow> = by_id.into_values().collect();
        // The whole-build order (`all_entities`): file, line, id.
        entities.sort_by(|a, b| {
            (&a.file_id, a.start_line, &a.id).cmp(&(&b.file_id, b.start_line, &b.id))
        });
        (
            entities,
            store.relationships_with_symbol_prefix(build_id, ROUTE_TARGET_PREFIX)?,
        )
    } else if narrow {
        let mut entities = Vec::new();
        let mut routes = Vec::new();
        for f in touched {
            let mut e = store.file_entities(build_id, f)?;
            // The whole-build order (file, line, id).
            e.sort_by(|a, b| (a.start_line, &a.id).cmp(&(b.start_line, &b.id)));
            entities.extend(e);
            // What the build-wide prefix query (`LIKE`, ASCII case-folded)
            // returns for this file.
            routes.extend(
                store
                    .file_relationships(build_id, f)?
                    .into_iter()
                    .filter(|r| {
                        r.target_symbol.as_deref().is_some_and(|t| {
                            t.as_bytes()
                                .get(..ROUTE_TARGET_PREFIX.len())
                                .is_some_and(|p| {
                                    p.eq_ignore_ascii_case(ROUTE_TARGET_PREFIX.as_bytes())
                                })
                        })
                    }),
            );
        }
        routes.sort_by(|a, b| a.id.cmp(&b.id));
        (entities, routes)
    } else {
        (
            store.all_entities(build_id)?,
            store.relationships_with_symbol_prefix(build_id, ROUTE_TARGET_PREFIX)?,
        )
    };
    let paths: HashMap<String, String> = store
        .files(build_id)?
        .into_iter()
        .map(|f| (f.id, f.rel_path))
        .collect();
    stats.entities_loaded = entities.len();
    stats.units_loaded = units.len();
    stats.routes_loaded = routes.len();
    stats.narrow = narrow;
    lap(&mut stats.load_seconds);

    let mut entities_by_file: HashMap<&str, Vec<&EntityRow>> = HashMap::new();
    let mut file_of: HashMap<&str, &str> = HashMap::new();
    for e in &entities {
        entities_by_file
            .entry(e.file_id.as_str())
            .or_default()
            .push(e);
        file_of.insert(e.id.as_str(), e.file_id.as_str());
    }
    // Each file's routes, in id order (a route belongs to the file of its
    // source entity).
    let mut routes_by_file: HashMap<&str, Vec<&RelationshipRow>> = HashMap::new();
    for r in &routes {
        if let Some(f) = file_of.get(r.source_entity_id.as_str()) {
            routes_by_file.entry(f).or_default().push(r);
        }
    }
    fn namespace_of<'e>(ents: &[&'e EntityRow]) -> Option<&'e EntityRow> {
        ents.iter()
            .find(|e| e.kind == EntityType::Namespace.as_str())
            .copied()
    }
    let all_units: Vec<&Unit> = units.iter().collect();
    let all_index = UnitIndex::new(all_units.iter().map(|u| u.text.as_str()).collect());
    let code_routes: Vec<&RelationshipRow> = touched
        .iter()
        .filter_map(|f| routes_by_file.get(f.as_str()))
        .flatten()
        .copied()
        .collect();
    let code_route_positions = RouteMatcher::new(&code_routes).map(|m| m.positions(&all_units));
    lap(&mut stats.index_seconds);

    let mut out = Vec::new();
    let mut covered: HashSet<&str> = HashSet::new();
    let mut timed = |slot: &mut f64, found: Vec<Candidate>, out: &mut Vec<Candidate>| {
        *slot += clock.elapsed().as_secs_f64();
        clock = Instant::now();
        out.extend(found);
    };
    for file_id in touched {
        let Some(file_entities) = entities_by_file.get(file_id.as_str()) else {
            continue;
        };
        covered.insert(file_id.as_str());
        let filenames = paths
            .get(file_id)
            .map(|p| filename_candidates(p))
            .unwrap_or_default();
        let file_routes = routes_by_file
            .get(file_id.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let found = match_exact(file_entities, &all_units, &all_index);
        timed(&mut stats.exact_seconds, found, &mut out);
        let found = match_qualified(file_entities, &all_units, &all_index);
        timed(&mut stats.qualified_seconds, found, &mut out);
        let found = match_alias(file_entities, &all_units, &all_index);
        timed(&mut stats.alias_seconds, found, &mut out);
        let found = match_filename(
            namespace_of(file_entities),
            &filenames,
            &all_units,
            &all_index,
        );
        timed(&mut stats.filename_seconds, found, &mut out);
        let found = match &code_route_positions {
            Some(p) => match_routes_with(file_routes, &all_units, p),
            None => Vec::new(),
        };
        timed(&mut stats.route_seconds, found, &mut out);
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
        let doc_route_matcher = RouteMatcher::new(&doc_side_routes);
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
            timed(&mut stats.match_seconds, Vec::new(), &mut out);
            let idx = UnitIndex::new(file_units.iter().map(|u| u.text.as_str()).collect());
            timed(&mut stats.index_seconds, Vec::new(), &mut out);
            let found = match_exact(&doc_side_entities, file_units, &idx);
            timed(&mut stats.exact_seconds, found, &mut out);
            let found = match_qualified(&doc_side_entities, file_units, &idx);
            timed(&mut stats.qualified_seconds, found, &mut out);
            let found = match_alias(&doc_side_entities, file_units, &idx);
            timed(&mut stats.alias_seconds, found, &mut out);
            let found = match &doc_route_matcher {
                Some(m) => {
                    match_routes_with(&doc_side_routes, file_units, &m.positions(file_units))
                }
                None => Vec::new(),
            };
            timed(&mut stats.route_seconds, found, &mut out);
            let mut found = Vec::new();
            for (ns, filenames) in &namespaces {
                found.extend(match_filename(*ns, filenames, file_units, &idx));
            }
            timed(&mut stats.filename_seconds, found, &mut out);
        }
    }

    stats.candidates = out.len();
    let mut seen = HashSet::new();
    out.retain(|c| {
        seen.insert((
            c.entity_id.clone(),
            c.document_id.clone(),
            c.chunk_id.clone(),
            c.resolver,
        ))
    });
    // Per entity, document and resolver, the first chunks only (local to
    // one document, so incremental passes equal a full build).
    let mut per_doc: HashMap<(String, String, &'static str), usize> = HashMap::new();
    out.retain(|c| {
        let n = per_doc
            .entry((c.entity_id.clone(), c.document_id.clone(), c.resolver))
            .or_default();
        *n += 1;
        *n <= MAX_LINKS_PER_DOCUMENT
    });
    timed(&mut stats.match_seconds, Vec::new(), &mut out);
    stats.match_seconds += stats.exact_seconds
        + stats.qualified_seconds
        + stats.alias_seconds
        + stats.filename_seconds
        + stats.route_seconds;
    Ok((out, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(id: &str, text: &str) -> Unit {
        Unit {
            id: id.into(),
            document_id: "d".into(),
            file_id: "f".into(),
            text: text.into(),
        }
    }

    fn route(id: &str, symbol: &str) -> RelationshipRow {
        RelationshipRow {
            id: id.into(),
            source_entity_id: format!("e-{id}"),
            target_symbol: Some(symbol.into()),
            ..RelationshipRow::default()
        }
    }

    #[test]
    fn the_route_automaton_finds_exactly_what_the_reference_search_finds() {
        let units = [
            unit("u1", "GET /api/a/b and /api/a"),
            unit("u2", "no routes here"),
            unit("u3", "xx/api/a/bxx"),
            unit("u4", "café /κόσμος/{id} done"),
            unit("u5", "/api/a/api/a overlapping"),
        ];
        let routes = [
            route("r1", "http_endpoint:GET:/api/a"),
            route("r2", "http_endpoint:GET:/api/a/b"),
            route("r3", "http_endpoint:POST:/κόσμος/{id}"),
            route("r4", "http_endpoint:GET:"),
            route("r5", "http_endpoint:GET:/api/a"),
            route("r6", "unrelated"),
            route("r7", "http_endpoint:PUT:/nowhere"),
        ];
        let units: Vec<&Unit> = units.iter().collect();
        let routes: Vec<&RelationshipRow> = routes.iter().collect();
        let fast = match_routes(&routes, &units);
        assert_eq!(fast, match_routes_naive(&routes, &units));
        // r1 and r5 (`/api/a`): u1, u3 ("/" follows) and u5; r2
        // (`/api/a/b`): u1 only (u3 continues with "x"); r3: u4.
        assert_eq!(fast.len(), 3 + 1 + 1 + 3, "{fast:?}");
        assert!(
            RouteMatcher::new(&routes[3..4]).is_none(),
            "no path, no automaton"
        );
    }
}
