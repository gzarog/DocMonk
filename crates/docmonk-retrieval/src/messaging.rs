//! Message flows across sources (ADR 0044): the publishers and consumers
//! of a message endpoint, read from the `message:<key>` references of the
//! graph derivation `graph-4` with one batched, indexed `target_symbol`
//! lookup per source and round, like exact URL occurrences.
//!
//! AMQP bindings are followed one hop: consumers of a queue bound to an
//! exchange are consumers of the exchange's matching publishers (topic
//! routing keys: `*` one word, `#` any), and publishers to that exchange
//! are publishers for the queue. Kafka topics and MassTransit message
//! types need no binding. Nothing is inferred from names alone: an
//! endpoint no code site names has no flow.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use docmonk_core::messaging::{self as msg, Role, MESSAGE_PREFIX};
use docmonk_storage::knowledge::EntityRow;
use serde::Serialize;

use crate::{Corpus, SearchError};

/// One publish, consume, bind or declare site.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessageSiteHit {
    #[serde(skip)]
    pub corpus: usize,
    pub source_id: String,
    pub path: String,
    pub line: Option<i64>,
    pub role: &'static str,
    pub framework: String,
    /// The endpoint key (`kafka/orders`, `amqp/exchange/orders`, ...).
    pub endpoint: String,
    pub routing_key: Option<String>,
    /// Bind sites: the endpoint bound to.
    pub bound: Option<String>,
    /// The function, method or type around the site.
    pub entity: Option<String>,
    #[serde(skip)]
    pub entity_row: Option<EntityRow>,
}

/// Every site of one endpoint family, deterministic order (role, endpoint,
/// path, line, entity, source).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MessageFlow {
    /// The endpoint keys that matched a site.
    pub endpoints: Vec<String>,
    pub publishers: Vec<MessageSiteHit>,
    pub consumers: Vec<MessageSiteHit>,
    pub bindings: Vec<MessageSiteHit>,
    pub declarations: Vec<MessageSiteHit>,
}

impl MessageFlow {
    pub fn is_empty(&self) -> bool {
        self.publishers.is_empty()
            && self.consumers.is_empty()
            && self.bindings.is_empty()
            && self.declarations.is_empty()
    }
}

fn split_location(loc: &str) -> (String, Option<i64>) {
    match loc.rsplit_once(':') {
        Some((p, l)) if !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) => {
            (p.to_owned(), l.parse().ok())
        }
        _ => (loc.to_owned(), None),
    }
}

