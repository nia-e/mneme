//! One complete typed capture envelope shared by MCP admission and execution.
//! The outer tool schema is discovery UX; this parser is the authority boundary.

use mneme_core::{
    CaptureReplayProof, CaptureRequestCodec, CaptureSource, EdgeKind, MAX_TAG_BYTES,
    ports::MAX_CAPTURE_EDGES,
};
use mneme_engine::{Capture, CaptureLink, CaptureResult, Memory};
use serde::Deserialize;
use serde_json::{Value, json};

use std::error::Error;
use ulid::Ulid;

pub type CaptureError = Box<dyn Error + Send + Sync>;

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_SUMMARY_BYTES: usize = 2 * 1024;
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_TAGS: usize = 32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceInput {
    namespace: String,
    key: String,
    reference: String,
    session: Option<String>,
    revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkInput {
    to: String,
    kind: Option<String>,
    weight: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedCapture {
    source: SourceInput,
    summary: String,
    body: Option<String>,
    tags: Option<Vec<String>>,
    stability: Option<f32>,
    confidence: Option<f32>,
    links: Option<Vec<LinkInput>>,
    touchstone: Option<crate::touchstone::AuthoredTouchstone>,
}

fn reject_null_optionals(
    object: &serde_json::Map<String, Value>,
    names: &[&str],
) -> Result<(), CaptureError> {
    for name in names {
        if object.get(*name).is_some_and(Value::is_null) {
            return Err(format!("capture field `{name}` must not be null").into());
        }
    }
    Ok(())
}

fn bounded_nonempty(name: &str, value: &str, max: usize) -> Result<(), CaptureError> {
    if value.is_empty() || value.len() > max {
        return Err(format!("capture field `{name}` must be 1..={max} UTF-8 bytes").into());
    }
    if value.trim() != value || value.chars().any(char::is_control) {
        return Err(
            format!("capture field `{name}` must be trimmed and contain no controls").into(),
        );
    }
    Ok(())
}

impl PreparedCapture {
    pub fn parse(raw: &Value) -> Result<Self, CaptureError> {
        if serde_json::to_vec(raw)?.len() > MAX_INPUT_BYTES {
            return Err(format!("capture input exceeds {MAX_INPUT_BYTES} UTF-8 bytes").into());
        }
        let object = raw
            .as_object()
            .ok_or("capture arguments must be an object")?;
        reject_null_optionals(
            object,
            &[
                "body",
                "tags",
                "stability",
                "confidence",
                "links",
                "touchstone",
            ],
        )?;
        let source = object
            .get("source")
            .and_then(Value::as_object)
            .ok_or("capture field `source` must be an object")?;
        reject_null_optionals(source, &["session", "revision"])?;
        if let Some(links) = object.get("links").and_then(Value::as_array) {
            for (index, link) in links.iter().enumerate() {
                let item = link
                    .as_object()
                    .ok_or_else(|| format!("capture links[{index}] must be an object"))?;
                reject_null_optionals(item, &["kind", "weight"])?;
            }
        }
        let input: Self = serde_json::from_value(raw.clone())?;
        input.validate()?;
        Ok(input)
    }

    fn validate(&self) -> Result<(), CaptureError> {
        bounded_nonempty("source.namespace", &self.source.namespace, 64)?;
        if !self.source.namespace.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        }) {
            return Err("capture source.namespace must be a lowercase ASCII identifier".into());
        }
        bounded_nonempty("source.key", &self.source.key, 512)?;
        bounded_nonempty("source.reference", &self.source.reference, 2048)?;
        if let Some(value) = &self.source.session {
            bounded_nonempty("source.session", value, 512)?;
        }
        if let Some(value) = &self.source.revision {
            bounded_nonempty("source.revision", value, 256)?;
        }
        bounded_nonempty("summary", &self.summary, MAX_SUMMARY_BYTES)?;
        if self
            .body
            .as_ref()
            .is_some_and(|body| body.len() > MAX_BODY_BYTES)
        {
            return Err(format!("capture body exceeds {MAX_BODY_BYTES} UTF-8 bytes").into());
        }
        let tags = self.tags.as_deref().unwrap_or(&[]);
        if tags.len() > MAX_TAGS {
            return Err(format!("capture tags exceed {MAX_TAGS} entries").into());
        }
        for tag in tags {
            bounded_nonempty("tags[]", tag, MAX_TAG_BYTES)?;
        }
        for (name, value) in [
            ("stability", self.stability),
            ("confidence", self.confidence),
        ] {
            if let Some(value) = value
                && (!value.is_finite() || !(0.0..=1.0).contains(&value))
            {
                return Err(format!("capture {name} must be finite and in 0..=1").into());
            }
        }
        self.capture_links()?;
        if let Some(touchstone) = &self.touchstone {
            touchstone.typed()?;
        }
        Ok(())
    }

    fn capture_links(&self) -> Result<Vec<CaptureLink>, CaptureError> {
        let input = self.links.as_deref().unwrap_or(&[]);
        if input.len() > MAX_CAPTURE_EDGES {
            return Err(format!("capture links exceed {MAX_CAPTURE_EDGES} edges").into());
        }
        let source = CaptureSource::new(
            &self.source.namespace,
            &self.source.key,
            &self.source.reference,
            self.source.session.as_deref(),
            self.source.revision.as_deref(),
            [0; 32],
        )?;
        let mut targets = std::collections::HashSet::new();
        let mut links = Vec::with_capacity(input.len());
        for link in input {
            let to = mneme_core::NodeId(
                Ulid::from_string(&link.to)
                    .map_err(|e| format!("invalid node id {:?}: {e}", link.to))?,
            );
            if to == source.node_id() || !targets.insert(to) {
                return Err("capture links require distinct, non-self targets".into());
            }
            let kind = match link.kind.as_deref().unwrap_or("associative") {
                "associative" => EdgeKind::Associative,
                "transition" => EdgeKind::Transition,
                "derived_from" => EdgeKind::DerivedFrom,
                _ => {
                    return Err(
                        "capture link kind must be associative|transition|derived_from".into(),
                    );
                }
            };
            links.push(CaptureLink::new(to, kind, link.weight.unwrap_or(0.5))?);
        }
        Ok(links)
    }

    pub fn requires_operator(&self) -> bool {
        if self
            .tags
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|tag| tag == "core")
        {
            true
        } else {
            false
        }
    }

    /// Preserve omission of optional fields when forwarding to a remote owner.
    pub fn into_json(self) -> Value {
        let mut source = json!({"namespace": self.source.namespace, "key": self.source.key, "reference": self.source.reference});
        if let Some(value) = self.source.session {
            source["session"] = json!(value);
        }
        if let Some(value) = self.source.revision {
            source["revision"] = json!(value);
        }
        let mut value = json!({"source": source, "summary": self.summary});
        if let Some(body) = self.body {
            value["body"] = json!(body);
        }
        if let Some(tags) = self.tags {
            value["tags"] = json!(tags);
        }
        if let Some(stability) = self.stability {
            value["stability"] = json!(stability);
        }
        if let Some(confidence) = self.confidence {
            value["confidence"] = json!(confidence);
        }
        if let Some(links) = self.links {
            value["links"] = json!(
                links
                    .into_iter()
                    .map(|link| {
                        let mut item = json!({"to": link.to});
                        if let Some(kind) = link.kind {
                            item["kind"] = json!(kind);
                        }
                        if let Some(weight) = link.weight {
                            item["weight"] = json!(weight);
                        }
                        item
                    })
                    .collect::<Vec<_>>()
            );
        }
        if let Some(touchstone) = self.touchstone {
            value["touchstone"] = json!(touchstone);
        }
        value
    }

    pub fn has_links(&self) -> bool {
        self.links.as_ref().is_some_and(|links| !links.is_empty())
    }

    pub fn expected_body(&self) -> &[u8] {
        self.body.as_deref().unwrap_or(&self.summary).as_bytes()
    }

    fn build_capture<'a>(
        &'a self,
        tags: &'a [&'a str],
        links: &'a [CaptureLink],
        origin_commit: Option<&str>,
    ) -> Result<Capture<'a>, CaptureError> {
        let mut capture = Capture::new(
            &self.source.namespace,
            &self.source.key,
            &self.source.reference,
            self.source.session.as_deref(),
            self.source.revision.as_deref(),
            &self.summary,
            self.expected_body(),
            tags,
        )
        .with_origin_commit(origin_commit)?
        .with_links(links);
        if let Some(touchstone) = &self.touchstone {
            capture = capture.with_touchstone(touchstone.typed()?);
        }
        if let Some(value) = self.stability {
            capture = capture.with_stability(value);
        }
        if let Some(value) = self.confidence {
            capture = capture.with_confidence(value);
        }
        Ok(capture)
    }

    /// Compute the exact engine-owned capture identity and request digest, without I/O.
    pub fn expected_source(&self) -> Result<CaptureSource, CaptureError> {
        Ok(self.expected_replay_proof()?.source().clone())
    }

    /// Prove replay from the original incoming request, including historical
    /// status-byte digests. Never derive this from the current mutable node.
    pub fn expected_replay_proof(&self) -> Result<CaptureReplayProof, CaptureError> {
        let tags = self.tags.as_deref().unwrap_or(&[]);
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let links = self.capture_links()?;
        Ok(self
            .build_capture(&tag_refs, &links, None)?
            .validated_replay_proof()?)
    }

    /// Check an MCP `get` result against the immutable original request proof.
    /// A replay may have been explicitly edited since the first capture; only a
    /// newly applied write promises its current summary/body still match.
    pub fn verify_readback_json(&self, node: &Value, replayed: bool) -> Result<(), CaptureError> {
        let proof = self.expected_replay_proof()?;
        let expected = proof.source();
        let source = &node["provenance"]["source"];
        for optional in ["session", "revision"] {
            if !source[optional].is_null() && !source[optional].is_string() {
                return Err(format!("capture readback source {optional} has invalid type").into());
            }
        }
        let digest_hex = source["request_digest_sha256"]
            .as_str()
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or("capture readback missing bounded request digest")?;
        let mut digest = [0_u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&digest_hex[index * 2..index * 2 + 2], 16)?;
        }
        let codec: CaptureRequestCodec = serde_json::from_value(source["request_codec"].clone())?;
        let stored = CaptureSource::new_with_codec(
            source["namespace"]
                .as_str()
                .ok_or("capture readback missing namespace")?,
            source["key"]
                .as_str()
                .ok_or("capture readback missing key")?,
            source["reference"]
                .as_str()
                .ok_or("capture readback missing reference")?,
            source["session"].as_str(),
            source["revision"].as_str(),
            digest,
            codec,
        )?;
        let matching = node["id"] == expected.node_id().0.to_string()
            && node["provenance"]["type"] == "external"
            && proof.matches_source(&stored);
        if !matching {
            return Err("capture readback identity or provenance mismatch".into());
        }
        if let Some(touchstone) = &self.touchstone {
            touchstone.verify_record_json(&node["touchstone"], expected.node_id())?;
        }
        // The native get must resolve the *current* body, including on an
        // exact replay whose canonical text was edited after first capture.
        let current_body = node["body"]
            .as_str()
            .ok_or("capture readback current body is unavailable")?;
        let range = &node["body_range"];
        if !node["summary"].is_string()
            || range["source_start"] != 0
            || range["source_end"] != current_body.len()
            || !range["next_offset"].is_null()
            || range["has_more"] != false
        {
            return Err("capture readback current summary or body is incomplete".into());
        }
        // Unlike ordinary mutable captures, a typed owner cannot be edited:
        // replay still promises its authored summary/body remain intact.
        if !replayed || self.touchstone.is_some() {
            if node["summary"] != self.summary || node["summary_truncated"] != false {
                return Err(if self.touchstone.is_some() {
                    "touchstone owner readback summary is missing or changed"
                } else {
                    "fresh capture readback summary is missing or changed"
                }
                .into());
            }
            let body = self.expected_body();
            if node["body"] != std::str::from_utf8(body)? || current_body.len() != body.len() {
                return Err(if self.touchstone.is_some() {
                    "touchstone owner readback body is missing, truncated, or changed"
                } else {
                    "fresh capture readback body is missing, truncated, or changed"
                }
                .into());
            }
        }
        Ok(())
    }

    pub async fn run(
        self,
        mem: &Memory,
        origin_commit: Option<&str>,
    ) -> Result<CaptureResult, CaptureError> {
        let tags = self.tags.as_deref().unwrap_or(&[]);
        let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        let links = self.capture_links()?;
        let capture = self.build_capture(&tag_refs, &links, origin_commit)?;
        Ok(mem.capture(capture).await?)
    }
}

