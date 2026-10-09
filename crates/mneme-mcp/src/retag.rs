//! Native owner adapter; complete admission precedes profile and checkout.
use crate::{AnyErr, CapabilityProfile};
use mneme_app::retag::{self as shared, PreparedRetag as Domain};
use serde_json::{Value, json};
pub(crate) struct PreparedRetag {
    pub(crate) inner: Domain,
}
impl PreparedRetag {
    pub(crate) fn parse(raw: &Value) -> Result<Self, AnyErr> {
        if serde_json::to_vec(raw)?.len() > shared::MAX_RETAG_REQUEST_BYTES {
            return Err("retag input exceeds its encoded byte allowance".into());
        }
        let mut object = raw
            .as_object()
            .ok_or("retag arguments must be an object")?
            .clone();
        let db = object
            .remove("db")
            .ok_or("retag requires explicit argument `db`")?;
        let db = db.as_str().ok_or("retag db must be a string")?;
        if db.is_empty() || db.len() > 256 || db.trim() != db || db.chars().any(char::is_control) {
            return Err("retag db must be 1..=256 UTF-8 bytes, trimmed, without controls".into());
        }
        if crate::optional_expected_db_id(raw)?.is_none() {
            return Err("retag requires canonical argument `expected_db_id`".into());
        }
        object.remove("expected_db_id");
        Ok(Self {
            inner: Domain::parse(&Value::Object(object))?,
        })
    }
}
pub(crate) fn tool_schema(profile: CapabilityProfile) -> Value {
    let mut schema = shared::input_schema();
    schema["properties"]["db"] = crate::db_prop();
    schema["properties"]["expected_db_id"] = crate::expected_db_id_prop();
    schema["required"] = json!(["db", "expected_db_id", "id", "expected_tags", "tags"]);
    if profile != CapabilityProfile::Operator {
        for field in ["expected_tags", "tags"] {
            schema["properties"][field]["items"]["not"] = json!({"const":"core"});
        }
    }
    json!({"name":"retag","description":"Replace a semantic node's complete tag set only if its current tags equal expected_tags. Optional expected_content_fingerprint (from get) and guard_nodes check target and semantic guide content in the same write snapshot; guard_nodes requires the target fingerprint. Fingerprints bind content pointers, not mutable external body bytes. Requires curator, explicit db and expected_db_id; operator if either tag set contains core. Empty sets are valid. Preserves authored content and other node state. Stale guards refuse; inspect before forming new intent. No automatic retry, content edit, or revision-history promise.","inputSchema":schema})
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{empty_profile_server, schema_accepts, tool_input_schema};
    use crate::*;
    fn raw(core: bool) -> Value {
        json!({"db":"missing","expected_db_id":ulid::Ulid::from(2).to_string(),"id":ulid::Ulid::from(1).to_string(),"expected_tags":if core {vec!["core"]}else{vec![]},"tags":[]})
    }
    #[tokio::test]
    async fn profile_catalog_dispatch_denial_precedes_checkout() {
        for profile in [
            CapabilityProfile::ReadOnly,
            CapabilityProfile::ReceiptGrounded,
            CapabilityProfile::Curator,
            CapabilityProfile::Operator,
        ] {
            let server = empty_profile_server(profile);
            let schemas = tool_schemas(server.capability);
            let schema = schemas
                .iter()
                .find(|tool| tool["name"] == "retag")
                .map(|_| tool_input_schema(&schemas, "retag"));
            let _held = server.cold_work.admit("held lane").unwrap();
            for (core, guards) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut arguments = raw(core);
                if guards {
                    arguments["expected_content_fingerprint"] = json!("a".repeat(64));
                    arguments["guard_nodes"] = json!([{"id":ulid::Ulid::from(3).to_string(),"content_fingerprint":"b".repeat(64)}]);
                }
                let class = if core {
                    CapabilityClass::Operator
                } else {
                    CapabilityClass::Curator
                };
                let admitted = ValidatedToolArguments::parse("retag", &arguments).unwrap();
                assert_eq!(admitted.kind().capability_class(), class);
                let allowed = profile.permits(class);
                assert_eq!(
                    schema.is_some_and(|s| schema_accepts(s, &arguments)),
                    allowed
                );
                let response=server.handle(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"retag","arguments":arguments}})).await.unwrap();
                let decoded: Value = serde_json::from_slice(response.bytes()).unwrap();
                let error = decoded["result"]["content"][0]["text"].as_str().unwrap();
                assert_eq!(error.contains("capability profile"), !allowed, "{error}");
                assert_eq!(error.contains("unknown database"), allowed, "{error}");
                assert!(!error.contains("cold work"));
            }
        }
    }
    #[test]
    fn malformed_closed_guarded_request() {
        for field in ["id", "expected_tags", "tags", "db", "expected_db_id"] {
            let mut r = raw(false);
            r.as_object_mut().unwrap().remove(field);
            assert!(ValidatedToolArguments::parse("retag", &r).is_err());
        }
        let mut r = raw(false);
        r["tags"] = json!(["dup", "dup"]);
        assert!(ValidatedToolArguments::parse("retag", &r).is_err());
        r = raw(false);
        r["extra"] = json!(1);
        assert!(ValidatedToolArguments::parse("retag", &r).is_err());
        let schema = tool_schema(CapabilityProfile::Curator)["inputSchema"].clone();
        for fields in [
            json!({"expected_content_fingerprint":"A".repeat(64)}),
            json!({"guard_nodes":[]}),
            json!({"expected_content_fingerprint":"a".repeat(64),"guard_nodes":[{"id":ulid::Ulid::from(3).to_string(),"content_fingerprint":null}]}),
        ] {
            let mut r = raw(false);
            r.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert!(!schema_accepts(&schema, &r), "{r}");
            assert!(ValidatedToolArguments::parse("retag", &r).is_err());
        }
    }
}

