//! Opt-in, non-authorizing provenance for the bounded shadow observations.
//!
//! The native packer identifies returned cards, not cards an adapter eventually
//! delivers or an agent uses. No host registry, receipt, exposure or mutation is
//! created here. An adapter must record its own delivered subset separately.

use mneme_app::{ContextObservation, ContextWindow, ObservedContext};
use mneme_present::{PackPlan, PresentationBudget};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::num::NonZeroU32;

use mneme_core::ports::{
    MAX_ROUTING_HINT_BYTES, MAX_ROUTING_HINTS, RoutingBinding, RoutingDiagnostics, RoutingHint,
    SignedRoutingBias,
};
use serde::Deserialize;
use ulid::Ulid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireHint {
    db_id: Ulid,
    route: RoutingBinding,
    sign: SignedRoutingBias,
}

pub(crate) struct ParsedHints {
    entries: Vec<(Ulid, RoutingHint)>,
    pub ignored: usize,
}
impl ParsedHints {
    pub fn for_database(self, db_id: Ulid) -> (Vec<RoutingHint>, usize) {
        let mut ignored = self.ignored;
        let hints = self
            .entries
            .into_iter()
            .filter_map(|(db, hint)| {
                if db == db_id {
                    Some(hint)
                } else {
                    ignored += 1;
                    None
                }
            })
            .collect();
        (hints, ignored)
    }
}

/// Parse optional hints before DB checkout. Invalid optional learning is neutral,
/// never a lossy coercion or a reason to deny ordinary recall.
pub(crate) fn parse_hints(request: &Value) -> Option<ParsedHints> {
    let value = request.get("routing_hints")?;
    let Some(array) = value.as_array() else {
        return Some(ParsedHints {
            entries: Vec::new(),
            ignored: 1,
        });
    };
    if array.len() > MAX_ROUTING_HINTS
        || serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > MAX_ROUTING_HINT_BYTES)
    {
        return Some(ParsedHints {
            entries: Vec::new(),
            ignored: array.len(),
        });
    }
    let mut result = ParsedHints {
        entries: Vec::new(),
        ignored: 0,
    };
    for value in array {
        let parsed = serde_json::from_value::<WireHint>(value.clone())
            .ok()
            .filter(|hint| hint.route.has_canonical_fingerprints());
        if let Some(hint) = parsed {
            result.entries.push((
                hint.db_id,
                RoutingHint {
                    route: hint.route,
                    sign: hint.sign,
                },
            ));
        } else {
            // A malformed opposing row must not disappear and manufacture an
            // authoritative surviving signed prefix. Match typed admission.
            return Some(ParsedHints {
                entries: Vec::new(),
                ignored: array.len(),
            });
        }
    }
    Some(result)
}

pub(crate) fn hints_schema() -> Value {
    let id = json!({"type":"string", "pattern":"^[0-7][0-9A-HJKMNP-TV-Z]{25}$"});
    let fingerprint = json!({"type":"string", "pattern":"^[0-9a-f]{64}$"});
    json!({"type":"array", "maxItems":MAX_ROUTING_HINTS,
        "description":format!("Optional query-local +/-0.25 route ordering. A current positive hint may also nominate its target as a conditional recommendation, without a graph walk or query-similarity floor; weaken never nominates or globally excludes a target. Conditional targets obey the requested tag filter and explicit k/max_nodes/depth zero controls. The whole canonical compact array must fit {MAX_ROUTING_HINT_BYTES} bytes; maxItems is derived from that byte allowance, not a relevance quota. Oversized or malformed batches are wholly neutral, never a signed prefix. Stale or wrong-db hints are ignored with baseline fallback. Request observe:true to distinguish conditional-entry provenance from actual graph paths; absent observations mean origin unavailable, not direct. Empty array opts into native binding observations. No writes or feedback authority."),
        "items":{"type":"object","additionalProperties":false,"required":["db_id","route","sign"],
            "properties":{"db_id":id,"sign":{"type":"string","enum":["boost","weaken"]},
                "route":{"type":"object","additionalProperties":false,
                    "required":["previous","target","from","to","previous_fingerprint","target_fingerprint","edge_fingerprint"],
                    "properties":{"previous":id,"target":id,"from":id,"to":id,
                        "previous_fingerprint":fingerprint,"target_fingerprint":fingerprint,"edge_fingerprint":fingerprint}}}}})
}

pub(crate) fn render(
    plan: &PackPlan,
    observations: &[ContextObservation],
    max_bytes: usize,
) -> Result<String, crate::AnyErr> {
    render_inner(plan, Some(observations), max_bytes, None)
}

/// Routed provenance is never silently discarded to make an oversized result fit.
pub(crate) fn render_routed(
    plan: &PackPlan,
    observations: Option<&[ContextObservation]>,
    db_id: Ulid,
    diagnostics: &RoutingDiagnostics,
    max_bytes: usize,
) -> Result<String, crate::AnyErr> {
    render_inner(plan, observations, max_bytes, Some((db_id, diagnostics)))
}