/// The sites of `keys` (endpoint keys without the `message:` prefix) in
/// every corpus.
fn sites(
    corpora: &[Corpus<'_>],
    keys: &BTreeSet<String>,
    limit: usize,
) -> Result<Vec<MessageSiteHit>, SearchError> {
    let symbols: Vec<String> = keys
        .iter()
        .map(|k| format!("{MESSAGE_PREFIX}{k}"))
        .collect();
    let mut out = Vec::new();
    if symbols.is_empty() {
        return Ok(out);
    }
    for (i, c) in corpora.iter().enumerate() {
        crate::check_deadline()?;
        let found =
            c.store
                .unresolved_edges_many(c.build_id, &symbols, &["references"], limit as i64)?;
        let rows: Vec<_> = found.into_values().flatten().collect();
        let owners: Vec<String> = rows.iter().map(|r| r.source_entity_id.clone()).collect();
        let entities = if owners.is_empty() {
            HashMap::new()
        } else {
            c.store.entities_by_ids(c.build_id, &owners)?
        };
        for r in rows {
            let Some(endpoint) = r
                .target_symbol
                .as_deref()
                .and_then(|s| s.strip_prefix(MESSAGE_PREFIX))
            else {
                continue;
            };
            let Some((framework, role)) = msg::parse_resolver(&r.resolver) else {
                continue;
            };
            let evidence = r.evidence.as_deref().unwrap_or_default();
            let (path, line) = split_location(r.source_location.as_deref().unwrap_or(""));
            let entity_row = entities.get(&r.source_entity_id).cloned();
            out.push(MessageSiteHit {
                corpus: i,
                source_id: c.store.source_id().to_owned(),
                path,
                line,
                role: role.as_str(),
                framework: framework.to_owned(),
                endpoint: endpoint.to_owned(),
                routing_key: msg::routing_key_of(evidence).map(str::to_owned),
                bound: msg::bound_of(evidence).map(str::to_owned),
                entity: entity_row.as_ref().map(|e| e.qualified_name.clone()),
                entity_row,
            });
        }
    }
    Ok(out)
}

fn sort_key(h: &MessageSiteHit) -> (String, String, Option<i64>, String, String) {
    (
        h.endpoint.clone(),
        h.path.clone(),
        h.line,
        h.entity.clone().unwrap_or_default(),
        h.source_id.clone(),
    )
}

/// The flow of the endpoints `keys` (see the module docs).
pub fn message_flow(
    corpora: &[Corpus<'_>],
    keys: &[String],
    limit: usize,
) -> Result<MessageFlow, SearchError> {
    let wanted: BTreeSet<String> = keys.iter().cloned().collect();
    let mut hits = sites(corpora, &wanted, limit)?;
    // Routing keys published to each requested exchange.
    let mut published: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for h in hits.iter().filter(|h| h.role == Role::Publish.as_str()) {
        published
            .entry(h.endpoint.as_str())
            .or_default()
            .push(h.routing_key.as_deref().unwrap_or_default());
    }
    // Bindings of the requested endpoints: the other side, with the
    // binding key that has to match. A queue bound to a requested exchange
    // is followed when its binding matches a published routing key (or
    // nothing publishes to the exchange in these sources).
    let mut via: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for h in hits.iter().filter(|h| h.role == Role::Bind.as_str()) {
        let Some(b) = h.bound.as_ref().filter(|b| !wanted.contains(*b)) else {
            continue;
        };
        let binding = h.routing_key.clone().unwrap_or_default();
        let follows = match published.get(h.endpoint.as_str()) {
            Some(keys) if h.endpoint.starts_with("amqp/exchange/") => {
                keys.iter().any(|k| msg::routing_key_matches(&binding, k))
            }
            _ => true,
        };
        if follows {
            via.entry(b.clone()).or_default().insert(binding);
        }
    }
    let more: BTreeSet<String> = via.keys().cloned().collect();
    let mut extra = sites(corpora, &more, limit)?;
    // Reached through a binding: an exchange's publishers count when their
    // routing key matches the binding; a queue's consumers always do.
    extra.retain(|h| match Role::parse(h.role) {
        Some(Role::Publish) => via.get(&h.endpoint).is_some_and(|bindings| {
            bindings
                .iter()
                .any(|b| msg::routing_key_matches(b, h.routing_key.as_deref().unwrap_or_default()))
        }),
        Some(Role::Consume) => true,
        _ => false,
    });
    hits.extend(extra);
    let mut flow = MessageFlow::default();
    let mut endpoints = BTreeSet::new();
    for h in hits {
        endpoints.insert(h.endpoint.clone());
        match Role::parse(h.role) {
            Some(Role::Publish) => flow.publishers.push(h),
            Some(Role::Consume) => flow.consumers.push(h),
            Some(Role::Bind) => flow.bindings.push(h),
            Some(Role::Declare) => flow.declarations.push(h),
            None => {}
        }
    }
    for v in [
        &mut flow.publishers,
        &mut flow.consumers,
        &mut flow.bindings,
        &mut flow.declarations,
    ] {
        v.sort_by_key(sort_key);
        v.dedup_by(|a, b| sort_key(a) == sort_key(b) && a.role == b.role);
        v.truncate(limit);
    }
    flow.endpoints = endpoints.into_iter().collect();
    Ok(flow)
}

/// The endpoint keys `name` may stand for, plus the message-type key of
/// every type it matched (MassTransit contracts are types).
pub fn keys_for(name: &str, type_names: &[&str]) -> Vec<String> {
    let mut keys = msg::candidate_keys(name);
    for t in type_names {
        let k = msg::message_type(t);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}