pub fn properties() -> Value {
    let mut properties = json!({
        "source": {
            "type": "object", "additionalProperties": false,
            "required": ["namespace", "key", "reference"],
            "properties": {
                "namespace": { "type": "string", "minLength": 1, "maxLength": 64, "description": "lowercase ASCII source namespace" },
                "key": { "type": "string", "minLength": 1, "maxLength": 512, "description": "stable logical claim identity within namespace and database, not a content hash" },
                "reference": { "type": "string", "minLength": 1, "maxLength": 2048, "description": "source locator or citation" },
                "session": { "type": "string", "minLength": 1, "maxLength": 512 },
                "revision": { "type": "string", "minLength": 1, "maxLength": 256 }
            }
        },
        "summary": { "type": "string", "minLength": 1, "maxLength": MAX_SUMMARY_BYTES },
        "body": { "type": "string", "maxLength": MAX_BODY_BYTES, "description": "full content, defaults to summary" },
        "tags": { "type": "array", "maxItems": MAX_TAGS, "items": { "type": "string", "minLength": 1, "maxLength": MAX_TAG_BYTES } },
        "stability": { "type": "number", "minimum": 0, "maximum": 1 },
        "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
        "links": {
            "type": "array", "maxItems": MAX_CAPTURE_EDGES,
            "description": "up to 8 explicit, atomic links from the captured node to existing nodes in this database; no inferred similarity links. Exact retries require the same links; supersession is a separate operation",
            "items": {
                "type": "object", "additionalProperties": false, "required": ["to"],
                "properties": {
                    "to": { "type": "string", "description": "existing same-database node ULID" },
                    "kind": { "type": "string", "enum": ["associative", "transition", "derived_from"], "description": "default associative" },
                    "weight": { "type": "number", "minimum": 0, "maximum": 1, "description": "default 0.5; finite" }
                }
            }
        }
    });
    properties["touchstone"] = crate::touchstone::authoring_schema();
    properties
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> Value {
        json!({"source":{"namespace":"codex","key":"claim-1","reference":"codex://thread/1"},"summary":"A recorded claim"})
    }

    fn touchstone_candidate() -> Value {
        let mut raw = candidate();
        raw["touchstone"] = json!({"subject":"The scene matters", "references":[{
            "db_id":Ulid::from(1_u128).to_string(),"id":Ulid::from(2_u128).to_string(),
            "expected_snapshot_sha256":"a".repeat(64)
        }]});
        raw
    }

    #[test]
    fn touchstone_roundtrip_and_request_codec_are_distinct_without_changing_normal_capture() {
        let raw = touchstone_candidate();
        let prepared = PreparedCapture::parse(&raw).unwrap();
        let source = prepared.expected_source().unwrap();
        assert_eq!(source.request_codec(), CaptureRequestCodec::TouchstoneV1);
        let frozen = prepared.into_json();
        assert_eq!(frozen, raw);
        assert_eq!(
            PreparedCapture::parse(&frozen)
                .unwrap()
                .expected_source()
                .unwrap(),
            source
        );
        let normal = PreparedCapture::parse(&candidate())
            .unwrap()
            .expected_source()
            .unwrap();
        assert_eq!(normal.request_codec(), CaptureRequestCodec::CaptureV2);
        assert_ne!(normal.request_digest(), source.request_digest());
        let mut bad = raw;
        bad["touchstone"] = json!(null);
        assert!(PreparedCapture::parse(&bad).is_err());
    }

    #[test]
    fn touchstone_reference_order_is_not_request_identity_but_expected_hash_is() {
        let mut raw = touchstone_candidate();
        let mut second = raw["touchstone"]["references"][0].clone();
        second["id"] = json!(Ulid::from(3_u128).to_string());
        raw["touchstone"]["references"]
            .as_array_mut()
            .unwrap()
            .push(second);
        let source = PreparedCapture::parse(&raw)
            .unwrap()
            .expected_source()
            .unwrap();
        raw["touchstone"]["references"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert_eq!(
            PreparedCapture::parse(&raw)
                .unwrap()
                .expected_source()
                .unwrap(),
            source
        );
        raw["touchstone"]["references"][0]["expected_snapshot_sha256"] = json!("b".repeat(64));
        let changed = PreparedCapture::parse(&raw)
            .unwrap()
            .expected_source()
            .unwrap();
        assert_eq!(changed.node_id(), source.node_id());
        assert_ne!(changed.request_digest(), source.request_digest());
    }

    #[test]
    fn preflight_rejects_nullable_unknown_and_invalid_links() {
        let base = candidate();
        let target = Ulid::new();
        let mut cases = vec![
            json!({"source":null,"summary":"x"}),
            json!({"source":{"namespace":"codex","key":"k","reference":"r","session":null},"summary":"x"}),
            json!({"source":{"namespace":"codex","key":"k","reference":"r","extra":1},"summary":"x"}),
            json!({"source":{"namespace":"codex","key":"k","reference":"r"},"summary":"x","active":null}),
        ];
        for links in [
            json!(null),
            json!({}),
            json!([null]),
            json!([{"to":target.to_string(),"kind":null}]),
            json!([{"to":target.to_string(),"weight":2}]),
            json!([{"to":target.to_string()},{"to":target.to_string()}]),
            json!(vec![
                json!({"to":target.to_string()});
                MAX_CAPTURE_EDGES + 1
            ]),
        ] {
            let mut value = base.clone();
            value["links"] = links;
            cases.push(value);
        }
        for case in cases {
            assert!(PreparedCapture::parse(&case).is_err(), "{case}");
        }
    }

    #[test]
    fn identity_and_readback_use_engine_digest_even_with_empty_body() {
        let mut value = candidate();
        value["body"] = json!("");
        let prepared = PreparedCapture::parse(&value).unwrap();
        let source = prepared.expected_source().unwrap();
        let digest: String = source
            .request_digest()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let readback = json!({
            "id":source.node_id().0.to_string(),"summary":"A recorded claim","summary_truncated":false,
            "provenance":{"type":"external","source":{"namespace":"codex","key":"claim-1","reference":"codex://thread/1","session":null,"revision":null,"request_digest_sha256":digest,"request_codec":"capture_v2"}},
            "body":"","body_range":{"source_start":0,"source_end":0,"next_offset":null,"has_more":false}
        });
        prepared.verify_readback_json(&readback, false).unwrap();
        let mut truncated = readback.clone();
        truncated["body_range"]["has_more"] = json!(true);
        assert!(prepared.verify_readback_json(&truncated, false).is_err());
        assert_eq!(prepared.expected_body(), b"");
    }

    #[test]
    fn omission_and_operator_authority() {
        let prepared = PreparedCapture::parse(&candidate()).unwrap();
        assert!(!prepared.has_links());
        assert!(!prepared.requires_operator());
        assert!(prepared.into_json().get("links").is_none());
        let mut active = candidate();
        active["active"] = json!(true);
        assert!(PreparedCapture::parse(&active).is_err());
        let mut core = candidate();
        core["tags"] = json!(["core"]);
        assert!(PreparedCapture::parse(&core).unwrap().requires_operator());
    }

    #[test]
    fn normalized_legacy_replay_proves_original_request_not_current_body() {
        let prepared = PreparedCapture::parse(&candidate()).unwrap();
        let proof = prepared.expected_replay_proof().unwrap();
        let (legacy_digest, source) = match proof {
            CaptureReplayProof::Semantic {
                legacy_candidate_digest,
                source,
                ..
            } => (legacy_candidate_digest, source),
            CaptureReplayProof::Episode { .. } | CaptureReplayProof::Touchstone { .. } => {
                panic!("ordinary semantic capture has another proof codec")
            }
        };
        let digest: String = legacy_digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let readback = json!({
            "id":source.node_id().0.to_string(),
            "summary":"This node was deliberately edited later", "summary_truncated":false,
            "provenance":{"type":"external","source":{
                "namespace":"codex","key":"claim-1","reference":"codex://thread/1",
                "session":null,"revision":null,
                "request_digest_sha256":digest,"request_codec":"capture_v1"
            }},
            "body":"Updated body", "body_range":{"source_start":0,"source_end":12,"next_offset":null,"has_more":false}
        });
        prepared.verify_readback_json(&readback, true).unwrap();
        assert!(prepared.verify_readback_json(&readback, false).is_err());
        let mut changed = candidate();
        changed["summary"] = json!("A changed original request");
        assert!(
            PreparedCapture::parse(&changed)
                .unwrap()
                .verify_readback_json(&readback, true)
                .is_err()
        );
    }
}