/// A typed exact-envelope overflow, distinct from encoding or identity failure.
#[derive(Debug)]
pub(crate) struct EnvelopeOverflow {
    pub required_bytes: usize,
    pub max_bytes: usize,
}
impl std::fmt::Display for EnvelopeOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "observed recall requires {} bytes, exceeding the {}-byte context limit; reduce k, max_nodes or depth",
            self.required_bytes, self.max_bytes
        )
    }
}
impl std::error::Error for EnvelopeOverflow {}

/// Pure bounded repacking of one retrieved window. Metadata costs can change
/// nonmonotonically across lane substitutions, so only exact final encoding
/// establishes fit; the packing allowance itself always decreases.
pub(crate) fn pack_and_render(
    window: &ContextWindow,
    initial_budget: &PresentationBudget,
    observe: bool,
    routing: Option<(Ulid, usize)>,
    max_bytes: usize,
) -> Result<(ObservedContext, String), crate::AnyErr> {
    let mut budget = *initial_budget;
    let attempts = 2 + (u32::BITS - budget.max_content_bytes().leading_zeros());
    for attempt in 0..attempts {
        let mut context = window.pack(&budget)?;
        if let Some((_, preignored)) = routing {
            context.routing.get_or_insert_with(Default::default).ignored += preignored;
        }
        let rendered = if let Some((db_id, _)) = routing {
            render_routed(
                &context.plan,
                observe.then_some(context.observations.as_slice()),
                db_id,
                context.routing.as_ref().expect("routed diagnostics"),
                max_bytes,
            )
        } else if observe {
            render(&context.plan, &context.observations, max_bytes)
        } else {
            let rendered = context.plan.rendered_content();
            if rendered.len() > max_bytes {
                Err(Box::new(EnvelopeOverflow {
                    required_bytes: rendered.len(),
                    max_bytes,
                }) as crate::AnyErr)
            } else {
                Ok(rendered.to_owned())
            }
        };
        match rendered {
            Ok(rendered) => return Ok((context, rendered)),
            Err(error) => {
                let Some(overflow) = error.downcast_ref::<EnvelopeOverflow>() else {
                    return Err(error);
                };
                let current = budget.max_content_bytes();
                let half = (current / 2).max(1);
                let shrink = if attempt == 0 {
                    (overflow.required_bytes - overflow.max_bytes).min(half as usize) as u32
                } else {
                    half
                };
                let remaining = current
                    .checked_sub(shrink.max(1))
                    .and_then(NonZeroU32::new)
                    .ok_or(error)?;
                budget = PresentationBudget::new(
                    remaining,
                    NonZeroU32::new(budget.control_reserve_bytes()).expect("validated reserve"),
                    std::num::NonZeroU16::new(budget.max_items()).expect("validated items"),
                    std::num::NonZeroU16::new(budget.max_summary_bytes_each())
                        .expect("validated summaries"),
                    budget.body(),
                    budget.lanes(),
                )?;
            }
        }
    }
    Err("observed recall exhausted its bounded local packing allowance; reduce k, max_nodes or depth".into())
}