#[cfg(all(test, not(feature = "fastembed")))]
#[tokio::test]
async fn retag_native_cas_preserves_content_identity_and_refuses_stale_guard() {
    use crate::*;
    let (root, server, nodes) = crate::concern::tests::fixture().await;
    let db_id = server.registry.checkout("project").unwrap().db_id;
    let original = nodes[0].clone();
    let raw = json!({"db":"project","expected_db_id":db_id.to_string(),"id":original.id().0.to_string(),"expected_tags":["fact"],"tags":["closed","possibility"]});
    let call = |args: Value| {
        let server = &server;
        async move {
            call_tool_authorized(
                &server.registry,
                &server.sessions,
                &server.cold_work,
                server.capability,
                "retag",
                &args,
            )
            .await
        }
    };
    let mut strong = raw.clone();
    strong["expected_content_fingerprint"] = json!("0".repeat(64));
    assert!(call(strong.clone()).await.is_err());
    strong["expected_content_fingerprint"] =
        json!(mneme_core::ports::routing_content_fingerprint(&original));
    // The target guard is opt-in; legacy callers retain their old tag-only CAS.
    let reply = call(strong).await.unwrap();
    assert_eq!(
        reply,
        json!({"id":original.id().0.to_string(),"tags":["closed","possibility"],"changed":true,"db":"project","db_id":db_id.to_string()})
    );
    let read = server
        .registry
        .checkout("project")
        .unwrap()
        .mem
        .get_node(original.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.summary(), original.summary());
    assert_eq!(read.body(), original.body());
    assert_eq!(read.provenance(), original.provenance());
    assert_eq!(
        read.tag_set(),
        &mneme_core::BoundedTagSet::try_from_iter(["closed", "possibility"]).unwrap()
    );
    assert!(call(raw.clone()).await.is_err());
    let mut guarded = raw.clone();
    guarded["expected_tags"] = reply["tags"].clone();
    guarded["expected_db_id"] = json!(ulid::Ulid::new().to_string());
    assert!(call(guarded).await.is_err());
    let mut unchanged = raw;
    unchanged["expected_tags"] = reply["tags"].clone();
    let reply = call(unchanged).await.unwrap();
    assert_eq!(reply["changed"], false);
    drop(server);
    std::fs::remove_dir_all(root).unwrap();
}
