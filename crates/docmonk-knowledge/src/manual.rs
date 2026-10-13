//! Explicit, user-defined links (`docmonk link add|remove|list`).
//!
//! Stored persistently in `manual_links` by entity qualified name and
//! document path (plus attachment index and optional chunk ordinal), so they
//! survive every rebuild, and materialized into each build as
//! `resolver = "user"`, `confidence = exact`, `documented_by`. The automated
//! linker never writes or removes them.

use std::collections::HashMap;

use docmonk_core::ids::record;
use docmonk_core::models::{Confidence, RelationshipType};
use docmonk_storage::knowledge::{EntityRow, LinkRow, ManualLink, ProjectStore};
use docmonk_storage::StorageError;

pub const USER_RESOLVER: &str = "user";

#[derive(Debug, thiserror::Error)]
pub enum ManualLinkError {
    #[error("no unambiguous match for entity '{0}'")]
    Entity(String),
    #[error("no unambiguous match for document '{0}'")]
    Document(String),
    #[error("no such section {1} in document '{0}'")]
    Section(String, i64),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Entity by exact id, else by bare or qualified name; must be unique.
pub fn resolve_entity(
    store: &ProjectStore,
    build_id: &str,
    r: &str,
) -> Result<EntityRow, ManualLinkError> {
    if let Some(e) = store.entity(build_id, r)? {
        return Ok(e);
    }
    let mut c = store.entities_named(build_id, r)?;
    if c.len() == 1 {
        Ok(c.remove(0))
    } else {
        Err(ManualLinkError::Entity(r.to_owned()))
    }
}

/// Document by id, else by file path equal to / ending with / named `r`;
/// must be unique. Returns `(rel_path, attachment_index, document_id)`.
pub fn resolve_document(
    store: &ProjectStore,
    build_id: &str,
    r: &str,
) -> Result<(String, Option<i64>, String), ManualLinkError> {
    let files: HashMap<String, String> = store
        .files(build_id)?
        .into_iter()
        .map(|f| (f.id, f.rel_path))
        .collect();
    let docs = store.documents(build_id)?;
    let describe = |d: &docmonk_storage::knowledge::DocumentRow| {
        (
            files.get(&d.file_id).cloned().unwrap_or_default(),
            d.attachment.as_ref().map(|a| a.index),
            d.id.clone(),
        )
    };
    if let Some(d) = docs.iter().find(|d| d.id == r) {
        return Ok(describe(d));
    }
    let matches: Vec<_> = docs
        .iter()
        .filter(|d| d.attachment.is_none())
        .filter(|d| {
            files
                .get(&d.file_id)
                .is_some_and(|p| p == r || p.ends_with(r) || p.rsplit('/').next() == Some(r))
        })
        .collect();
    match matches.as_slice() {
        [d] => Ok(describe(d)),
        _ => Err(ManualLinkError::Document(r.to_owned())),
    }
}

/// Adds a manual link (and makes it visible in `build_id` immediately).
/// `Ok(None)` when it already exists.
pub fn add(
    store: &mut ProjectStore,
    build_id: &str,
    entity_ref: &str,
    document_ref: &str,
    chunk_ordinal: Option<i64>,
    note: Option<String>,
    now: &str,
) -> Result<Option<ManualLink>, ManualLinkError> {
    let entity = resolve_entity(store, build_id, entity_ref)?;
    let (rel_path, attachment_index, document_id) =
        resolve_document(store, build_id, document_ref)?;
    if let Some(o) = chunk_ordinal {
        let exists = store
            .link_units(build_id)?
            .iter()
            .any(|u| u.document_id == document_id && u.ordinal == o);
        if !exists {
            return Err(ManualLinkError::Section(document_ref.to_owned(), o));
        }
    }
    let link = ManualLink {
        id: record::link_id(
            &entity.qualified_name,
            &format!("{rel_path}#{}", attachment_index.unwrap_or(-1)),
            chunk_ordinal.map(|o| o.to_string()).as_deref(),
            RelationshipType::DocumentedBy.as_str(),
            USER_RESOLVER,
        ),
        link_type: RelationshipType::DocumentedBy.as_str().into(),
        entity_qualified_name: entity.qualified_name,
        document_rel_path: rel_path,
        attachment_index,
        chunk_ordinal,
        note,
        created_at: now.to_owned(),
    };
    if !store.add_manual_link(&link)? {
        return Ok(None);
    }
    apply(store, build_id)?;
    Ok(Some(link))
}

/// Removes a manual link by id (and from `build_id`); `false` if unknown.
pub fn remove(store: &mut ProjectStore, build_id: &str, id: &str) -> Result<bool, ManualLinkError> {
    let removed = store.remove_manual_link(id)?;
    if removed {
        apply(store, build_id)?;
    }
    Ok(removed)
}

/// Re-materializes every manual link into `build_id`. A link whose entity
/// or document is not in the build stays stored and reappears with it; a
/// chunk ordinal that no longer exists falls back to the whole document.
pub fn apply(store: &mut ProjectStore, build_id: &str) -> Result<usize, StorageError> {
    store.delete_links_by_resolver(build_id, USER_RESOLVER)?;
    let rows: Vec<LinkRow> = materialize(store, build_id)?
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    let n = rows.len();
    store.put_links(build_id, &rows)?;
    Ok(n)
}

/// Materialized link id to the id of the manual link that produced it.
pub fn manual_ids(
    store: &ProjectStore,
    build_id: &str,
) -> Result<HashMap<String, String>, StorageError> {
    Ok(materialize(store, build_id)?
        .into_iter()
        .map(|(manual, row)| (row.id, manual))
        .collect())
}

/// Each manual link's rows in `build_id`, paired with its manual id.
fn materialize(
    store: &ProjectStore,
    build_id: &str,
) -> Result<Vec<(String, LinkRow)>, StorageError> {
    let manual = store.manual_links()?;
    if manual.is_empty() {
        return Ok(Vec::new());
    }
    let files: HashMap<String, String> = store
        .files(build_id)?
        .into_iter()
        .map(|f| (f.rel_path, f.id))
        .collect();
    let docs: HashMap<String, ()> = store
        .documents(build_id)?
        .into_iter()
        .map(|d| (d.id, ()))
        .collect();
    let chunks: HashMap<(String, i64), String> = store
        .link_units(build_id)?
        .into_iter()
        .map(|u| ((u.document_id, u.ordinal), u.id))
        .collect();
    let mut rows = Vec::new();
    for m in manual {
        let Some(file_id) = files.get(&m.document_rel_path) else {
            continue;
        };
        let document_id = record::document_id(file_id, m.attachment_index.map(|i| i as u64));
        if !docs.contains_key(&document_id) {
            continue;
        }
        let chunk_id = m
            .chunk_ordinal
            .and_then(|o| chunks.get(&(document_id.clone(), o)).cloned());
        let entities: Vec<EntityRow> = store
            .entities_named(build_id, &m.entity_qualified_name)?
            .into_iter()
            .filter(|e| e.qualified_name == m.entity_qualified_name)
            .collect();
        for e in entities {
            rows.push((
                m.id.clone(),
                LinkRow {
                    id: record::link_id(
                        &e.id,
                        &document_id,
                        chunk_id.as_deref(),
                        &m.link_type,
                        USER_RESOLVER,
                    ),
                    link_type: m.link_type.clone(),
                    entity_id: e.id,
                    document_id: document_id.clone(),
                    chunk_id: chunk_id.clone(),
                    resolver: USER_RESOLVER.into(),
                    confidence: Confidence::Exact.as_str().into(),
                    evidence: m.note.clone(),
                },
            ));
        }
    }
    Ok(rows)
}
