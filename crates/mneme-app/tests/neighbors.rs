use mneme_app::neighbors::{MAX_NEIGHBOR_PAGE_BYTES, PreparedNeighbors};
use mneme_body::InlineStore;
use mneme_core::episode::OccurrenceSpan;
use mneme_core::ports::{GraphStore, SystemClock};
use mneme_core::{Edge, EdgeKind, NodeId, Provenance};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::{Config, EpisodeWrite, Ingest, Memory};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};
use ulid::Ulid;

fn fixture() -> (Memory, Arc<MemStore>) {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = Memory::new(
        store.clone(),
        store.clone(),
        store.clone(),
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config {
            graph_slot_cap: 0,
            ..Config::default()
        },
    )
    .with_body_store(Arc::new(InlineStore::new()));
    (mem, store)
}
async fn note(mem: &Memory, text: &str) -> NodeId {
    mem.ingest(Ingest::new(
        text,
        text.as_bytes(),
        &[],
        Provenance::derived_empty(),
    ))
    .await
    .unwrap()
}
async fn read(mem: &Memory, db: Ulid, id: NodeId, limit: usize, after: Option<&str>) -> Value {
    let mut args = json!({"id":id,"limit":limit});
    if let Some(after) = after {
        args["after"] = json!(after);
    }
    PreparedNeighbors::parse(&args)
        .unwrap()
        .run(mem, db)
        .await
        .unwrap()
}

#[test]
fn neighbors_admission_is_closed_and_bounded() {
    let id = NodeId(Ulid::new());
    for bad in [
        json!({}),
        json!({"id":id,"limit":0}),
        json!({"id":id,"limit":65}),
        json!({"id":id,"limit":1.5}),
        json!({"id":id,"limit":null}),
        json!({"id":id,"after":null}),
        json!({"id":id,"after":""}),
        json!({"id":id,"after":"wrong-version"}),
        json!({"id":id,"all":true}),
    ] {
        assert!(PreparedNeighbors::parse(&bad).is_err(), "admitted {bad}");
    }
    assert_eq!(
        PreparedNeighbors::parse(&json!({"id":id}))
            .unwrap()
            .into_json()["limit"],
        32
    );
}

#[tokio::test]
async fn pages_visit_more_than_64_raw_edges_both_legs_without_mutation() {
    let (mem, store) = fixture();
    let db = Ulid::new();
    let hub = note(&mem, "hub").await;
    let mut expected = BTreeSet::new();
    for index in 0..80 {
        let id = note(&mem, &format!("endpoint-{index}")).await;
        for (from, to) in [(hub, id), (id, hub)] {
            store
                .put_edge(&Edge::new(from, to, 0.1, EdgeKind::Transition, 1))
                .await
                .unwrap();
            expected.insert((id.0.to_string(), from != hub));
        }
    }
    let before = store.export().canonical_value().unwrap();
    let mut after = None;
    let mut found = BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = read(&mem, db, hub, 17, after.as_deref()).await;
        pages += 1;
        assert!(page["items"].as_array().unwrap().len() <= 17);
        assert!(page["coverage"]["rows_scanned"].as_u64().unwrap() <= 17);
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_NEIGHBOR_PAGE_BYTES);
        for item in page["items"].as_array().unwrap() {
            assert!(found.insert((
                item["neighbor"].as_str().unwrap().to_owned(),
                item["incoming"].as_bool().unwrap()
            )));
        }
        after = page["next_cursor"].as_str().map(str::to_owned);
        assert_eq!(page["has_more"].as_bool().unwrap(), after.is_some());
        if after.is_none() {
            break;
        }
        assert!(pages < 30, "non-progressing page sequence");
    }
    assert_eq!(found, expected);
    assert!(pages > 4);
    assert_eq!(store.export().canonical_value().unwrap(), before);
}

#[tokio::test]
async fn episodes_dangling_summaries_and_cursor_scope_are_preserved() {
    let (mem, store) = fixture();
    let db = Ulid::new();
    let hub = note(&mem, "hub").await;
    let episode = mem
        .append_episode(EpisodeWrite::new(
            "neighbor-test",
            "scene",
            "fixture",
            None,
            None,
            "scene",
            b"body",
            &[],
            OccurrenceSpan::Unknown,
            None,
        ))
        .await
        .unwrap()
        .identity
        .edition_id;
    let missing = NodeId(Ulid::new());
    let long = note(&mem, &"é".repeat(1000)).await;
    for id in [episode, missing, long] {
        store
            .put_edge(&Edge::new(hub, id, 0.9, EdgeKind::DerivedFrom, 1))
            .await
            .unwrap();
    }
    let page = read(&mem, db, hub, 64, None).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(
        items
            .iter()
            .any(|item| item["neighbor"] == json!(episode) && item["summary"] == "scene")
    );
    assert!(
        items
            .iter()
            .any(|item| item["neighbor"] == json!(missing) && item["summary"] == "")
    );
    let long_item = items
        .iter()
        .find(|item| item["neighbor"] == json!(long))
        .unwrap();
    assert_eq!(long_item["summary"].as_str().unwrap().len(), 1024);
    assert_eq!(long_item["summary_truncated"], true);

    let first = read(&mem, db, hub, 1, None).await;
    let cursor = first["next_cursor"].as_str().unwrap();
    assert!(PreparedNeighbors::parse(&json!({"id":episode,"after":cursor})).is_err());
    let err = PreparedNeighbors::parse(&json!({"id":hub,"after":cursor}))
        .unwrap()
        .run(&mem, Ulid::new())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("database identity mismatch"));
    let continued = read(&mem, db, hub, 2, Some(cursor)).await;
    assert!(continued["returned"].as_u64().unwrap() > 0);
    assert_eq!(
        read(&mem, db, missing, 32, None).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let absent = NodeId(Ulid::new());
    let empty = read(&mem, db, absent, 32, None).await;
    assert_eq!(empty["returned"], 0);
    assert_eq!(empty["has_more"], false);
}

#[tokio::test]
async fn escaped_full_page_is_bounded_without_rejecting_valid_summaries() {
    let (mem, store) = fixture();
    let hub = note(&mem, "hub").await;
    for index in 0..64 {
        let id = note(
            &mem,
            &format!("escaped endpoint {index} {}", "\u{01}".repeat(2000)),
        )
        .await;
        store
            .put_edge(&Edge::new(hub, id, 0.5, EdgeKind::Transition, 1))
            .await
            .unwrap();
    }
    let page = read(&mem, Ulid::new(), hub, 64, None).await;
    assert_eq!(page["returned"], 64);
    assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_NEIGHBOR_PAGE_BYTES);
    for item in page["items"].as_array().unwrap() {
        assert_eq!(item["summary_truncated"], true);
        assert!(serde_json::to_vec(&item["summary"]).unwrap().len() <= 1026);
    }
}
