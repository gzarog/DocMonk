//! Bounded per-corpus concurrency (P06).
//!
//! Server-mode readers are thread-safe, so the corpora of one request can
//! be read concurrently. A request over 150 sources must not start 150
//! threads (or 150 simultaneous HTTP requests): [`map_corpora`] runs at
//! most [`max_concurrency`] workers pulling corpora from a shared queue,
//! and returns results in corpus order, so rankings never depend on
//! scheduling. SQLite connections belong to one thread: those corpora run
//! sequentially on the calling thread.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use docmonk_storage::read::KnowledgeRead;

use crate::{Corpus, SearchError};

/// Default worker cap (`search.max_concurrency`).
pub const DEFAULT_MAX_CONCURRENCY: usize = 8;

static MAX_CONCURRENCY: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_CONCURRENCY);

/// Sets the process-wide worker cap (at least 1).
pub fn set_max_concurrency(n: usize) {
    MAX_CONCURRENCY.store(n.max(1), Ordering::Relaxed);
}

pub fn max_concurrency() -> usize {
    MAX_CONCURRENCY.load(Ordering::Relaxed).max(1)
}

/// `f(index, corpus)` for every corpus, results in corpus order. With
/// thread-safe readers and more than one corpus, at most
/// [`max_concurrency`] scoped workers run (each entering the caller's
/// request deadline); otherwise everything runs on this thread. The first
/// error (in corpus order) is returned.
pub fn map_corpora<T, F>(corpora: &[Corpus<'_>], f: F) -> Result<Vec<T>, SearchError>
where
    T: Send,
    F: Fn(usize, Corpus<'_>) -> Result<T, SearchError> + Sync,
{
    let workers = max_concurrency().min(corpora.len());
    let sync: Option<Vec<&(dyn KnowledgeRead + Sync)>> =
        corpora.iter().map(|c| c.store.as_sync()).collect();
    let stores = match sync {
        Some(s) if workers > 1 => s,
        _ => return corpora.iter().enumerate().map(|(i, &c)| f(i, c)).collect(),
    };
    let builds: Vec<&str> = corpora.iter().map(|c| c.build_id).collect();
    let n = builds.len();
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<Result<T, SearchError>>>> =
        (0..corpora.len()).map(|_| Mutex::new(None)).collect();
    let token = docmonk_core::deadline::current();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (next, slots, stores, builds, f, token) =
                (&next, &slots, &stores, &builds, &f, token.clone());
            scope.spawn(move || {
                let _deadline = token.as_ref().map(docmonk_core::deadline::Token::enter);
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let c = Corpus {
                        store: stores[i] as &dyn KnowledgeRead,
                        build_id: builds[i],
                    };
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(i, c)))
                        .unwrap_or_else(|_| Err(SearchError("search worker panicked".into())));
                    *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(r);
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|s| {
            s.into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .unwrap_or_else(|| Err(SearchError("search worker did not run".into())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use docmonk_storage::error::Result;
    use docmonk_storage::knowledge::{ChunkRow, EntityRow, RelationshipRow};
    use docmonk_storage::search::{
        AttachmentProvenance, DocumentSearchRow, EntityLinkRow, EntitySearchRow, PathSearchRow,
    };
    use docmonk_storage::vectors::SubjectMeta;

    /// A thread-safe reader whose reads are never called.
    struct Remote(String);

    impl KnowledgeRead for Remote {
        fn source_id(&self) -> &str {
            &self.0
        }
        fn backend_kind(&self) -> &'static str {
            "opensearch"
        }
        fn as_sync(&self) -> Option<&(dyn KnowledgeRead + Sync)> {
            Some(self)
        }
        fn search_entities_exact(&self, _: &str, _: &str) -> Result<Vec<EntitySearchRow>> {
            unimplemented!()
        }
        fn search_entities_alias(&self, _: &str, _: &str, _: i64) -> Result<Vec<EntitySearchRow>> {
            unimplemented!()
        }
        fn search_entities_fts(&self, _: &str, _: &str, _: i64) -> Result<Vec<EntitySearchRow>> {
            unimplemented!()
        }
        fn search_document_titles(
            &self,
            _: &str,
            _: &str,
            _: i64,
        ) -> Result<Vec<DocumentSearchRow>> {
            unimplemented!()
        }
        fn search_chunks_fts(
            &self,
            _: &str,
            _: &str,
            _: i64,
            _: i64,
        ) -> Result<Vec<DocumentSearchRow>> {
            unimplemented!()
        }
        fn search_paths_projection(&self, _: &str, _: &str, _: i64) -> Result<Vec<PathSearchRow>> {
            unimplemented!()
        }
        fn symbol_entities(&self, _: &str, _: &str) -> Result<Vec<EntityRow>> {
            unimplemented!()
        }
        fn entity(&self, _: &str, _: &str) -> Result<Option<EntityRow>> {
            unimplemented!()
        }
        fn entity_edges(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: i64,
        ) -> Result<Vec<RelationshipRow>> {
            unimplemented!()
        }
        fn unresolved_edges(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: i64,
        ) -> Result<Vec<RelationshipRow>> {
            unimplemented!()
        }
        fn file_rel_path(&self, _: &str, _: &str) -> Result<Option<String>> {
            unimplemented!()
        }
        fn entity_links(&self, _: &str, _: &str) -> Result<Vec<EntityLinkRow>> {
            unimplemented!()
        }
        fn document_attachment(&self, _: &str, _: &str) -> Result<Option<AttachmentProvenance>> {
            unimplemented!()
        }
        fn chunk(&self, _: &str, _: &str) -> Result<Option<ChunkRow>> {
            unimplemented!()
        }
        fn chunk_at(&self, _: &str, _: &str, _: i64) -> Result<Option<ChunkRow>> {
            unimplemented!()
        }
        fn chunk_siblings(
            &self,
            _: &str,
            _: &ChunkRow,
            _: i64,
            _: i64,
        ) -> Result<(Vec<ChunkRow>, Vec<ChunkRow>)> {
            unimplemented!()
        }
        fn subject_meta(&self, _: &str, _: &[(String, String)]) -> Result<Vec<SubjectMeta>> {
            unimplemented!()
        }
    }

    /// 150 server sources: results in corpus order, never more than the
    /// cap running at once, the first error (in corpus order) returned.
    #[test]
    fn bounded_ordered_and_failing_in_order() {
        let readers: Vec<Remote> = (0..150).map(|i| Remote(format!("s{i:03}"))).collect();
        let corpora: Vec<Corpus<'_>> = readers
            .iter()
            .map(|r| Corpus {
                store: r,
                build_id: "b",
            })
            .collect();
        set_max_concurrency(4);
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let ids = map_corpora(&corpora, |i, c| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(1));
            running.fetch_sub(1, Ordering::SeqCst);
            Ok((i, c.store.source_id().to_owned()))
        })
        .unwrap();
        assert_eq!(ids.len(), 150);
        assert!(ids
            .iter()
            .enumerate()
            .all(|(i, (j, s))| i == *j && *s == format!("s{i:03}")));
        let p = peak.load(Ordering::SeqCst);
        assert!((1..=4).contains(&p), "peak {p}");
        let err = map_corpora(&corpora, |i, _| {
            if i == 7 || i == 90 {
                Err(SearchError(format!("fail {i}")))
            } else {
                Ok(i)
            }
        })
        .unwrap_err();
        assert_eq!(err.0, "fail 7");
        set_max_concurrency(DEFAULT_MAX_CONCURRENCY);
    }
}
