//! Shared strict touchstone authoring and indexed browse/read projections.
//!
//! Historical summary snapshots are evidence of the copied projection only:
//! neither their hashes nor current resolution attest to body equality.

use mneme_core::{Node, NodeId};
use mneme_engine::Memory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::error::Error;
use ulid::Ulid;

pub type TouchstoneError = Box<dyn Error + Send + Sync>;
use mneme_core::touchstone::{MAX_TOUCHSTONE_CURSOR_BYTES, MAX_TOUCHSTONE_RECORD_BYTES};
pub use mneme_core::touchstone::{
    MAX_TOUCHSTONE_INPUT_BYTES, MAX_TOUCHSTONE_PAGE_ITEMS as MAX_TOUCHSTONE_LIST_LIMIT,
    MAX_TOUCHSTONE_SUBJECT_BYTES,
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthoredTouchstone {
    subject: String,
    references: Vec<AuthoredReference>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthoredReference {
    db_id: String,
    id: String,
    expected_snapshot_sha256: String,
}

fn canonical_ulid(value: &str, name: &str) -> Result<Ulid, TouchstoneError> {
    let id = Ulid::from_string(value).map_err(|_| format!("invalid touchstone {name}"))?;
    if id.to_string() != value {
        return Err(format!("touchstone {name} must be a canonical ULID").into());
    }
    Ok(id)
}

impl AuthoredTouchstone {
    pub(crate) fn typed(&self) -> Result<mneme_core::TouchstoneInput, TouchstoneError> {
        let references = self
            .references
            .iter()
            .map(|reference| {
                Ok(mneme_core::TouchstoneReference::new(
                    canonical_ulid(&reference.db_id, "reference db_id")?,
                    NodeId(canonical_ulid(&reference.id, "reference id")?),
                    mneme_core::SummarySnapshotDigest::from_hex(
                        &reference.expected_snapshot_sha256,
                    )?,
                ))
            })
            .collect::<Result<Vec<_>, TouchstoneError>>()?;
        Ok(mneme_core::TouchstoneInput::new(
            mneme_core::TouchstoneSubject::new(&self.subject)?,
            references,
        )?)
    }

    pub(crate) fn verify_record_json(
        &self,
        raw: &Value,
        owner: NodeId,
    ) -> Result<(), TouchstoneError> {
        let record: mneme_core::TouchstoneRecord = serde_json::from_value(raw.clone())?;
        let input = self.typed()?;
        if record.owner() != owner
            || record.subject() != input.subject()
            || record.references().len() != input.references().len()
        {
            return Err(
                "capture readback touchstone owner/subject/reference count mismatch".into(),
            );
        }
        let mut actual = record
            .references()
            .iter()
            .map(|snapshot| (snapshot.db_id(), snapshot.id(), snapshot.digest().to_hex()))
            .collect::<Vec<_>>();
        let mut expected = self
            .references
            .iter()
            .map(|reference| {
                Ok((
                    canonical_ulid(&reference.db_id, "reference db_id")?,
                    NodeId(canonical_ulid(&reference.id, "reference id")?),
                    reference.expected_snapshot_sha256.clone(),
                ))
            })
            .collect::<Result<Vec<_>, TouchstoneError>>()?;
        actual.sort();
        expected.sort();
        if actual != expected {
            return Err("capture readback historical touchstone references mismatch".into());
        }
        Ok(())
    }
}

/// Closed authoring shape reused by capture and SAVE discovery schemas.
pub fn authoring_schema() -> Value {
    json!({
        "type":"object", "additionalProperties":false, "required":["subject","references"],
        "description":"Agent-authored immutable summary-only references; 8 KiB total input. Obtain each expected snapshot hash from native GET in this same database. Not supported for episode owners.",
        "properties":{
            "subject":{"type":"string","minLength":1,"maxLength":MAX_TOUCHSTONE_SUBJECT_BYTES},
            "references":{"type":"array","minItems":1,
                "items":{"type":"object","additionalProperties":false,
                    "required":["db_id","id","expected_snapshot_sha256"],
                    "properties":{
                        "db_id":{"type":"string","minLength":26,"maxLength":26,"description":"Same local logical database ULID"},
                        "id":{"type":"string","minLength":26,"maxLength":26,"description":"Exact node or episode-edition ULID, never current-head substitution"},
                        "expected_snapshot_sha256":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$","description":"GET summary_snapshot.expected_snapshot_sha256; binds summary/provenance/creation/kind, not body"}
                    }}}
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedTouchstoneList {
    kind: String,
    after: Option<String>,
    limit: Option<usize>,
}

impl PreparedTouchstoneList {
    pub fn parse(raw: &Value) -> Result<Self, TouchstoneError> {
        let object = raw
            .as_object()
            .ok_or("touchstone list arguments must be an object")?;
        if serde_json::to_vec(raw)?.len() > MAX_TOUCHSTONE_INPUT_BYTES {
            return Err("touchstone list input exceeds 8192 UTF-8 bytes".into());
        }
        for name in ["after", "limit"] {
            if object.get(name).is_some_and(Value::is_null) {
                return Err(format!("touchstone list {name} must not be null").into());
            }
        }
        let input: Self = serde_json::from_value(raw.clone())?;
        if input.kind != "touchstones" {
            return Err("list kind must be touchstones".into());
        }
        if let Some(limit) = input.limit
            && !(1..=MAX_TOUCHSTONE_LIST_LIMIT).contains(&limit)
        {
            return Err(
                format!("touchstone list limit must be 1..={MAX_TOUCHSTONE_LIST_LIMIT}").into(),
            );
        }
        if let Some(after) = &input.after
            && (after.is_empty()
                || after.len() > MAX_TOUCHSTONE_CURSOR_BYTES
                || after.chars().any(char::is_control))
        {
            return Err("touchstone list after must be a nonempty bounded native cursor".into());
        }
        input.request()?;
        Ok(input)
    }

    fn request(&self) -> Result<mneme_core::TouchstonePageRequest, TouchstoneError> {
        let cursor = self.after.as_deref().map(str::parse).transpose()?;
        Ok(mneme_core::TouchstonePageRequest::new(
            None,
            cursor,
            self.limit.unwrap_or(MAX_TOUCHSTONE_LIST_LIMIT),
        )?)
    }

    /// `has_more` means a continuation is available, not that another row is
    /// known to exist. Bounded native scans may require an empty final page.
    pub async fn run(self, mem: &Memory, db_id: Ulid) -> Result<Value, TouchstoneError> {
        let request = self.request()?;
        request.validate(db_id)?;
        let page = mem.touchstones_page(&request).await?;
        let items = page
            .items
            .iter()
            .map(|header| {
                json!({
                    "id":header.id,"summary":header.summary,"subject":header.subject,
                    "reference_count":header.reference_count,"status":header.status
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"kind":"touchstones","db_id":db_id.to_string(),
            "items":items,"next_cursor":page.next,"has_more":page.next.is_some()}))
    }

    /// Freeze the complete list selection for forwarding to an admitted owner.
    pub fn into_json(self) -> Value {
        let mut value = json!({"kind":self.kind});
        if let Some(after) = self.after {
            value["after"] = json!(after);
        }
        if let Some(limit) = self.limit {
            value["limit"] = json!(limit);
        }
        value
    }
}

pub fn list_input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["kind"],
    "description":"Indexed, bounded keyset browse of native touchstone owners, not semantic top-k search. Follow next_cursor until null; has_more means continuation available, not guaranteed additional items, so the final page may be empty. Cursors retain logical database identity.",
    "properties":{
        "kind":{"type":"string","const":"touchstones"},
        "after":{"type":"string","minLength":1,"maxLength":MAX_TOUCHSTONE_CURSOR_BYTES,"description":"Opaque next_cursor from the previous page in this database"},
        "limit":{"type":"integer","minimum":1,"maximum":MAX_TOUCHSTONE_LIST_LIMIT,"default":MAX_TOUCHSTONE_LIST_LIMIT}
    }})
}

/// Fields to merge into native node GET, without changing the host's existing
/// body/range/provenance projection. Nonowners omit both owner metadata fields.
/// The supplied node is the exact edition already fetched by the host.
pub async fn node_touchstone_projection(
    mem: &Memory,
    db_id: Ulid,
    node: &Node,
) -> Result<Value, TouchstoneError> {
    let snapshot = mneme_core::SummarySnapshot::from_node(db_id, node)?;
    let mut value = json!({"summary_snapshot":{
        "db_id":db_id.to_string(),"id":node.id(),
        "expected_snapshot_sha256":snapshot.digest().to_hex(),"coverage":"summary_only"
    }});
    if let Some(record) = mem.get_touchstone(node.id()).await? {
        record.validate_owner(db_id, node)?;
        let mut current = Vec::with_capacity(record.references().len());
        for historical in record.references() {
            let (status, digest) = match mem.summary_snapshot(historical.id()).await {
                Ok(Some(snapshot)) => {
                    let digest = snapshot.digest();
                    (
                        if digest == historical.digest() {
                            "unchanged"
                        } else {
                            "snapshot_changed"
                        },
                        Some(digest.to_hex()),
                    )
                }
                Ok(None) => ("absent", None),
                Err(_) => ("unavailable", None),
            };
            let mut item = json!({"db_id":historical.db_id().to_string(),"id":historical.id(),"status":status});
            if let Some(digest) = digest {
                item["current_snapshot_sha256"] = json!(digest);
            }
            current.push(item);
        }
        value["touchstone"] = serde_json::to_value(record)?;
        value["touchstone_current"] = json!({"coverage":"summary_only","references":current});
    }
    // Full historical records are individually bounded at 64 KiB. Reserve room
    // for the current-resolution caveats and the main-node binding; never permit
    // a full historical record to silently truncate under a body-page budget.
    const MAX_GET_PROJECTION_BYTES: usize =
        MAX_TOUCHSTONE_RECORD_BYTES + 4 * MAX_TOUCHSTONE_INPUT_BYTES;
    if serde_json::to_vec(&value)?.len() > MAX_GET_PROJECTION_BYTES {
        return Err("touchstone GET metadata exceeds aggregate 96 KiB bound".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authored() -> Value {
        json!({"subject":"The scene that changed the checklist", "references":[{
            "db_id":Ulid::from(1_u128).to_string(), "id":Ulid::from(2_u128).to_string(),
            "expected_snapshot_sha256":"a".repeat(64)
        }]})
    }

    #[test]
    fn strict_authoring_rejects_null_unknown_duplicate_and_unbounded_fields() {
        let good = authored();
        serde_json::from_value::<AuthoredTouchstone>(good.clone())
            .unwrap()
            .typed()
            .unwrap();
        let mut cases = vec![
            json!(null),
            json!({}),
            json!({"subject":null,"references":[]}),
        ];
        for (field, value) in [
            ("subject", json!("x".repeat(257))),
            ("subject", json!(" untrimmed")),
            ("references", json!([])),
            ("references", json!(null)),
            ("extra", json!(1)),
        ] {
            let mut bad = good.clone();
            bad[field] = value;
            cases.push(bad);
        }
        let mut duplicate = good.clone();
        duplicate["references"] = json!([good["references"][0], good["references"][0]]);
        cases.push(duplicate);
        for (field, value) in [
            ("db_id", json!(null)),
            ("id", json!("invalid")),
            ("expected_snapshot_sha256", json!("A".repeat(64))),
            ("expected_snapshot_sha256", json!("é".repeat(32))),
            ("extra", json!(1)),
        ] {
            let mut bad = good.clone();
            bad["references"][0][field] = value;
            cases.push(bad);
        }
        for bad in cases {
            let rejected = match serde_json::from_value::<AuthoredTouchstone>(bad.clone()) {
                Err(_) => true,
                Ok(authored) => authored.typed().is_err(),
            };
            assert!(rejected, "accepted {bad}");
        }
    }

    #[tokio::test]
    async fn native_save_get_readback_and_indexed_browse_preserve_deleted_reference() {
        use mneme_core::ports::{GraphStore, SystemClock};
        use mneme_cozo::MemStore;
        use mneme_embed::{DEFAULT_DIM, HashingEmbedder};
        use mneme_engine::Config;
        use std::sync::Arc;

        let store = Arc::new(MemStore::new(DEFAULT_DIM));
        let memory = Memory::new(
            store.clone(),
            store.clone(),
            store.clone(),
            Arc::new(HashingEmbedder::new(DEFAULT_DIM)),
            Arc::new(SystemClock),
            Config {
                similarity_link_cap: 0,
                min_similarity_links: 0,
                ..Config::default()
            },
        )
        .with_body_store(Arc::new(mneme_body::InlineStore::new()));
        let target = crate::capture::PreparedCapture::parse(&json!({
            "source":{"namespace":"test","key":"scene","reference":"test:scene"},
            "summary":"The exact scene","body":"The scene body"
        }))
        .unwrap()
        .run(&memory, None)
        .await
        .unwrap();
        let snapshot = memory.summary_snapshot(target.id).await.unwrap().unwrap();
        let db_id = snapshot.db_id();
        let node = memory.get_node(target.id).await.unwrap().unwrap();
        let binding = node_touchstone_projection(&memory, db_id, &node)
            .await
            .unwrap();
        assert!(binding.get("touchstone").is_none());
        let mut reference = binding["summary_snapshot"].clone();
        reference.as_object_mut().unwrap().remove("coverage");
        let raw = json!({"summary":"The lesson","operation_id":"lesson",
            "touchstone":{"subject":"The scene","references":[reference]}});
        let proof = crate::save::PreparedSave::parse(&raw, "unused").unwrap();
        let frozen = crate::save::PreparedSave::parse(&raw, "unused")
            .unwrap()
            .into_json();
        let receipt = crate::save::PreparedSave::parse(&frozen, "")
            .unwrap()
            .run(&memory, db_id, None)
            .await
            .unwrap();
        let owner = proof.expected_id().unwrap();
        let owner_node = memory.get_node(owner).await.unwrap().unwrap();
        let projection = node_touchstone_projection(&memory, db_id, &owner_node)
            .await
            .unwrap();
        assert_eq!(
            projection["touchstone"]["references"][0]["summary"],
            "The exact scene"
        );
        assert_eq!(
            projection["touchstone_current"]["references"][0]["status"],
            "unchanged"
        );
        let mneme_core::Provenance::External { source } = owner_node.provenance() else {
            panic!("missing source")
        };
        let digest = source
            .request_digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut readback = json!({"id":owner,"summary":"The lesson","summary_truncated":false,
            "provenance":{"type":"external","source":{
                "namespace":source.namespace(),"key":source.key(),"reference":source.reference(),
                "session":source.session(),"revision":source.revision(),
                "request_digest_sha256":digest,"request_codec":source.request_codec()}},
            "body":"The lesson","body_range":{"source_start":0,"source_end":10,"next_offset":null,"has_more":false}});
        readback["touchstone"] = projection["touchstone"].clone();
        proof.verify_readback_json(&receipt, &readback).unwrap();
        let mut corrupted = readback.clone();
        corrupted["touchstone"]["subject"] = json!("An invented subject");
        assert!(proof.verify_readback_json(&receipt, &corrupted).is_err());
        store.delete_node(target.id).await.unwrap();
        let after_delete = node_touchstone_projection(&memory, db_id, &owner_node)
            .await
            .unwrap();
        assert_eq!(after_delete["touchstone"], projection["touchstone"]);
        assert_eq!(
            after_delete["touchstone_current"]["references"][0]["status"],
            "absent"
        );
        let replay = crate::save::PreparedSave::parse(&frozen, "")
            .unwrap()
            .run(&memory, db_id, None)
            .await
            .unwrap();
        assert_eq!(replay["replayed"], true);
        proof.verify_readback_json(&replay, &readback).unwrap();
        let mut bad_replay = readback.clone();
        bad_replay["summary"] = json!("An edited owner");
        assert!(proof.verify_readback_json(&replay, &bad_replay).is_err());
        let page = PreparedTouchstoneList::parse(&json!({"kind":"touchstones","limit":1}))
            .unwrap()
            .run(&memory, db_id)
            .await
            .unwrap();
        assert_eq!(page["items"][0]["id"], json!(owner));
        // A full bounded page cannot prove exhaustion without hidden lookahead.
        // Follow the continuation even when the last delivered row is the last
        // owner: an empty final page is the native completion signal.
        assert_eq!(page["has_more"], true);
        assert!(page["next_cursor"].is_string());
        let end = PreparedTouchstoneList::parse(&json!({"kind":"touchstones","limit":1,
            "after":page["next_cursor"]}))
        .unwrap()
        .run(&memory, db_id)
        .await
        .unwrap();
        assert_eq!(end["items"], json!([]));
        assert_eq!(end["has_more"], false);
        assert!(end["next_cursor"].is_null());
        assert!(page["items"][0].get("references").is_none());
        assert!(page["items"][0].get("body").is_none());
    }

    #[test]
    fn browse_parser_is_strict_and_roundtrips_omission() {
        let value = json!({"kind":"touchstones"});
        assert_eq!(
            PreparedTouchstoneList::parse(&value).unwrap().into_json(),
            value
        );
        for bad in [
            json!({}),
            json!({"kind":"note"}),
            json!({"kind":"touchstones","limit":0}),
            json!({"kind":"touchstones","limit":33}),
            json!({"kind":"touchstones","limit":null}),
            json!({"kind":"touchstones","after":null}),
            json!({"kind":"touchstones","after":""}),
            json!({"kind":"touchstones","after":"garbage"}),
            json!({"kind":"touchstones","subject":"filter"}),
        ] {
            assert!(
                PreparedTouchstoneList::parse(&bad).is_err(),
                "accepted {bad}"
            );
        }
    }
}
