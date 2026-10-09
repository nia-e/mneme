use super::*;
use mneme_core::ports::{GraphStore, SystemClock, Traversal, VectorIndex};
use mneme_core::{
    BodyRef, BoundedTagSet, Confidence, NodeInit, NodeSummary, Provenance, Stability,
};
use mneme_cozo::MemStore;
use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
use mneme_engine::Config;
use std::sync::Arc;

fn memory<T: GraphStore + VectorIndex + Traversal + 'static>(store: Arc<T>) -> Memory {
    Memory::new(
        store.clone(),
        store.clone(),
        store,
        Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
        Arc::new(SystemClock),
        Config::default(),
    )
    .with_body_store(Arc::new(mneme_body::InlineStore::new()))
}

fn node(index: u128, summary: &str, tags: Vec<String>, status: NodeStatus) -> Node {
    Node::new(NodeInit {
        id: NodeId(Ulid(index)),
        summary: NodeSummary::new(summary).unwrap(),
        body: BodyRef::new("file:///never-read-body").unwrap(),
        tags: BoundedTagSet::try_from_iter(tags).unwrap(),
        provenance: Provenance::Conversation {
            session: Ulid(0),
            turn: 0,
        },
        stability: Stability::new(0.5).unwrap(),
        confidence: Confidence::new(0.5).unwrap(),
        status,
        created: index,
    })
}

async fn page(memory: &Memory, db_id: Ulid, args: Value) -> Value {
    PreparedList::parse(&args)
        .unwrap()
        .run(memory, db_id)
        .await
        .unwrap()
}

#[test]
fn nodes_typed_admission_and_forwarding_keep_defaults_and_reject_invalid_requests() {
    assert_eq!(
        PreparedList::parse(&json!({})).unwrap().into_json(),
        json!({"kind":"nodes","status":"all","limit":50})
    );
    for bad in [
        json!([]),
        json!({"kind":"notes"}),
        json!({"kind":null}),
        json!({"status":null}),
        json!({"status":"candidate"}),
        json!({"limit":0}),
        json!({"limit":65}),
        json!({"limit":1.5}),
        json!({"limit":null}),
        json!({"tag":null}),
        json!({"tag":""}),
        json!({"tag":" spaced "}),
        json!({"tag":"x".repeat(257)}),
        json!({"after":null}),
        json!({"after":"bad"}),
        json!({"after":"x".repeat(1025)}),
        json!({"body":true}),
    ] {
        assert!(PreparedList::parse(&bad).is_err(), "accepted {bad}");
    }
    assert!(PreparedList::parse(&json!({"kind":"touchstones","limit":32})).is_ok());
    assert!(PreparedList::parse(&json!({"kind":"touchstones","limit":33})).is_err());
}

async fn check_inventory<T: GraphStore + VectorIndex + Traversal + 'static>(store: Arc<T>) {
    let mem = memory(store.clone());
    let db_id = Ulid(9000);
    for index in 1..=300 {
        store
            .put_node(&node(
                index,
                "Inventory",
                if index == 300 {
                    vec!["rare".into()]
                } else {
                    vec![]
                },
                if index % 2 == 0 {
                    NodeStatus::Archived
                } else {
                    NodeStatus::Active
                },
            ))
            .await
            .unwrap();
    }
    let mut after = None;
    let mut ids = Vec::new();
    let mut calls = 0;
    loop {
        let mut args = json!({"limit":64});
        if let Some(cursor) = after {
            args["after"] = cursor;
        }
        let result = page(&mem, db_id, args).await;
        assert!(result["coverage"]["scanned_nodes"].as_u64().unwrap() <= 256);
        assert!(result["coverage"]["hydrated_nodes"].as_u64().unwrap() <= 256);
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_LIST_PAGE_BYTES);
        ids.extend(
            result["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap().to_owned()),
        );
        calls += 1;
        if result["next_cursor"].is_null() {
            break;
        }
        after = Some(result["next_cursor"].clone());
        assert!(calls < 10);
    }
    assert_eq!(calls, 5);
    assert_eq!(ids.len(), 300);
    assert!(ids.windows(2).all(|window| window[0] < window[1]));
    let first = page(&mem, db_id, json!({"tag":"rare"})).await;
    assert!(first["items"].as_array().unwrap().is_empty());
    assert_eq!(first["coverage"]["scanned_nodes"], 256);
    assert_eq!(first["coverage"]["stopped"], "scan_budget");
    assert!(first["has_more"].as_bool().unwrap());
    let cursor = first["next_cursor"].clone();
    assert!(PreparedList::parse(&json!({"after":cursor})).is_err());
    assert!(PreparedList::parse(&json!({"tag":"rare","status":"active","after":cursor})).is_err());
    assert!(
        PreparedList::parse(&json!({"tag":"rare","after":cursor}))
            .unwrap()
            .run(&mem, Ulid(9001))
            .await
            .unwrap_err()
            .to_string()
            .contains("identity mismatch")
    );
    // The upper key is frozen: a newly added key after pass start is omitted,
    // but no lifetime cap prevents the next fresh inventory from seeing it.
    store
        .put_node(&node(
            301,
            "Added later",
            vec!["rare".into()],
            NodeStatus::Active,
        ))
        .await
        .unwrap();
    let second = page(&mem, db_id, json!({"tag":"rare","after":cursor})).await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["items"][0]["id"], Ulid(300).to_string());
    assert!(second["next_cursor"].is_null());
    let archived = page(&mem, db_id, json!({"tag":"rare","status":"archived"})).await;
    let archived = page(
        &mem,
        db_id,
        json!({"tag":"rare","status":"archived","after":archived["next_cursor"]}),
    )
    .await;
    assert_eq!(archived["items"][0]["status"], "archived");
    let active = page(&mem, db_id, json!({"status":"active","limit":1})).await;
    assert_eq!(active["items"][0]["id"], Ulid(1).to_string());
    let unchanged = mem.get_node(NodeId(Ulid(1))).await.unwrap().unwrap();
    assert_eq!(unchanged.exposure_count(), 0);
    assert_eq!(unchanged.last_exposed(), None);
    assert!(mem.neighbors(unchanged.id(), 1).await.unwrap().is_empty());
}

