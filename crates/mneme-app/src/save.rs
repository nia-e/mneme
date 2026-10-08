//! Shared admission, execution and presentation for the SAVE operation.
//!
//! Hosts select scope and generate an operation ID before calling this module.
//! Preparation consults no clock, randomness, environment, store, or provider.
//! Freeze `into_json()` before sending it: retries must retain the entire authored
//! request, not merely its operation ID. Generated linking priors are not input.

use crate::{capture::PreparedCapture, episode::PreparedEpisode};
use mneme_core::NodeId;
use mneme_engine::Memory;
use serde::Deserialize;
use serde_json::{Value, json};
use std::error::Error;
use ulid::Ulid;

pub type SaveError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveKind {
    Note,
    Episode,
}

impl SaveKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Episode => "episode",
        }
    }
}

/// Submission origin, never a claim of verified supporting evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOrigin {
    ManualSubmission,
    ProvidedSource,
}

impl SaveOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManualSubmission => "manual_submission",
            Self::ProvidedSource => "provided_source",
        }
    }
}

/// The exact logical observation identity used by the native replay contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveIdentity {
    pub origin: SaveOrigin,
    pub namespace: String,
    pub key: String,
    pub reference: String,
    pub session: Option<String>,
    pub revision: Option<String>,
}

// This is only a projection of source fields after native admission. Their
// validation remains owned by the capture/episode parsers.
#[derive(Deserialize)]
struct SourceIdentity {
    namespace: String,
    key: String,
    reference: String,
    session: Option<String>,
    revision: Option<String>,
}

pub enum SaveRequest {
    Note(PreparedCapture),
    Episode(PreparedEpisode),
}

pub struct PreparedSave {
    identity: SaveIdentity,
    request: SaveRequest,
}

impl PreparedSave {
    /// Admit one note or episode append using the existing native parsers.
    ///
    /// `host_operation_id` is used only when neither `source` nor `operation_id`
    /// is supplied. It must be generated and retained by the host before any
    /// submission. Repeated manual invocations with fresh IDs are fresh writes.
    pub fn parse(raw: &Value, host_operation_id: &str) -> Result<Self, SaveError> {
        let object = raw.as_object().ok_or("save arguments must be an object")?;
        let kind = match object.get("kind") {
            None => SaveKind::Note,
            Some(Value::String(kind)) if kind == "note" => SaveKind::Note,
            Some(Value::String(kind)) if kind == "episode" => SaveKind::Episode,
            Some(_) => return Err("save kind must be note|episode, not null".into()),
        };
        if object.contains_key("source") && object.contains_key("operation_id") {
            return Err("save source and operation_id are mutually exclusive".into());
        }
        if kind == SaveKind::Episode && object.contains_key("touchstone") {
            return Err("touchstone owners must be semantic notes, not episodes".into());
        }
        // `action` is a native episode field, not part of SAVE. Never silently
        // overwrite a caller's editorial/revision intent with append.
        if object.contains_key("action") {
            return Err("unknown save field `action`; save only appends".into());
        }
        let mut native = raw.clone();
        let envelope = native.as_object_mut().expect("object checked above");
        envelope.remove("kind");
        let operation_id = envelope.remove("operation_id");
        if !envelope.contains_key("source") {
            let operation_id = match &operation_id {
                Some(Value::String(id)) => id.as_str(),
                Some(_) => return Err("save operation_id must be a string, not null".into()),
                None => host_operation_id,
            };
            envelope.insert(
                "source".into(),
                json!({
                    "namespace": "manual",
                    "key": operation_id,
                    "reference": format!("manual-submission:{operation_id}")
                }),
            );
        }
        let request = match kind {
            SaveKind::Note => SaveRequest::Note(PreparedCapture::parse(&native)?),
            SaveKind::Episode => {
                native["action"] = json!("append");
                SaveRequest::Episode(PreparedEpisode::parse(&native)?)
            }
        };
        let source: SourceIdentity = serde_json::from_value(native["source"].clone())?;
        // Frozen manual requests arrive with `source` on retry. The exact
        // encoded submission origin, not which input spelling was used, owns
        // this label. Supplied sources declaring this shape mean the same thing.
        let origin = if source.namespace == "manual"
            && source.reference == format!("manual-submission:{}", source.key)
            && source.session.is_none()
            && source.revision.is_none()
        {
            SaveOrigin::ManualSubmission
        } else {
            SaveOrigin::ProvidedSource
        };
        let identity = SaveIdentity {
            origin,
            namespace: source.namespace,
            key: source.key,
            reference: source.reference,
            session: source.session,
            revision: source.revision,
        };
        Ok(Self { identity, request })
    }

    pub fn kind(&self) -> SaveKind {
        match self.request {
            SaveRequest::Note(_) => SaveKind::Note,
            SaveRequest::Episode(_) => SaveKind::Episode,
        }
    }

    pub fn identity(&self) -> &SaveIdentity {
        &self.identity
    }

    pub fn requires_operator(&self) -> bool {
        match &self.request {
            SaveRequest::Note(request) => request.requires_operator(),
            SaveRequest::Episode(_) => false,
        }
    }

    pub fn expected_id(&self) -> Result<NodeId, SaveError> {
        match &self.request {
            SaveRequest::Note(request) => Ok(request.expected_source()?.node_id()),
            SaveRequest::Episode(request) => request
                .expected_edition_id()?
                .ok_or_else(|| "SAVE episode must be an append".into()),
        }
    }

