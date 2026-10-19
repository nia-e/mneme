//! Shared in-memory context fixtures; no test suite is reused as support code.

use std::sync::Arc;

use mneme_body::InlineStore;
use mneme_core::episode::{EpisodeThread, EpisodeTime, OccurrenceSpan};
use mneme_core::ports::SystemClock;
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Config, EpisodeWrite, Memory};

pub(super) fn fixture() -> Memory {
    fixture_with_store().0
}

pub(super) fn fixture_with_store() -> (Memory, Arc<MemStore>) {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let memory = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            graph_seed_cap: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()))
    .with_lexical_index(store.clone());
    (memory, store)
}

pub(super) fn episode_write<'a>(key: &'a str, summary: &'a str) -> EpisodeWrite<'a> {
    EpisodeWrite::new(
        "context-test",
        key,
        "fixture://automatic-context",
        None,
        None,
        summary,
        b"episodic body must never enter ordinary context",
        &["scope"],
        OccurrenceSpan::Point {
            at: EpisodeTime::new(1234).unwrap(),
        },
        Some(EpisodeThread::new("violet-evening").unwrap()),
    )
}