#[tokio::test]
async fn reference_inventory_is_paged_sparse_bounded_bound_to_identity_and_read_only() {
    check_inventory(Arc::new(MemStore::new(DEFAULT_DIM))).await;
}

#[tokio::test]
async fn inventory_byte_budget_truncates_utf8_summaries_without_skipping_nodes() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone());
    // Quotes force JSON escaping; preserve all 64 tags on each card.
    let tags = (0..64)
        .map(|index| format!("{index:02}{}", "\"".repeat(254)))
        .collect::<Vec<_>>();
    for index in 1..=12 {
        store
            .put_node(&node(
                index,
                &"é".repeat(1024),
                tags.clone(),
                NodeStatus::Active,
            ))
            .await
            .unwrap();
    }
    let mut args = json!({"limit":64});
    let mut ids = Vec::new();
    let mut pages = 0;
    loop {
        let result = page(&mem, Ulid(700), args.clone()).await;
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_LIST_PAGE_BYTES);
        for item in result["items"].as_array().unwrap() {
            assert_eq!(
                item["summary"].as_str().unwrap().len(),
                MAX_LIST_SUMMARY_BYTES
            );
            assert_eq!(item["summary_truncated"], true);
            assert_eq!(item["tags"].as_array().unwrap().len(), 64);
            ids.push(item["id"].clone());
        }
        pages += 1;
        if result["next_cursor"].is_null() {
            break;
        }
        assert_eq!(result["coverage"]["stopped"], "output_bytes");
        args["after"] = result["next_cursor"].clone();
        assert!(pages < 20);
    }
    assert!(pages > 1);
    assert_eq!(ids.len(), 12);
    ids.sort_by_key(|id| id.as_str().unwrap().to_owned());
    ids.dedup();
    assert_eq!(ids.len(), 12);
}

#[tokio::test]
async fn canonical_inventory_includes_exact_historical_episode_editions() {
    use crate::episode::PreparedEpisode;
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store);
    let db_id = Ulid(0);
    let first = PreparedEpisode::parse(&json!({"action":"append","summary":"Original scene",
        "source":{"namespace":"list-test","key":"first","reference":"test://episode"}}))
    .unwrap()
    .run(&mem, db_id, None)
    .await
    .unwrap();
    let second = PreparedEpisode::parse(&json!({"action":"revise","episode_id":first["episode_id"],
        "expected_edition_id":first["edition_id"],"reason":"Corrected the account","summary":"Corrected scene",
        "source":{"namespace":"list-test","key":"second","reference":"test://episode"}}))
        .unwrap().run(&mem, db_id, None).await.unwrap();
    let result = page(&mem, db_id, json!({})).await;
    assert_eq!(result["items"].as_array().unwrap().len(), 2);
    let ids = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].clone())
        .collect::<Vec<_>>();
    assert!(ids.contains(&first["edition_id"]));
    assert!(ids.contains(&second["edition_id"]));
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] == "episode")
    );
}

#[test]
fn tag_vocabulary_admission_is_closed_exact_and_bounded() {
    assert_eq!(
        PreparedList::parse(&json!({"kind":"tags"}))
            .unwrap()
            .into_json(),
        json!({"kind":"tags","status":"all","prefix":"","limit":50})
    );
    assert_eq!(
        PreparedList::parse(&json!({"kind":"tags","prefix":"Rust "}))
            .unwrap()
            .into_json()["prefix"],
        "Rust "
    );
    for bad in [
        json!({"kind":"tags","prefix":null}),
        json!({"kind":"tags","prefix":"\n"}),
        json!({"kind":"tags","prefix":"é".repeat(129)}),
        json!({"kind":"tags","status":"unknown"}),
        json!({"kind":"tags","after":null}),
        json!({"kind":"tags","after":"tags-v2:{}"}),
        json!({"kind":"tags","limit":65}),
        json!({"kind":"tags","limit":0}),
        json!({"kind":"tags","limit":1.5}),
        json!({"kind":"tags","tag":"rust"}),
        json!({"kind":"nodes","prefix":"rust"}),
        json!({"kind":"touchstones","prefix":"rust"}),
    ] {
        assert!(PreparedList::parse(&bad).is_err(), "accepted {bad}");
    }
}