    pub fn expected_body(&self) -> &[u8] {
        match &self.request {
            SaveRequest::Note(request) => request.expected_body(),
            SaveRequest::Episode(request) => {
                request.expected_body().expect("SAVE only admits append")
            }
        }
    }

    /// Verify the SAVE envelope before its identifiers drive native readback.
    /// Database identity and selection remain the transport's responsibility.
    pub fn verify_write_receipt_json(&self, receipt: &Value) -> Result<(), SaveError> {
        if !receipt.is_object()
            || receipt["kind"] != self.kind().as_str()
            || receipt["id"] != json!(self.expected_id()?)
            || !receipt["replayed"].is_boolean()
            || receipt["origin"] != self.identity.origin.as_str()
            || receipt.get("action").is_some()
        {
            return Err("SAVE write receipt kind/identity/replay/origin mismatch".into());
        }
        match self.identity.origin {
            SaveOrigin::ManualSubmission if receipt["operation_id"] != self.identity.key => {
                return Err("SAVE manual receipt operation ID mismatch".into());
            }
            SaveOrigin::ProvidedSource if receipt.get("operation_id").is_some() => {
                return Err(
                    "SAVE provided-source receipt must not claim a manual operation ID".into(),
                );
            }
            _ => {}
        }
        match &self.request {
            SaveRequest::Note(_) => {
                if ["episode_id", "edition_id", "revision"]
                    .iter()
                    .any(|name| receipt.get(*name).is_some())
                {
                    return Err("SAVE note receipt must not carry episode identity".into());
                }
                Ok(())
            }
            SaveRequest::Episode(request) => {
                request.verify_write_receipt_json(&native_episode_receipt(receipt))
            }
        }
    }

    /// Delegate exact source/body proof to the original native verifier. Episodes
    /// use native episode `get`; notes use native node `get` readback shapes.
    pub fn verify_readback_json(&self, receipt: &Value, node: &Value) -> Result<(), SaveError> {
        self.verify_write_receipt_json(receipt)?;
        match &self.request {
            SaveRequest::Note(request) => request.verify_readback_json(
                node,
                receipt["replayed"]
                    .as_bool()
                    .expect("receipt checked above"),
            ),
            SaveRequest::Episode(request) => {
                request.verify_readback_json(&native_episode_receipt(receipt), node)
            }
        }
    }

    /// Execute against an already admitted owner. The frontend retains its lease,
    /// applies capability checks and checkpoints reference backends, including
    /// exact replays. This method neither opens storage nor grants authority.
    pub async fn run(
        self,
        mem: &Memory,
        db_id: Ulid,
        origin_commit: Option<&str>,
    ) -> Result<Value, SaveError> {
        let kind = self.kind();
        let mut receipt = match self.request {
            SaveRequest::Note(request) => {
                let result = request.run(mem, origin_commit).await?;
                json!({"id":result.id,"replayed":result.replayed})
            }
            SaveRequest::Episode(request) => {
                let mut result = request.run(mem, db_id, origin_commit).await?;
                result
                    .as_object_mut()
                    .expect("native append returns an object")
                    .remove("action");
                result["id"] = result["edition_id"].clone();
                result
            }
        };
        receipt["kind"] = json!(kind.as_str());
        receipt["origin"] = json!(self.identity.origin.as_str());
        if self.identity.origin == SaveOrigin::ManualSubmission {
            receipt["operation_id"] = json!(self.identity.key);
        }
        Ok(receipt)
    }

    /// Canonical SAVE payload to freeze and submit again for either kind.
    ///
    /// The native serializers own authored fields/default omissions. SAVE only
    /// carries explicit kind and frozen source, without a native episode action
    /// or a competing operation ID. No fallback identity is needed on retry.
    pub fn into_json(self) -> Value {
        let (kind, mut value) = match self.request {
            SaveRequest::Note(request) => ("note", request.into_json()),
            SaveRequest::Episode(request) => ("episode", request.into_json()),
        };
        value
            .as_object_mut()
            .expect("native serializers return objects")
            .remove("action");
        value["kind"] = json!(kind);
        value
    }

    pub fn into_request(self) -> SaveRequest {
        self.request
    }
}

fn native_episode_receipt(receipt: &Value) -> Value {
    // Only envelope vocabulary changes. Root/edition/revision and source proofs
    // are still checked by the existing native append verifier.
    let mut native = receipt.clone();
    native["action"] = json!("append");
    native
}

/// Closed SAVE discovery shape. Kind branches refine root properties rather
/// than closing another object, so transports can add owner/identity selectors
/// at the root without accidentally making them illegal in a branch.
pub fn input_schema() -> Value {
    let mut properties = crate::capture::properties();
    let episodes = crate::episode::input_schema(&[crate::episode::EpisodeAction::Append]);
    let episode = &episodes["oneOf"][0]["properties"];
    properties["kind"] = json!({"type":"string","enum":["note","episode"],"default":"note"});
    properties["operation_id"] = json!({"type":"string","minLength":1,"maxLength":512,
        "description":"Stable manual submission identity (512 UTF-8 bytes max); freeze the whole request before retry. Omission creates a fresh operation; a lost generated receipt is ambiguous."});
    properties["occurred"] = episode["occurred"].clone();
    properties["thread"] = episode["thread"].clone();
    properties["occurrence_contexts"] = episode["occurrence_contexts"].clone();
    properties["source"]["properties"]["reference"]["description"] = json!(
        "Observation/submission origin reference, not verification of the claim; supporting citations may live in the body."
    );
    properties["links"]["description"] = json!(
        "Up to eight explicit atomic same-store links. Exact retry preserves these authored links; generated similarity priors belong to native policy, not this request."
    );
    properties["confidence"]["description"] =
        json!("Note retention/use value, not factual truth or evidentiary confidence.");
    json!({"type":"object","additionalProperties":false,"required":["summary"],
    "description":"Save one note (default) or immutable episode. Source and manual operation_id are mutually exclusive. Without either, this is a fresh manual submission, not automatically retry-safe after a lost response.",
    "properties":properties,
    "not":{"required":["source","operation_id"]},
    "oneOf":[
        {"properties":{"kind":{"const":"note"}},
         "not":{"anyOf":[{"required":["occurred"]},{"required":["thread"]},{"required":["occurrence_contexts"]}]}},
        {"required":["kind"],"properties":{"kind":{"const":"episode"},
            "body":episode["body"],"tags":episode["tags"],"links":episode["links"]},
         "not":{"anyOf":[{"required":["stability"]},{"required":["confidence"]},{"required":["touchstone"]}]}}
    ]})
}

