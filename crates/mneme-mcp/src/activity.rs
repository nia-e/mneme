//! Disposable visual telemetry, never a memory or delivery correctness boundary.
//!
//! Only IDs from successful prepared responses enter this process-local ring.
//! Observation cannot block a memory call: contention drops the event. No query,
//! summary, body, caller identity, receipt, or persistent handle is retained.

use std::collections::VecDeque;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use mneme_core::NodeId;
use serde::Serialize;
use ulid::Ulid;

const CAPACITY: usize = 256;
pub(crate) const MAX_PAGE: usize = 64;
pub(crate) const MAX_IDS: usize = 32;
const MAX_DB_BYTES: usize = 128;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Event {
    seq: u64,
    timestamp_ms: u64,
    tool: &'static str,
    db: String,
    db_id: String,
    node_ids: Vec<String>,
    node_ids_truncated: bool,
    db_truncated: bool,
}

#[derive(Default)]
struct State {
    latest: u64,
    events: VecDeque<Event>,
}

pub(crate) struct ActivityRing {
    instance: String,
    state: Mutex<State>,
    dropped: AtomicU64,
}

impl Default for ActivityRing {
    fn default() -> Self {
        Self {
            instance: Ulid::new().to_string(),
            state: Mutex::new(State::default()),
            dropped: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Page {
    schema: &'static str,
    instance: String,
    events: Vec<Event>,
    next_after: u64,
    oldest_seq: Option<u64>,
    latest_seq: Option<u64>,
    missed: bool,
    dropped: u64,
    has_more: bool,
    busy: bool,
}

impl ActivityRing {
    /// The caller supplies only typed, returned IDs. Even iterator consumption
    /// is capped; no failed telemetry operation propagates into memory work.
    pub(crate) fn returned(
        &self,
        tool: &'static str,
        db: &str,
        db_id: Ulid,
        ids: impl IntoIterator<Item = NodeId>,
    ) {
        let Ok(mut state) = self.state.try_lock() else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(seq) = state.latest.checked_add(1) else {
            return;
        };
        let mut ids = ids.into_iter();
        let node_ids = ids
            .by_ref()
            .take(MAX_IDS)
            .map(|id| id.0.to_string())
            .collect();
        let node_ids_truncated = ids.next().is_some();
        let mut db_end = db.len().min(MAX_DB_BYTES);
        while !db.is_char_boundary(db_end) {
            db_end -= 1;
        }
        let event = Event {
            seq,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
                }),
            tool,
            db: db[..db_end].to_owned(),
            db_id: db_id.to_string(),
            node_ids,
            node_ids_truncated,
            db_truncated: db_end != db.len(),
        };
        if state.events.len() == CAPACITY {
            state.events.pop_front();
        }
        state.events.push_back(event);
        state.latest = seq;
    }

    /// Already bounded by typed tool admission. Polling has no store checkout,
    /// cold permit, event emission, or waiting lock acquisition.
    pub(crate) fn page(&self, after: u64, limit: usize) -> Page {
        let mut page = Page {
            schema: "mneme.activity.v1",
            instance: self.instance.clone(),
            events: Vec::new(),
            next_after: after,
            oldest_seq: None,
            latest_seq: None,
            missed: false,
            dropped: self.dropped.load(Ordering::Relaxed),
            has_more: false,
            busy: true,
        };
        let Ok(state) = self.state.try_lock() else {
            return page;
        };
        let oldest = state.events.front().map(|event| event.seq);
        let effective_after = if after > state.latest { 0 } else { after };
        page.missed = after > state.latest
            || oldest.is_some_and(|oldest| effective_after < oldest.saturating_sub(1));
        page.events = state
            .events
            .iter()
            .filter(|event| event.seq > effective_after)
            .take(limit.min(MAX_PAGE))
            .cloned()
            .collect();
        page.next_after = page
            .events
            .last()
            .map_or(effective_after, |event| event.seq);
        page.oldest_seq = oldest;
        page.latest_seq = Some(state.latest);
        page.has_more = page.next_after < state.latest;
        page.busy = false;
        page
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit(ring: &ActivityRing) {
        ring.returned(
            "query",
            "project",
            Ulid::from(1_u128),
            [NodeId(Ulid::from(2_u128))],
        );
    }

    #[test]
    fn activity_retention_cursor_and_restart_are_bounded() {
        let ring = ActivityRing::default();
        for _ in 0..300 {
            emit(&ring);
        }
        let page = ring.page(0, MAX_PAGE);
        assert_eq!(page.oldest_seq, Some(45));
        assert_eq!(page.latest_seq, Some(300));
        assert_eq!(page.events.len(), 64);
        assert_eq!(page.next_after, 108);
        assert!(page.missed && page.has_more && !page.busy);
        assert_eq!(ring.page(page.next_after, 1).events[0].seq, 109);
        let tail = ring.page(300, MAX_PAGE);
        assert!(tail.events.is_empty() && !tail.has_more && !tail.missed);
        assert_eq!(tail.next_after, 300);
        let fresh = ActivityRing::default();
        assert_ne!(fresh.instance, ring.instance);
        let reset = fresh.page(300, MAX_PAGE);
        assert!(reset.missed);
        assert_eq!(reset.next_after, 0);
    }

    #[test]
    fn activity_contention_drops_writes_and_preserves_poll_cursor() {
        let ring = ActivityRing::default();
        let held = ring.state.lock().unwrap();
        emit(&ring);
        let page = ring.page(12, MAX_PAGE);
        assert!(page.busy && page.events.is_empty());
        assert_eq!(page.next_after, 12);
        assert_eq!(page.dropped, 1);
        drop(held);
        assert_eq!(ring.page(0, MAX_PAGE).latest_seq, Some(0));
    }

    #[test]
    fn activity_payload_and_iterator_work_stay_bounded() {
        let ring = ActivityRing::default();
        let db = "界".repeat(100);
        for _ in 0..MAX_PAGE {
            let mut count = 0;
            ring.returned(
                "recall_context",
                &db,
                Ulid::from(1_u128),
                std::iter::from_fn(|| {
                    count += 1;
                    assert!(count <= MAX_IDS + 1);
                    Some(NodeId(Ulid::from(count as u128)))
                }),
            );
        }
        let page = ring.page(0, MAX_PAGE);
        let first = &page.events[0];
        assert!(first.db_truncated && first.node_ids_truncated);
        assert!(first.db.len() <= MAX_DB_BYTES);
        assert_eq!(first.node_ids.len(), MAX_IDS);
        let encoded = serde_json::to_vec(&page).unwrap();
        assert!(encoded.len() < crate::response::MAX_TOOL_TEXT_BYTES);
        let value = serde_json::to_value(&page).unwrap();
        let fields = value["events"][0].as_object().unwrap();
        assert_eq!(fields.len(), 8);
        for forbidden in ["body", "summary", "text", "receipt", "query"] {
            assert!(!fields.contains_key(forbidden));
        }
    }
}