fn render_inner(
    plan: &PackPlan,
    observations: Option<&[ContextObservation]>,
    max_bytes: usize,
    routing: Option<(Ulid, &RoutingDiagnostics)>,
) -> Result<String, crate::AnyErr> {
    let mut envelope: Value = serde_json::from_str(plan.rendered_content())?;
    if let Some(observations) = observations {
        let cards: Vec<_> = observations
            .iter()
            .map(|card| {
                // The MCP extension re-encodes the envelope as a JSON value. Its
                // object-key order need not match the presenter's typed encoding;
                // bind the exact final card bytes, not the earlier packer's bytes.
                let lane = match card.lane {
                    mneme_present::Lane::Core => "core",
                    mneme_present::Lane::Primary => "primary",
                    mneme_present::Lane::Expansion => "expansions",
                    mneme_present::Lane::Episodic => "episodes",
                };
                let emitted = envelope[lane]
                    .as_array()
                    .and_then(|cards| {
                        cards
                            .iter()
                            .find(|c| c["id"].as_str() == Some(card.node_id.0.to_string().as_str()))
                    })
                    .ok_or("observation did not match an emitted card")?;
                let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(emitted)?));
                let path = card.graph_path.as_ref().map(|path| {
                    path.iter()
                        .map(|hop| {
                            json!({
                                "previous": hop.previous.0.to_string(),
                                "target": hop.target.0.to_string(),
                                "from": hop.edge.from.0.to_string(),
                                "to": hop.edge.to.0.to_string(),
                                "kind": crate::kind_str(hop.edge.kind),
                                "anchor": hop.edge.anchor,
                            })
                        })
                        .collect::<Vec<_>>()
                });
                let mut observed = json!({
                    "node_id": card.node_id.0.to_string(),
                    "lane": card.lane,
                    "card_sha256": digest,
                    "graph_path": path,
                });
                if let Some(route) = &card.conditional_binding {
                    if card.lane != mneme_present::Lane::Primary
                        || card.routing_binding.is_some()
                        || card
                            .graph_path
                            .as_ref()
                            .is_some_and(|path| !path.is_empty())
                        || route.target != card.node_id
                        || !route.has_canonical_fingerprints()
                    {
                        return Err("invalid conditional-entry observation".into());
                    }
                    let (db_id, _) = routing
                        .ok_or("conditional-entry observation requires database provenance")?;
                    observed["entry_kind"] = json!("conditional");
                    observed["conditional_binding"] =
                        json!({"db_id":db_id.to_string(),"route":route});
                } else if let Some((db_id, _)) = routing {
                    observed["routing_binding"] = card.routing_binding.as_ref().map_or(
                        Value::Null,
                        |route| json!({"db_id":db_id.to_string(),"route":route}),
                    );
                }
                Ok::<_, crate::AnyErr>(observed)
            })
            .collect::<Result<_, _>>()?;
        envelope["observation"] = json!({
            "schema": 1,
            "learning": "disabled",
            "cards": cards,
        });
    }
    if let Some((_, diagnostics)) = routing {
        envelope["routing"] = json!({"validated":diagnostics.validated,"ignored":diagnostics.ignored,"learning":"disabled"});
    }
    encode_exact(envelope, max_bytes)
}