/// One human rendering shared by local CLI and remote CLI receipts.
pub fn render_human(receipt: &Value) -> String {
    let verb = if receipt["replayed"] == true {
        "replayed"
    } else {
        "saved"
    };
    let kind = receipt["kind"].as_str().unwrap_or("?");
    let id = receipt["id"].as_str().unwrap_or("?");
    let scope = receipt
        .get("db")
        .and_then(Value::as_str)
        .map_or_else(|| id.to_owned(), |db| format!("{db}:{id}"));
    let mut rendered = format!("{verb} {kind} {scope}");
    if let Some(operation_id) = receipt.get("operation_id").and_then(Value::as_str) {
        rendered.push_str(&format!(" (operation {operation_id})"));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use mneme_body::InlineStore;
    use mneme_core::{
        CaptureRequestCodec, CaptureSource, Node, NodeId, Provenance,
        episode::{EpisodeTime, OccurrenceSpan},
        ports::{Error as NativeError, GraphStore, SystemClock},
    };
    use mneme_cozo::MemStore;
    use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
    use mneme_engine::{CaptureResult, Config, Memory};
    use std::sync::Arc;
    use ulid::Ulid;

    fn note_proof(save: PreparedSave) -> CaptureSource {
        let SaveRequest::Note(request) = save.into_request() else {
            panic!("expected note")
        };
        request.expected_source().unwrap()
    }

    fn sourced() -> Value {
        json!({"source":{"namespace":"host","key":"event/lesson-1",
            "reference":"host-event:event","session":"session","revision":"v1"},
            "summary":"Retain the author's claim", "body":"Its actual citation lives here.",
            "tags":["lesson"], "stability":0.6,"confidence":0.7})
    }

    #[test]
    fn provided_source_preserves_native_proof_and_ignores_fallback() {
        let raw = sourced();
        let expected = PreparedCapture::parse(&raw)
            .unwrap()
            .expected_source()
            .unwrap();
        let prepared = PreparedSave::parse(&raw, "").unwrap();
        assert_eq!(prepared.identity().origin, SaveOrigin::ProvidedSource);
        let frozen = prepared.into_json();
        assert_eq!(frozen["source"], raw["source"]);
        assert_eq!(
            note_proof(PreparedSave::parse(&frozen, "different").unwrap()),
            expected
        );
    }

    #[test]
    fn explicit_manual_operation_freezes_exact_replay() {
        let raw = json!({"summary":"Keep this", "operation_id":"operation-1"});
        let first = PreparedSave::parse(&raw, "ignored-one").unwrap();
        let identity = first.identity().clone();
        let frozen = first.into_json();
        let retry = PreparedSave::parse(&frozen, "different-fallback").unwrap();
        assert_eq!(retry.identity(), &identity);
        assert_eq!(retry.into_json(), frozen);
        assert_eq!(
            note_proof(PreparedSave::parse(&frozen, "").unwrap()),
            note_proof(PreparedSave::parse(&raw, "").unwrap())
        );
    }

    #[test]
    fn generated_manual_origin_does_not_fabricate_session_or_evidence() {
        let prepared = PreparedSave::parse(&json!({"summary":"A thought"}), "host-id").unwrap();
        let identity = prepared.identity();
        assert_eq!(identity.origin, SaveOrigin::ManualSubmission);
        assert_eq!(identity.namespace, "manual");
        assert_eq!(identity.key, "host-id");
        assert_eq!(identity.reference, "manual-submission:host-id");
        assert_eq!(
            (identity.session.as_ref(), identity.revision.as_ref()),
            (None, None)
        );
        let frozen = prepared.into_json();
        assert!(frozen["source"].get("session").is_none());
        assert!(frozen["source"].get("revision").is_none());
        assert_ne!(
            note_proof(PreparedSave::parse(&frozen, "").unwrap()).node_id(),
            note_proof(
                PreparedSave::parse(&json!({"summary":"A thought"}), "fresh-host-id").unwrap()
            )
            .node_id()
        );
    }

    #[test]
    fn changed_authored_content_keeps_node_identity_but_changes_proof() {
        let first = note_proof(
            PreparedSave::parse(&json!({"summary":"Old", "operation_id":"same"}), "").unwrap(),
        );
        let second = note_proof(
            PreparedSave::parse(&json!({"summary":"New", "operation_id":"same"}), "").unwrap(),
        );
        assert_eq!(first.node_id(), second.node_id());
        assert_ne!(first.request_digest(), second.request_digest());
    }

    fn episode_fixture() -> Value {
        json!({"kind":"episode", "summary":"Scene", "operation_id":"same",
            "occurred":{"kind":"point","at":42}, "thread":"task-1"})
    }

    #[test]
    fn canonical_save_roundtrips_both_kinds_without_native_action() {
        for kind in ["note", "episode"] {
            for explicit in [false, true] {
                let mut raw = json!({"kind":kind,"summary":"Scene"});
                if explicit {
                    raw["operation_id"] = json!("explicit-id");
                }
                let prepared = PreparedSave::parse(&raw, "first-fallback").unwrap();
                let identity = prepared.identity().clone();
                let frozen = prepared.into_json();
                assert_eq!(frozen["kind"], kind);
                assert!(frozen.get("action").is_none());
                assert!(frozen.get("operation_id").is_none());
                let retry = PreparedSave::parse(&frozen, "different-fallback").unwrap();
                assert_eq!(retry.identity(), &identity);
                assert_eq!(retry.into_json(), frozen);
            }
        }
        let episode = PreparedSave::parse(&episode_fixture(), "").unwrap();
        assert_eq!(episode.kind(), SaveKind::Episode);
        let SaveRequest::Episode(request) = episode.into_request() else {
            panic!("expected episode")
        };
        assert_eq!(request.action(), crate::episode::EpisodeAction::Append);
        assert_eq!(
            request.into_json()["occurred"],
            json!({"kind":"point","at":42})
        );
    }

    #[test]
    fn save_occurrence_contexts_belong_only_to_episodes_and_are_not_recorder_identity() {
        let contexts = json!([{"namespace":"Place","key":"The shared room","label":"Room"}]);
        for kind in [None, Some("note"), Some("episode")] {
            let mut raw = json!({"summary":"A scene", "operation_id":"manual-1", "occurrence_contexts":contexts});
            if let Some(kind) = kind {
                raw["kind"] = json!(kind);
            }
            assert_eq!(
                PreparedSave::parse(&raw, "unused").is_ok(),
                kind == Some("episode")
            );
            assert_eq!(
                schema_shape_accepts(&input_schema(), &raw),
                kind == Some("episode")
            );
        }
        let raw = json!({"kind":"episode", "summary":"A scene", "operation_id":"manual-1", "occurrence_contexts":contexts});
        let prepared = PreparedSave::parse(&raw, "unused").unwrap();
        assert_eq!(prepared.identity().namespace, "manual");
        assert_eq!(prepared.identity().key, "manual-1");
        let frozen = prepared.into_json();
        assert_eq!(frozen["occurrence_contexts"], contexts);
        assert!(frozen["source"].get("session").is_none());
        assert_eq!(
            PreparedSave::parse(&frozen, "different")
                .unwrap()
                .into_json(),
            frozen
        );
        let unknown = PreparedSave::parse(
            &json!({"kind":"episode", "summary":"Unknown place"}),
            "host",
        )
        .unwrap()
        .into_json();
        assert!(unknown.get("occurrence_contexts").is_none());
    }

    #[tokio::test]
    async fn save_contexts_are_exact_native_metadata_and_retry_intent() {
        let (memory, store) = fixture_memory(0);
        let mut raw = episode_fixture();
        raw["occurrence_contexts"] = json!([
            {"namespace":"Place", "key":"Room", "label":"Shared room"},
            {"namespace":"Chat", "key":"Scene/1"}
        ]);
        let frozen = PreparedSave::parse(&raw, "unused").unwrap().into_json();
        let proof = PreparedSave::parse(&frozen, "different").unwrap();
        let receipt = PreparedSave::parse(&frozen, "different")
            .unwrap()
            .run(&memory, Ulid::from(0_u128), None)
            .await
            .unwrap();
        let node = store.get_node(edition_id(&receipt)).await.unwrap().unwrap();
        assert_eq!(
            stored_source(&node).request_codec(),
            CaptureRequestCodec::EpisodeV2
        );
        assert_eq!(
            json!(node.episode().unwrap().occurrence_contexts().unwrap()),
            frozen["occurrence_contexts"]
        );
        let readback = PreparedEpisode::parse(
            &json!({"action":"get", "episode_id":receipt["episode_id"], "body":true}),
        )
        .unwrap()
        .run(&memory, Ulid::from(0_u128), None)
        .await
        .unwrap();
        proof.verify_readback_json(&receipt, &readback).unwrap();
        let mut reordered = frozen.clone();
        reordered["occurrence_contexts"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let replay = PreparedSave::parse(&reordered, "")
            .unwrap()
            .run(&memory, Ulid::from(0_u128), None)
            .await
            .unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["id"], receipt["id"]);
        for contexts in [None, Some(json!([{"namespace":"Chat", "key":"Scene/1"}]))] {
            let mut changed = frozen.clone();
            if let Some(contexts) = contexts {
                changed["occurrence_contexts"] = contexts;
            } else {
                changed
                    .as_object_mut()
                    .unwrap()
                    .remove("occurrence_contexts");
            }
            assert_native_conflict(
                PreparedSave::parse(&changed, "")
                    .unwrap()
                    .run(&memory, Ulid::from(0_u128), None)
                    .await
                    .unwrap_err(),
            );
        }
        let mut missing = readback.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("occurrence_contexts");
        assert!(proof.verify_readback_json(&receipt, &missing).is_err());
    }

    fn fixture_memory(prior_cap: usize) -> (Memory, Arc<MemStore>) {
        let store = Arc::new(MemStore::new(DEFAULT_DIM));
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
            Arc::new(SystemClock),
            Config {
                similarity_link_cap: prior_cap,
                min_similarity_links: 0,
                similarity_link_threshold: 0.9,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(InlineStore::new()));
        (memory, store)
    }

    fn stored_source(node: &Node) -> &CaptureSource {
        let Provenance::External { source } = node.provenance() else {
            panic!("missing source")
        };
        source
    }

    async fn save_note(memory: &Memory, raw: &Value) -> Result<CaptureResult, SaveError> {
        let SaveRequest::Note(request) = PreparedSave::parse(raw, "fallback")?.into_request()
        else {
            panic!("expected note")
        };
        request.run(memory, None).await
    }

    async fn save_episode(memory: &Memory, raw: &Value) -> Result<Value, SaveError> {
        let SaveRequest::Episode(request) = PreparedSave::parse(raw, "fallback")?.into_request()
        else {
            panic!("expected episode")
        };
        request.run(memory, Ulid::from(0_u128), None).await
    }

    fn edition_id(result: &Value) -> NodeId {
        NodeId(Ulid::from_string(result["edition_id"].as_str().unwrap()).unwrap())
    }

    fn assert_native_conflict(error: SaveError) {
        assert!(
            matches!(
                error.downcast_ref::<NativeError>(),
                Some(NativeError::Conflict(_))
            ),
            "not a native identity conflict: {error}"
        );
    }

    #[tokio::test]
    async fn episode_actual_admission_matches_native_and_binds_occurrence_and_thread() {
        let raw = episode_fixture();
        let frozen = PreparedSave::parse(&raw, "unused").unwrap().into_json();
        let (memory, store) = fixture_memory(0);
        let context = json!({"summary":"Context target","operation_id":"context"});
        let target = save_note(&memory, &context).await.unwrap().id;
        let saved = save_episode(&memory, &frozen).await.unwrap();
        assert_eq!(saved["replayed"], false);
        let node = store.get_node(edition_id(&saved)).await.unwrap().unwrap();
        let facet = node.episode().unwrap();
        assert_eq!(
            facet.occurrence(),
            &OccurrenceSpan::Point {
                at: EpisodeTime::new(42).unwrap()
            }
        );
        assert_eq!(facet.thread().unwrap().as_str(), "task-1");
        assert_eq!(
            stored_source(&node).request_codec(),
            CaptureRequestCodec::EpisodeV1
        );

        // Run the actual native parser on an independent disposable store.
        let SaveRequest::Episode(native_request) =
            PreparedSave::parse(&frozen, "").unwrap().into_request()
        else {
            panic!("expected episode")
        };
        let (direct_memory, direct_store) = fixture_memory(0);
        let direct = PreparedEpisode::parse(&native_request.into_json())
            .unwrap()
            .run(&direct_memory, Ulid::from(0_u128), None)
            .await
            .unwrap();
        let direct_node = direct_store
            .get_node(edition_id(&direct))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_source(&direct_node), stored_source(&node));
        assert_eq!(direct["edition_id"], saved["edition_id"]);

        let replay = save_episode(&memory, &frozen).await.unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["edition_id"], saved["edition_id"]);
        for (field, value) in [
            ("occurred", json!({"kind":"point","at":43})),
            ("thread", json!("task-2")),
            ("body", json!("A changed scene body")),
            (
                "links",
                json!([{"to":target.0.to_string(),"kind":"associative","weight":0.6}]),
            ),
        ] {
            let mut changed = frozen.clone();
            changed[field] = value;
            assert_native_conflict(save_episode(&memory, &changed).await.unwrap_err());
            let (changed_memory, changed_store) = fixture_memory(0);
            assert_eq!(
                save_note(&changed_memory, &context).await.unwrap().id,
                target
            );
            let changed_result = save_episode(&changed_memory, &changed).await.unwrap();
            let changed_node = changed_store
                .get_node(edition_id(&changed_result))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(changed_node.id(), node.id());
            assert_ne!(
                stored_source(&changed_node).request_digest(),
                stored_source(&node).request_digest()
            );
        }
    }

    #[tokio::test]
    async fn save_note_native_priors_commit_once_and_replay_never_relinks() {
        let (memory, store) = fixture_memory(2);
        let target = save_note(
            &memory,
            &json!({"summary":"Identical neighbour", "operation_id":"target"}),
        )
        .await
        .unwrap();
        let frozen = PreparedSave::parse(
            &json!({"summary":"Identical neighbour", "operation_id":"saved"}),
            "",
        )
        .unwrap()
        .into_json();
        let saved = save_note(&memory, &frozen).await.unwrap();
        assert!(!saved.replayed);
        assert!(store.get_edge(saved.id, target.id).await.unwrap().is_some());
        store.delete_edge(saved.id, target.id).await.unwrap();
        save_note(
            &memory,
            &json!({"summary":"Identical neighbour", "operation_id":"new-corpus"}),
        )
        .await
        .unwrap();
        // A new insertion may create an incoming prior. Delete that too so the
        // replay assertion distinguishes new admission from replay rebuilding.
        for edge in store.neighbors(saved.id, 8).await.unwrap() {
            store
                .delete_edge(edge.edge.from, edge.edge.to)
                .await
                .unwrap();
        }
        let replay = save_note(&memory, &frozen).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.id, saved.id);
        assert!(store.neighbors(saved.id, 8).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn changing_kind_under_one_operation_id_is_a_native_conflict_both_ways() {
        for first_kind in ["note", "episode"] {
            let (memory, store) = fixture_memory(0);
            let mut raw = json!({"summary":"Scene", "operation_id":"same", "kind":first_kind});
            let id = if first_kind == "note" {
                save_note(&memory, &raw).await.unwrap().id
            } else {
                edition_id(&save_episode(&memory, &raw).await.unwrap())
            };
            raw["kind"] = json!(if first_kind == "note" {
                "episode"
            } else {
                "note"
            });
            let error = if first_kind == "note" {
                save_episode(&memory, &raw).await.unwrap_err()
            } else {
                save_note(&memory, &raw).await.unwrap_err()
            };
            assert_native_conflict(error);
            let retained = store.get_node(id).await.unwrap().unwrap();
            assert_eq!(retained.episode().is_some(), first_kind == "episode");
        }
    }

    #[test]
    fn save_delegates_note_touchstones_but_rejects_episode_owners() {
        let mut raw = json!({"summary":"A turning point","operation_id":"touchstone-1",
        "touchstone":{"subject":"The scene matters","references":[{
            "db_id":Ulid::from(1_u128).to_string(),"id":Ulid::from(2_u128).to_string(),
            "expected_snapshot_sha256":"a".repeat(64)
        }]}});
        let prepared = PreparedSave::parse(&raw, "unused").unwrap();
        let id = prepared.expected_id().unwrap();
        let frozen = prepared.into_json();
        assert_eq!(frozen["touchstone"], raw["touchstone"]);
        assert_eq!(
            PreparedSave::parse(&frozen, "different")
                .unwrap()
                .expected_id()
                .unwrap(),
            id
        );
        assert!(schema_shape_accepts(&input_schema(), &raw));
        raw["kind"] = json!("episode");
        assert!(PreparedSave::parse(&raw, "unused").is_err());
        assert!(!schema_shape_accepts(&input_schema(), &raw));
    }

    #[test]
    fn rejects_save_envelope_wrong_types_nulls_conflicts_and_editorial_intent() {
        for raw in [
            json!(null),
            json!([]),
            json!({"summary":"x","kind":null}),
            json!({"summary":"x","kind":"experience"}),
            json!({"summary":"x","kind":1}),
            json!({"summary":"x","operation_id":null}),
            json!({"summary":"x","operation_id":1}),
            json!({"summary":"x","source":{},"operation_id":"id"}),
            json!({"summary":"x","kind":"episode","action":"revise"}),
        ] {
            assert!(
                PreparedSave::parse(&raw, "fallback").is_err(),
                "accepted {raw}"
            );
        }
    }

    #[test]
    fn delegates_all_native_fields_bounds_and_nullable_validation() {
        for kind in ["note", "episode"] {
            for (field, value) in [
                ("summary", json!(null)),
                ("summary", json!(7)),
                ("summary", json!(" ")),
                ("summary", json!("x".repeat(2049))),
                ("body", json!(null)),
                ("body", json!(7)),
                ("tags", json!(null)),
                ("tags", json!([null])),
                ("tags", json!(["x".repeat(2049)])),
                ("tags", json!(vec!["x"; 33])),
                ("source", json!(null)),
                ("links", json!(null)),
                ("links", json!({})),
                ("unknown", json!(true)),
                ("generated_priors", json!([])),
            ] {
                let mut raw = json!({"kind":kind,"summary":"x"});
                raw[field] = value;
                assert!(PreparedSave::parse(&raw, "id").is_err(), "accepted {raw}");
            }
        }
        for raw in [
            json!({"summary":"x","occurred":{"kind":"unknown"}}),
            json!({"summary":"x","thread":"task"}),
            json!({"summary":"x","confidence":null}),
            json!({"summary":"x","stability":2}),
            json!({"kind":"episode","summary":"x","stability":0.5}),
            json!({"kind":"episode","summary":"x","confidence":0.5}),
            json!({"kind":"episode","summary":"x","occurred":null}),
            json!({"kind":"episode","summary":"x","thread":null}),
            json!({"kind":"episode","summary":"x","body":"x".repeat(16385)}),
            json!({"summary":"x","body":"x".repeat(262145)}),
        ] {
            assert!(PreparedSave::parse(&raw, "id").is_err());
        }
    }

    #[test]
    fn operation_ids_use_existing_source_bounds_only_when_used() {
        for id in ["", " untrimmed", "control\n", &"x".repeat(513)] {
            assert!(PreparedSave::parse(&json!({"summary":"x"}), id).is_err());
            assert!(
                PreparedSave::parse(&json!({"summary":"x","operation_id":id}), "fallback").is_err()
            );
        }
        assert!(PreparedSave::parse(&sourced(), "\nignored").is_ok());
        assert!(
            PreparedSave::parse(&json!({"summary":"x","operation_id":"valid"}), "\nignored")
                .is_ok()
        );
    }

    #[test]
    fn rejects_nested_unknown_and_nullable_source_or_link_fields() {
        for kind in ["note", "episode"] {
            for patch in [
                json!({"session":null}),
                json!({"revision":null}),
                json!({"unknown":1}),
            ] {
                let mut raw = sourced();
                raw.as_object_mut().unwrap().remove("stability");
                raw.as_object_mut().unwrap().remove("confidence");
                raw["kind"] = json!(kind);
                raw["source"]
                    .as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                assert!(PreparedSave::parse(&raw, "id").is_err());
            }
            for patch in [
                json!({"weight":null}),
                json!({"kind":null}),
                json!({"unknown":1}),
            ] {
                let mut link = json!({"to":"01ARZ3NDEKTSV4RRFFQ69G5FAV"});
                link.as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                let raw = json!({"kind":kind,"summary":"x","links":[link]});
                assert!(PreparedSave::parse(&raw, "id").is_err());
            }
        }
    }

    #[tokio::test]
    async fn shared_run_receipts_and_native_readback_proofs_cover_both_kinds() {
        for kind in ["note", "episode"] {
            let raw = if kind == "episode" {
                episode_fixture()
            } else {
                json!({"summary":"A useful note","operation_id":"same"})
            };
            let prepared = PreparedSave::parse(&raw, "unused").unwrap();
            let id = prepared.expected_id().unwrap();
            let body = prepared.expected_body().to_vec();
            let frozen = prepared.into_json();
            let proof = PreparedSave::parse(&frozen, "different").unwrap();
            let (memory, store) = fixture_memory(0);
            let mut receipt = PreparedSave::parse(&frozen, "different")
                .unwrap()
                .run(&memory, Ulid::from(0_u128), None)
                .await
                .unwrap();
            assert_eq!(receipt["kind"], kind);
            assert_eq!(receipt["id"], json!(id));
            assert_eq!(receipt["origin"], "manual_submission");
            assert_eq!(receipt["operation_id"], "same");
            assert_eq!(receipt["replayed"], false);
            assert!(receipt.get("action").is_none());
            assert!(receipt.get("body").is_none());
            assert!(receipt.get("db").is_none());
            proof.verify_write_receipt_json(&receipt).unwrap();
            // Transport-owned scope is additive, never minted by app execution.
            receipt["db"] = json!("project");
            receipt["db_id"] = json!(Ulid::from(0_u128).to_string());
            assert_eq!(
                render_human(&receipt),
                format!("saved {kind} project:{} (operation same)", id.0)
            );

            let readback = if kind == "episode" {
                PreparedEpisode::parse(&json!({"action":"get","episode_id":receipt["episode_id"],
                    "edition_id":receipt["edition_id"],"body":true}))
                .unwrap()
                .run(&memory, Ulid::from(0_u128), None)
                .await
                .unwrap()
            } else {
                let node = store.get_node(id).await.unwrap().unwrap();
                let source = stored_source(&node);
                let digest = source
                    .request_digest()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                json!({"id":id,"summary":node.summary(),"summary_truncated":false,
                    "provenance":{"type":"external","source":{
                        "namespace":source.namespace(),"key":source.key(),"reference":source.reference(),
                        "session":source.session(),"revision":source.revision(),
                        "request_digest_sha256":digest,"request_codec":source.request_codec()}},
                    "body":std::str::from_utf8(&body).unwrap(),
                    "body_range":{"source_start":0,"source_end":body.len(),"next_offset":null,"has_more":false}})
            };
            proof.verify_readback_json(&receipt, &readback).unwrap();
            if kind == "episode" {
                assert_eq!(readback["summary_snapshot"]["id"], readback["edition_id"]);
                assert_eq!(readback["summary_snapshot"]["coverage"], "summary_only");
                assert_eq!(
                    readback["summary_snapshot"]["expected_snapshot_sha256"]
                        .as_str()
                        .unwrap()
                        .len(),
                    64
                );
            }
            for (field, value) in [
                ("kind", json!("wrong")),
                ("id", json!("wrong")),
                ("replayed", json!(null)),
                ("origin", json!("verified_evidence")),
                ("operation_id", json!("different")),
                ("action", json!("append")),
            ] {
                let mut bad = receipt.clone();
                bad[field] = value;
                assert!(
                    proof.verify_write_receipt_json(&bad).is_err(),
                    "accepted {bad}"
                );
            }
            let mut bad_body = readback.clone();
            bad_body["body"] = json!("wrong body");
            assert!(proof.verify_readback_json(&receipt, &bad_body).is_err());
            let mut bad_digest = readback.clone();
            if kind == "episode" {
                bad_digest["source"]["request_digest_sha256"] = json!("0".repeat(64));
            } else {
                bad_digest["provenance"]["source"]["request_digest_sha256"] = json!("0".repeat(64));
            }
            assert!(proof.verify_readback_json(&receipt, &bad_digest).is_err());

            let replay = PreparedSave::parse(&frozen, "changed-fallback")
                .unwrap()
                .run(&memory, Ulid::from(0_u128), None)
                .await
                .unwrap();
            proof.verify_write_receipt_json(&replay).unwrap();
            proof.verify_readback_json(&replay, &readback).unwrap();
            assert_eq!(replay["replayed"], true);
            assert!(render_human(&replay).starts_with(&format!("replayed {kind} ")));
        }
    }

    #[tokio::test]
    async fn provided_source_receipt_does_not_claim_manual_origin_or_operator_authority() {
        let raw = sourced();
        let proof = PreparedSave::parse(&raw, "").unwrap();
        assert!(!proof.requires_operator());
        let (memory, _) = fixture_memory(0);
        let receipt = PreparedSave::parse(&raw, "")
            .unwrap()
            .run(&memory, Ulid::from(0_u128), None)
            .await
            .unwrap();
        assert_eq!(receipt["origin"], "provided_source");
        assert!(receipt.get("operation_id").is_none());
        proof.verify_write_receipt_json(&receipt).unwrap();
        let mut bad = receipt;
        bad["operation_id"] = json!("fake-manual-origin");
        assert!(proof.verify_write_receipt_json(&bad).is_err());
        let mut core = raw;
        core["tags"] = json!(["core"]);
        assert!(PreparedSave::parse(&core, "").unwrap().requires_operator());
        assert!(
            !PreparedSave::parse(&episode_fixture(), "")
                .unwrap()
                .requires_operator()
        );
        core["kind"] = json!("episode");
        core.as_object_mut().unwrap().remove("confidence");
        core.as_object_mut().unwrap().remove("stability");
        assert!(PreparedSave::parse(&core, "").is_err());
    }

    // Test-only JSON Schema subset evaluator: schema describes shape/bounds,
    // while native parsers own semantic checks (UTF-8 bytes, canonical IDs etc).
    fn schema_shape_accepts(schema: &Value, value: &Value) -> bool {
        if let Some(expected) = schema.get("const")
            && value != expected
        {
            return false;
        }
        if let Some(choices) = schema.get("enum").and_then(Value::as_array)
            && !choices.contains(value)
        {
            return false;
        }
        if let Some(variants) = schema.get("oneOf").and_then(Value::as_array)
            && variants
                .iter()
                .filter(|branch| schema_shape_accepts(branch, value))
                .count()
                != 1
        {
            return false;
        }
        if let Some(variants) = schema.get("anyOf").and_then(Value::as_array)
            && !variants
                .iter()
                .any(|branch| schema_shape_accepts(branch, value))
        {
            return false;
        }
        if let Some(not) = schema.get("not")
            && schema_shape_accepts(not, value)
        {
            return false;
        }
        if let Some(kind) = schema.get("type").and_then(Value::as_str) {
            let valid = match kind {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "number" => value.is_number(),
                "integer" => value.as_u64().is_some(),
                _ => false,
            };
            if !valid {
                return false;
            }
        }
        if let Some(object) = value.as_object() {
            if let Some(required) = schema.get("required").and_then(Value::as_array)
                && required
                    .iter()
                    .any(|name| !object.contains_key(name.as_str().unwrap()))
            {
                return false;
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema["additionalProperties"] == false
                && object
                    .keys()
                    .any(|name| properties.is_none_or(|fields| !fields.contains_key(name)))
            {
                return false;
            }
            if let Some(properties) = properties
                && properties.iter().any(|(name, field)| {
                    object
                        .get(name)
                        .is_some_and(|value| !schema_shape_accepts(field, value))
                })
            {
                return false;
            }
        }
        if let Some(items) = value.as_array() {
            if let Some(limit) = schema.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < limit
            {
                return false;
            }
            if let Some(limit) = schema.get("maxItems").and_then(Value::as_u64)
                && items.len() as u64 > limit
            {
                return false;
            }
            if let Some(item_schema) = schema.get("items")
                && items
                    .iter()
                    .any(|item| !schema_shape_accepts(item_schema, item))
            {
                return false;
            }
        }
        if let Some(text) = value.as_str() {
            let length = text.chars().count() as u64;
            if schema
                .get("minLength")
                .and_then(Value::as_u64)
                .is_some_and(|min| length < min)
                || schema
                    .get("maxLength")
                    .and_then(Value::as_u64)
                    .is_some_and(|max| length > max)
            {
                return false;
            }
        }
        if let Some(number) = value.as_f64()
            && (schema
                .get("minimum")
                .and_then(Value::as_f64)
                .is_some_and(|min| number < min)
                || schema
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .is_some_and(|max| number > max))
        {
            return false;
        }
        true
    }

    #[test]
    fn schema_closed_union_matches_kind_identity_and_transport_composition() {
        let schema = input_schema();
        let valid = [
            json!({"summary":"Fresh"}),
            json!({"summary":"Manual","operation_id":"id"}),
            sourced(),
            episode_fixture(),
            json!({"kind":"episode","summary":"Scene","body":""}),
        ];
        for raw in valid {
            assert!(schema_shape_accepts(&schema, &raw), "schema rejected {raw}");
            assert!(PreparedSave::parse(&raw, "fallback").is_ok());
        }
        for raw in [
            json!({"summary":"x","kind":null}),
            json!({"summary":"x","operation_id":null}),
            json!({"summary":"x","source":null}),
            json!({"summary":"x","unknown":1}),
            json!({"summary":"x","action":"append"}),
            json!({"summary":"x","thread":"task"}),
            json!({"summary":"x","occurred":{"kind":"unknown"}}),
            json!({"summary":"x","operation_id":"id","source":sourced()["source"]}),
            json!({"kind":"episode","summary":"x","confidence":0.5}),
            json!({"kind":"episode","summary":"x","stability":0.5}),
            json!({"kind":"episode","summary":"x","tags":["core"]}),
            json!({"kind":"episode","summary":"x","body":"x".repeat(16385)}),
        ] {
            assert!(
                !schema_shape_accepts(&schema, &raw),
                "schema accepted {raw}"
            );
            assert!(PreparedSave::parse(&raw, "fallback").is_err());
        }
        let mut transport = schema;
        transport["properties"]["db"] = json!({"type":"string"});
        transport["properties"]["expected_db_id"] = json!({"type":"string"});
        transport["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("db"));
        for kind in ["note", "episode"] {
            let raw = json!({"kind":kind,"summary":"x","db":"project","expected_db_id":"guard"});
            assert!(schema_shape_accepts(&transport, &raw));
            assert!(!schema_shape_accepts(&input_schema(), &raw));
        }
    }
}