#[tokio::test]
async fn tag_vocabulary_pages_bind_owner_selection_and_report_semantic_only_bounded_work() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone());
    for (index, tags, status) in [
        (
            1,
            vec!["Rust".into(), "rust".into(), "rust async".into()],
            NodeStatus::Active,
        ),
        (
            2,
            vec!["rust".into(), "rust-lang".into()],
            NodeStatus::Archived,
        ),
        (
            3,
            vec!["rust".into(), "rustacean".into()],
            NodeStatus::Active,
        ),
    ] {
        store
            .put_node(&node(index, "summary never returned here", tags, status))
            .await
            .unwrap();
    }
    let first = page(
        &mem,
        Ulid(700),
        json!({"kind":"tags","prefix":"rust","limit":1}),
    )
    .await;
    assert_eq!(first["items"][0]["name"], "rust");
    assert_eq!(
        first["items"][0]["count"],
        json!({"status":"exact","value":3})
    );
    assert_eq!(first["items"][0]["examples"].as_array().unwrap().len(), 3);
    assert_eq!(first["coverage"]["semantic_only"], true);
    assert_eq!(first["coverage"]["snapshot"], false);
    assert_eq!(first["has_more"], true);
    assert!(first["coverage"]["name_seeks"].as_u64().unwrap() <= 256);
    assert!(first["coverage"]["membership_rows"].as_u64().unwrap() <= 4096);
    let cursor = first["next_cursor"].clone();
    assert!(PreparedList::parse(&json!({"kind":"tags","after":cursor})).is_err());
    assert!(PreparedList::parse(&json!({"kind":"tags","prefix":"Rust","after":cursor})).is_err());
    assert!(
        PreparedList::parse(
            &json!({"kind":"tags","prefix":"rust","status":"active","after":cursor})
        )
        .is_err()
    );
    let continuation = json!({"kind":"tags","prefix":"rust","after":cursor});
    assert!(
        PreparedList::parse(&continuation)
            .unwrap()
            .run(&mem, Ulid(701))
            .await
            .unwrap_err()
            .to_string()
            .contains("identity mismatch")
    );
    let second = page(&mem, Ulid(700), continuation).await;
    assert_eq!(
        second["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["rust async", "rust-lang", "rustacean"]
    );
    assert!(second["next_cursor"].is_null());
    let active = page(
        &mem,
        Ulid(700),
        json!({"kind":"tags","prefix":"rust","status":"active"}),
    )
    .await;
    assert_eq!(
        active["items"][0]["count"],
        json!({"status":"exact","value":2})
    );
    assert_eq!(active["items"].as_array().unwrap().len(), 3);
    let unchanged = mem.get_node(NodeId(Ulid(1))).await.unwrap().unwrap();
    assert_eq!(unchanged.exposure_count(), 0);
    assert_eq!(unchanged.last_exposed(), None);
    assert!(mem.neighbors(unchanged.id(), 1).await.unwrap().is_empty());
}

#[tokio::test]
async fn tag_vocabulary_sparse_filtered_pages_keep_progress_without_inference() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store.clone());
    for index in 1..=200 {
        store
            .put_node(&node(
                index,
                "note",
                vec![format!("tag-{index:03}")],
                if index == 200 {
                    NodeStatus::Active
                } else {
                    NodeStatus::Archived
                },
            ))
            .await
            .unwrap();
    }
    let first = page(&mem, Ulid(800), json!({"kind":"tags","status":"active"})).await;
    assert!(first["items"].as_array().unwrap().is_empty());
    assert_eq!(first["coverage"]["stopped"], "seek_budget");
    assert!(first["next_cursor"].is_string());
    let second = page(
        &mem,
        Ulid(800),
        json!({"kind":"tags","status":"active","after":first["next_cursor"]}),
    )
    .await;
    assert_eq!(second["items"][0]["name"], "tag-200");
    assert!(second["next_cursor"].is_null());
}

#[tokio::test]
async fn tag_vocabulary_does_not_promote_episode_tags_to_semantic_vocabulary() {
    let store = Arc::new(MemStore::new(DEFAULT_DIM));
    let mem = memory(store);
    crate::episode::PreparedEpisode::parse(&json!({"action":"append","summary":"Historical account",
        "tags":["episode-only"],"source":{"namespace":"tag-list-test","key":"first","reference":"test://episode"}}))
        .unwrap().run(&mem, Ulid(801), None).await.unwrap();
    let result = page(&mem, Ulid(801), json!({"kind":"tags"})).await;
    assert!(result["items"].as_array().unwrap().is_empty());
    assert_eq!(result["coverage"]["semantic_only"], true);
}