fn encode_exact(mut envelope: Value, max_bytes: usize) -> Result<String, crate::AnyErr> {
    // The extension changes total control bytes but no packed card. Recompute
    // the self-reported size of this final envelope, including its own digits.
    // The sidecar is never allowed past the ordinary total context ceiling.
    for _ in 0..16 {
        let encoded = serde_json::to_string(&envelope)?;
        if envelope["usage"]["content_bytes"].as_u64() == Some(encoded.len() as u64) {
            if encoded.len() > max_bytes {
                return Err(Box::new(EnvelopeOverflow {
                    required_bytes: encoded.len(),
                    max_bytes,
                }));
            }
            return Ok(encoded);
        }
        envelope["usage"]["content_bytes"] = json!(encoded.len());
    }
    Err("observed recall size accounting did not converge".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RoutingWindowEmbedder;
    #[async_trait::async_trait]
    impl mneme_core::ports::Embedder for RoutingWindowEmbedder {
        fn dim(&self) -> usize {
            4
        }
        fn fingerprint(&self) -> mneme_core::EmbeddingFingerprint {
            mneme_core::EmbeddingFingerprint::new(
                "routing-window-fixture",
                4,
                "l2-f32-v1",
                "symmetric-v1",
            )
        }
        async fn embed(&self, texts: &[&str]) -> mneme_core::ports::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
        }
    }

    fn hint() -> Value {
        json!({"db_id":"00000000000000000000000001", "sign":"boost", "route":{
            "previous":"00000000000000000000000002", "target":"00000000000000000000000003",
            "from":"00000000000000000000000002", "to":"00000000000000000000000003",
            "previous_fingerprint":"a".repeat(64),"target_fingerprint":"b".repeat(64),"edge_fingerprint":"c".repeat(64)}})
    }
    #[test]
    fn routing_optional_parser_is_bounded_and_neutral_on_invalid_or_wrong_db() {
        assert!(parse_hints(&json!({})).is_none());
        let db = Ulid::from(1u128);
        let (hints, ignored) = parse_hints(&json!({"routing_hints":[hint()]}))
            .unwrap()
            .for_database(db);
        assert_eq!((hints.len(), ignored), (1, 0));
        let (hints, ignored) = parse_hints(&json!({"routing_hints":[hint()]}))
            .unwrap()
            .for_database(Ulid::from(99u128));
        assert_eq!((hints.len(), ignored), (0, 1));
        for invalid in [
            Value::Null,
            json!("bad"),
            json!([{"sign":"neutral"}]),
            json!(vec![hint(); MAX_ROUTING_HINTS + 1]),
        ] {
            let (hints, ignored) = parse_hints(&json!({"routing_hints":invalid}))
                .unwrap()
                .for_database(db);
            assert!(hints.is_empty());
            assert!(ignored > 0);
        }
        let (hints, ignored) = parse_hints(&json!({"routing_hints":[]}))
            .unwrap()
            .for_database(db);
        assert_eq!((hints.len(), ignored), (0, 0));
        assert_eq!(hints_schema()["maxItems"], MAX_ROUTING_HINTS);
        let distinct: Vec<_> = (3..12u128)
            .map(|target| {
                let mut value = hint();
                value["route"]["target"] = json!(Ulid::from(target));
                value["route"]["to"] = json!(Ulid::from(target));
                value
            })
            .collect();
        let (hints, ignored) = parse_hints(&json!({"routing_hints":distinct}))
            .unwrap()
            .for_database(db);
        assert_eq!((hints.len(), ignored), (9, 0));
    }

    #[test]
    fn routing_whole_array_bytes_precede_per_row_parsing_without_a_signed_prefix() {
        let db = Ulid::from(1u128);
        let mut oversized = hint();
        oversized["route"]["edge_fingerprint"] =
            json!("\u{0001}".repeat(MAX_ROUTING_HINT_BYTES / 6));
        let request = json!({"routing_hints":[hint(), oversized]});
        assert!(
            serde_json::to_vec(&request["routing_hints"]).unwrap().len() > MAX_ROUTING_HINT_BYTES
        );
        let (hints, ignored) = parse_hints(&request).unwrap().for_database(db);
        assert_eq!((hints.len(), ignored), (0, 2));
        let (hints, ignored) =
            parse_hints(&json!({"routing_hints":vec![hint();MAX_ROUTING_HINTS]}))
                .unwrap()
                .for_database(db);
        assert_eq!((hints.len(), ignored), (MAX_ROUTING_HINTS, 0));
        let mut malformed = hint();
        malformed["route"]["edge_fingerprint"] = json!("A".repeat(64));
        let (hints, ignored) = parse_hints(&json!({"routing_hints":[hint(),malformed]}))
            .unwrap()
            .for_database(db);
        assert_eq!((hints.len(), ignored), (0, 2));
        let mut opposing = hint();
        opposing["sign"] = json!("weaken");
        opposing["route"]["edge_fingerprint"] = json!("A".repeat(64));
        for bad in [opposing, json!({"unrelated_malformed_row":true})] {
            let (hints, ignored) = parse_hints(&json!({"routing_hints":[hint(),bad]}))
                .unwrap()
                .for_database(db);
            assert_eq!((hints.len(), ignored), (0, 2));
        }
    }

    async fn routing_fixture_window(summary_repeats: usize) -> ContextWindow {
        use mneme_core::{
            BodyRef, Edge, EdgeKind, Node, NodeId, NodeStatus, Provenance,
            ports::{Budget, GraphStore, SystemClock, VectorIndex},
        };
        use mneme_cozo::MemStore;
        use mneme_engine::{Config, Memory};
        use std::sync::Arc;
        let store = Arc::new(MemStore::new(4));
        let make = |id, summary: String| {
            Node::try_new(
                NodeId(Ulid::from(id)),
                summary,
                BodyRef::new("inline://fixture").unwrap(),
                std::iter::empty::<&str>(),
                Provenance::derived_empty(),
                1.0,
                1.0,
                NodeStatus::Active,
                1,
            )
            .unwrap()
        };
        let root = make(930_000u128, "root".into());
        store.put_node(&root).await.unwrap();
        store
            .upsert(root.id(), &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        for index in 0..8u128 {
            let child = make(
                930_001 + index,
                format!(
                    "child {index}: {}",
                    "escaped \"mémöry\" ".repeat(summary_repeats)
                ),
            );
            let grandchild = make(
                930_101 + index,
                format!(
                    "grandchild {index}: {}",
                    "escaped \"mémöry\" ".repeat(summary_repeats)
                ),
            );
            for node in [&child, &grandchild] {
                store.put_node(node).await.unwrap();
                store
                    .upsert(node.id(), &[0.0, 1.0, 0.0, 0.0])
                    .await
                    .unwrap();
            }
            for (from, to) in [(root.id(), child.id()), (child.id(), grandchild.id())] {
                store
                    .put_edge(&Edge::new(from, to, 0.8, EdgeKind::Transition, 1))
                    .await
                    .unwrap();
            }
        }
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store,
            Arc::new(RoutingWindowEmbedder),
            Arc::new(SystemClock),
            Config {
                graph_seed_cap: 1,
                graph_slot_cap: 64,
                lexical_k: 0,
                ..Config::default()
            },
        );
        mneme_app::prepare_context_window(
            &memory,
            "query",
            1,
            Budget {
                max_nodes: 64,
                max_depth: 2,
                min_relevance: 0.0,
                relevance_ratio: 0.0,
                query_conditioning: 0.0,
                dedup_similarity: 1.0,
                explore: 0.0,
                ..Budget::default()
            },
            &[],
            true,
            Some(&[]),
        )
        .await
        .unwrap()
    }

    async fn conditional_fixture(
        count: usize,
        summary_repeats: usize,
    ) -> (mneme_engine::Memory, Vec<RoutingHint>, mneme_core::NodeId) {
        use mneme_core::{
            BodyRef, Edge, EdgeKind, Node, NodeId, NodeStatus, Provenance,
            ports::{GraphStore, SystemClock, VectorIndex},
        };
        use mneme_cozo::MemStore;
        use mneme_engine::{Config, Memory};
        use std::sync::Arc;
        let store = Arc::new(MemStore::new(4));
        let make = |id, summary: String| {
            Node::try_new(
                NodeId(Ulid::from(id)),
                summary,
                BodyRef::new("inline://conditional-fixture").unwrap(),
                std::iter::empty::<&str>(),
                Provenance::derived_empty(),
                1.0,
                1.0,
                NodeStatus::Active,
                1,
            )
            .unwrap()
        };
        let root = make(940_000u128, "ordinary root".into());
        store.put_node(&root).await.unwrap();
        store
            .upsert(root.id(), &[1.0, 0.0, 0.0, 0.0])
            .await
            .unwrap();
        let mut hints = Vec::new();
        for index in 0..count {
            let target = make(
                940_001 + index as u128,
                format!(
                    "conditional {index}: {}",
                    "escaped \"mémöry\" ".repeat(summary_repeats)
                ),
            );
            store.put_node(&target).await.unwrap();
            store
                .upsert(target.id(), &[0.0, 1.0, 0.0, 0.0])
                .await
                .unwrap();
            let edge = Edge::new(root.id(), target.id(), 0.8, EdgeKind::Transition, 1);
            store.put_edge(&edge).await.unwrap();
            hints.push(RoutingHint {
                route: RoutingBinding::new(&root, &target, &edge),
                sign: SignedRoutingBias::Boost,
            });
        }
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store,
            Arc::new(RoutingWindowEmbedder),
            Arc::new(SystemClock),
            Config {
                graph_seed_cap: 0,
                graph_slot_cap: 0,
                lexical_k: 0,
                ..Config::default()
            },
        );
        (memory, hints, root.id())
    }

    async fn conditional_window(
        memory: &mneme_engine::Memory,
        hints: &[RoutingHint],
        capacity: usize,
    ) -> ContextWindow {
        mneme_app::prepare_context_window(
            memory,
            "query",
            1,
            mneme_core::ports::Budget {
                max_nodes: capacity,
                max_depth: 2,
                min_relevance: 0.1,
                relevance_ratio: 0.5,
                query_conditioning: 1.0,
                dedup_similarity: 1.0,
                explore: 0.0,
                ..mneme_core::ports::Budget::default()
            },
            &[],
            true,
            Some(hints),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn conditional_observation_has_distinct_origin_and_observe_false_stays_unobserved() {
        let (memory, hints, root) = conditional_fixture(1, 0).await;
        let window = conditional_window(&memory, &hints, 2).await;
        let budget = crate::context_presentation_budget(2);
        let db = Ulid::from(1u128);
        let (observed, text) =
            pack_and_render(&window, &budget, true, Some((db, 0)), 32768).unwrap();
        assert_eq!(observed.plan.manifest().cards().len(), 2);
        let result: Value = serde_json::from_str(&text).unwrap();
        let rows = result["observation"]["cards"].as_array().unwrap();
        let ordinary = rows
            .iter()
            .find(|row| row["node_id"] == json!(root))
            .unwrap();
        assert!(ordinary.get("entry_kind").is_none());
        assert!(ordinary.get("conditional_binding").is_none());
        assert_eq!(
            ordinary.as_object().unwrap().len(),
            5,
            "ordinary routed wire shape stays unchanged"
        );
        let target = hints[0].route.target;
        let conditional = rows
            .iter()
            .find(|row| row["node_id"] == json!(target))
            .unwrap();
        assert_eq!(conditional["entry_kind"], "conditional");
        assert_eq!(
            conditional["conditional_binding"],
            json!({"db_id":db,"route":hints[0].route})
        );
        assert!(conditional["graph_path"].is_null());
        assert!(conditional.get("routing_binding").is_none());
        assert_eq!(result["usage"]["content_bytes"], text.len());
        let (_, unobserved) =
            pack_and_render(&window, &budget, false, Some((db, 0)), 32768).unwrap();
        let unobserved: Value = serde_json::from_str(&unobserved).unwrap();
        assert_eq!(unobserved["primary"], result["primary"]);
        assert!(
            unobserved.get("observation").is_none(),
            "explicit observe:false does not force provenance"
        );
        assert!(unobserved.get("entry_kind").is_none());
        assert!(unobserved["receipt"].is_null());
        assert!(
            hints_schema()["description"]
                .as_str()
                .unwrap()
                .contains("observe:true")
        );
    }

    #[tokio::test]
    async fn wrong_database_malformed_batch_and_weaken_cannot_produce_conditional_entries() {
        let (memory, hints, _) = conditional_fixture(1, 0).await;
        let db = Ulid::from(1u128);
        let valid = json!({"db_id":db,"sign":"boost","route":hints[0].route});
        let mut wrong_db = valid.clone();
        wrong_db["db_id"] = json!(Ulid::from(99u128));
        let mut malformed = valid.clone();
        malformed["sign"] = json!("weaken");
        malformed["route"]["edge_fingerprint"] = json!("A".repeat(64));
        let mut weaken = valid.clone();
        weaken["sign"] = json!("weaken");
        let budget = crate::context_presentation_budget(2);
        for (batch, preignored) in [
            (json!([wrong_db]), 1),
            (json!([valid, malformed]), 2),
            (json!([weaken]), 0),
        ] {
            let (parsed, ignored) = parse_hints(&json!({"routing_hints":batch}))
                .unwrap()
                .for_database(db);
            assert_eq!(ignored, preignored);
            let window = conditional_window(&memory, &parsed, 2).await;
            let (_, text) =
                pack_and_render(&window, &budget, true, Some((db, ignored)), 32768).unwrap();
            let result: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(result["primary"].as_array().unwrap().len(), 1);
            assert_eq!(result["routing"]["ignored"], ignored);
            for row in result["observation"]["cards"].as_array().unwrap() {
                assert!(row.get("entry_kind").is_none());
                assert!(row.get("conditional_binding").is_none());
                assert!(row["graph_path"].is_null());
                assert!(row["routing_binding"].is_null());
            }
        }
    }

    #[tokio::test]
    async fn conditional_binding_cost_repacks_whole_cards_without_losing_origin() {
        let (memory, hints, _) = conditional_fixture(32, 20).await;
        let window = conditional_window(&memory, &hints, 64).await;
        let budget = crate::context_presentation_budget(64);
        let db = Ulid::from(1u128);
        let original = window.pack(&budget).unwrap();
        assert_eq!(
            original
                .observations
                .iter()
                .filter(|row| row.conditional_binding.is_some())
                .count(),
            32
        );
        let overflow = render_routed(
            &original.plan,
            Some(&original.observations),
            db,
            original.routing.as_ref().unwrap(),
            16384,
        )
        .unwrap_err();
        assert!(overflow.downcast_ref::<EnvelopeOverflow>().is_some());
        let (packed, text) = pack_and_render(&window, &budget, true, Some((db, 0)), 16384).unwrap();
        assert!(text.len() <= 16384);
        let result: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(result["usage"]["content_bytes"], text.len());
        let rows = result["observation"]["cards"].as_array().unwrap();
        let cards = result["primary"].as_array().unwrap();
        assert_eq!(rows.len(), cards.len());
        assert!(rows.len() < original.observations.len());
        assert!(rows.iter().any(|row| row["entry_kind"] == "conditional"));
        assert_eq!(
            rows.len() as u64
                + result["omitted"]["primary"]["bounded_window_budget"]
                    .as_u64()
                    .unwrap(),
            33
        );
        for row in rows {
            if row["entry_kind"] == "conditional" {
                assert_eq!(row["conditional_binding"]["db_id"], json!(db));
                assert_eq!(
                    row["conditional_binding"]["route"]["target"],
                    row["node_id"]
                );
                assert!(row.get("routing_binding").is_none());
                assert!(row["graph_path"].is_null());
            } else {
                assert!(row.get("conditional_binding").is_none());
            }
            let card = cards
                .iter()
                .find(|card| card["id"] == row["node_id"])
                .unwrap();
            assert_eq!(
                row["card_sha256"],
                format!("{:x}", Sha256::digest(serde_json::to_vec(card).unwrap()))
            );
        }
        assert_eq!(packed.plan.manifest().cards().len(), rows.len());
        let again = window.pack(&budget).unwrap();
        assert_eq!(again.plan, original.plan);
        assert_eq!(
            again
                .observations
                .iter()
                .filter(|row| row.conditional_binding.is_some())
                .count(),
            32
        );
    }

    #[tokio::test]
    async fn invalid_conditional_projection_is_refused_not_relabelled_as_direct() {
        let (memory, hints, _) = conditional_fixture(1, 0).await;
        let window = conditional_window(&memory, &hints, 2).await;
        let original = window.pack(&crate::context_presentation_budget(2)).unwrap();
        let index = original
            .observations
            .iter()
            .position(|row| row.conditional_binding.is_some())
            .unwrap();
        let db = Ulid::from(1u128);
        for corruption in 0..5 {
            let mut observations = original.observations.clone();
            let card = &mut observations[index];
            match corruption {
                0 => card.lane = mneme_present::Lane::Expansion,
                1 => card.conditional_binding.as_mut().unwrap().target = hints[0].route.previous,
                2 => {
                    card.conditional_binding
                        .as_mut()
                        .unwrap()
                        .target_fingerprint = "A".repeat(64)
                }
                3 => card.routing_binding = Some(hints[0].route.clone()),
                4 => {
                    let route = &hints[0].route;
                    card.graph_path = Some(vec![mneme_core::ports::TraversalHop {
                        previous: route.previous,
                        target: route.target,
                        edge: mneme_core::Edge::new(
                            route.edge_from,
                            route.edge_to,
                            0.8,
                            mneme_core::EdgeKind::Transition,
                            1,
                        ),
                    }]);
                }
                _ => unreachable!(),
            }
            assert!(
                render_routed(
                    &original.plan,
                    Some(&observations),
                    db,
                    original.routing.as_ref().unwrap(),
                    32768
                )
                .is_err()
            );
        }
        assert!(
            render(&original.plan, &original.observations, 32768).is_err(),
            "conditional provenance cannot lose its database wrapper"
        );
    }

    #[tokio::test]
    async fn routing_bindings_beyond_eight_render_when_short_window_fits() {
        let window = routing_fixture_window(0).await;
        let budget = crate::context_presentation_budget(64);
        let original = window.pack(&budget).unwrap();
        let (packed, text) =
            pack_and_render(&window, &budget, true, Some((Ulid::from(1u128), 0)), 32768).unwrap();
        assert!(text.len() <= 32768);
        assert_eq!(
            packed.plan, original.plan,
            "short window requires no repacking"
        );
        let encoded: Value = serde_json::from_str(&text).unwrap();
        let observations = encoded["observation"]["cards"].as_array().unwrap();
        assert_eq!(observations.len(), 17);
        assert_eq!(
            observations
                .iter()
                .filter(|row| !row["routing_binding"].is_null())
                .count(),
            16,
            "ninth and later bindings must render when the complete window fits"
        );
        assert_eq!(encoded["omitted"]["primary"]["bounded_window_budget"], 0);
        assert!(encoded["receipt"].is_null());
    }

    #[tokio::test]
    async fn routing_bindings_beyond_eight_fit_or_repack_the_complete_same_window() {
        let window = routing_fixture_window(60).await;
        let budget = crate::context_presentation_budget(64);
        let original = window.pack(&budget).unwrap();
        assert_eq!(
            original
                .observations
                .iter()
                .filter(|row| row.routing_binding.is_some())
                .count(),
            16
        );
        let db = Ulid::from(1u128);
        let overflow = render_routed(
            &original.plan,
            Some(&original.observations),
            db,
            original.routing.as_ref().unwrap(),
            32768,
        )
        .unwrap_err();
        assert!(overflow.downcast_ref::<EnvelopeOverflow>().is_some());
        let (packed, text) = pack_and_render(&window, &budget, true, Some((db, 0)), 32768).unwrap();
        assert!(text.len() <= 32768);
        let encoded: Value = serde_json::from_str(&text).unwrap();
        let observations = encoded["observation"]["cards"].as_array().unwrap();
        let binding_count = observations
            .iter()
            .filter(|row| !row["routing_binding"].is_null())
            .count();
        let graph_count = observations
            .iter()
            .filter(|row| {
                row["graph_path"]
                    .as_array()
                    .is_some_and(|path| !path.is_empty())
            })
            .count();
        assert_eq!(
            binding_count, graph_count,
            "byte packing may omit whole cards, never their requested route bindings"
        );
        // Long cards plus complete sidecars can leave fewer than nine cards.
        // That is real byte pressure, not a separate evidence-count ceiling.
        assert!(
            encoded["usage"]["normal_content_limit"].as_u64().unwrap()
                < u64::from(budget.normal_content_limit())
        );
        assert_eq!(packed.plan.manifest().cards().len(), observations.len());
        assert_eq!(
            observations.len() as u64
                + encoded["omitted"]["primary"]["bounded_window_budget"]
                    .as_u64()
                    .unwrap(),
            original.plan.manifest().cards().len() as u64,
            "repacked window reports exactly its omitted whole cards"
        );
        assert!(encoded["receipt"].is_null());
        for row in observations {
            if !row["routing_binding"].is_null() {
                let route = &row["routing_binding"]["route"];
                let last = row["graph_path"].as_array().unwrap().last().unwrap();
                for key in ["previous", "target", "from", "to"] {
                    assert_eq!(route[key], last[key]);
                }
                assert_eq!(route["target"], row["node_id"]);
            }
            let card = encoded["primary"]
                .as_array()
                .unwrap()
                .iter()
                .find(|card| card["id"] == row["node_id"])
                .unwrap();
            assert_eq!(
                row["card_sha256"],
                format!("{:x}", Sha256::digest(serde_json::to_vec(card).unwrap()))
            );
        }
        let again = window.pack(&budget).unwrap();
        assert_eq!(again.plan, original.plan);
        assert_eq!(
            again
                .observations
                .iter()
                .filter(|row| row.routing_binding.is_some())
                .count(),
            16
        );
    }

    #[tokio::test]
    async fn observed_envelope_repacking_conserves_window_and_never_drops_sidecar() {
        use mneme_core::{
            Provenance,
            ports::{Budget, SystemClock},
        };
        use mneme_cozo::MemStore;
        use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
        use mneme_engine::{Config, Ingest, Memory};
        use std::sync::Arc;

        let store = Arc::new(MemStore::new(DEFAULT_DIM));
        let mem = Memory::new(
            store.clone(),
            store.clone(),
            store,
            Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
            Arc::new(SystemClock),
            Config {
                lexical_k: 0,
                graph_seed_cap: 0,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(mneme_body::InlineStore::new()));
        for n in 0..64 {
            let summary = format!("distinct lesson {n}: {}", "escaped \"mémöry\" ".repeat(32));
            mem.ingest(Ingest::new(&summary, b"", &[], Provenance::derived_empty()))
                .await
                .unwrap();
        }
        let window = mneme_app::prepare_context_window(
            &mem,
            "distinct lesson",
            64,
            Budget {
                max_nodes: 64,
                max_depth: 0,
                min_relevance: 0.0,
                ..Budget::default()
            },
            &[],
            true,
            Some(&[]),
        )
        .await
        .unwrap();
        let budget = crate::context_presentation_budget(64);
        let original = window.pack(&budget).unwrap();
        let db_id = Ulid::from(1u128);
        let overflow = render_routed(
            &original.plan,
            Some(&original.observations),
            db_id,
            original.routing.as_ref().unwrap(),
            32768,
        )
        .unwrap_err();
        assert!(overflow.downcast_ref::<EnvelopeOverflow>().is_some());
        let (packed, encoded) =
            pack_and_render(&window, &budget, true, Some((db_id, 3)), 32768).unwrap();
        assert!(encoded.len() <= 32768);
        assert!(packed.plan.manifest().cards().len() > 13);
        assert!(packed.plan.manifest().cards().len() < original.plan.manifest().cards().len());
        let result: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(result["usage"]["content_bytes"], encoded.len());
        assert!(
            result["usage"]["normal_content_limit"].as_u64().unwrap()
                < u64::from(budget.normal_content_limit())
        );
        assert_eq!(result["routing"]["ignored"], 3);
        assert_eq!(result["observation"]["schema"], 1);
        assert_eq!(result["observation"]["learning"], "disabled");
        assert!(result["receipt"].is_null());
        let cards = result["primary"].as_array().unwrap();
        let observations = result["observation"]["cards"].as_array().unwrap();
        assert_eq!(cards.len(), observations.len());
        assert_eq!(
            cards.len() as u64
                + result["omitted"]["primary"]["bounded_window_budget"]
                    .as_u64()
                    .unwrap(),
            64
        );
        assert_eq!(result["omitted"]["primary"]["further_tail_unknown"], true);
        assert!(
            cards
                .windows(2)
                .all(|pair| pair[0]["rank"].as_u64() < pair[1]["rank"].as_u64())
        );
        for observation in observations {
            assert!(observation.get("graph_path").is_some());
            assert!(observation.get("routing_binding").is_some());
            let card = cards
                .iter()
                .find(|card| card["id"] == observation["node_id"])
                .unwrap();
            let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(card).unwrap()));
            assert_eq!(observation["card_sha256"], digest);
        }
        // No destructive path/binding consumption or retrieval on any pack pass.
        assert_eq!(window.pack(&budget).unwrap().plan, original.plan);
        let (plain, unobserved) = pack_and_render(&window, &budget, false, None, 32768).unwrap();
        assert_eq!(plain.plan, original.plan);
        assert_eq!(unobserved, original.plan.rendered_content());
        assert!(
            serde_json::from_str::<Value>(&unobserved)
                .unwrap()
                .get("observation")
                .is_none()
        );
    }

    #[test]
    fn exact_observation_size_counts_escaping_and_utf8() {
        let envelope = json!({"usage":{"content_bytes":0},"receipt":null,
            "observation":{"schema":1,"learning":"disabled","cards":[]},
            "primary":[{"id":"example","summary":"mémöry \"quote\"\n"}]});
        let encoded = encode_exact(envelope, 4096).unwrap();
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["usage"]["content_bytes"], encoded.len());
        assert!(decoded["receipt"].is_null());
        assert_eq!(
            encode_exact(decoded.clone(), encoded.len()).unwrap(),
            encoded
        );
        let error = encode_exact(decoded, encoded.len() - 1).unwrap_err();
        let overflow = error.downcast_ref::<EnvelopeOverflow>().unwrap();
        assert_eq!(overflow.required_bytes, encoded.len());
        assert_eq!(overflow.max_bytes, encoded.len() - 1);
    }
}
