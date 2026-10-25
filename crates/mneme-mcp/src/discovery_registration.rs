//! Add-only registration into explicitly writable, identity-bound hub catalogs.
//! Request validation is pure; filesystem work runs in a bounded blocking lane.

use crate::{AnyErr, Registry, expected_db_id_prop, optional_expected_db_id};
use mneme_app::discovery_registration::{RegistrationRequest, register_catalog};
use mneme_config::StoreDeclaration;
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) struct Request {
    db: String,
    expected_db_id: ulid::Ulid,
    registration: RegistrationRequest,
}

impl Request {
    pub(crate) fn parse(arguments: &Value) -> Result<Self, AnyErr> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            db: String,
            expected_db_id: String,
            label: String,
            store: StoreDeclaration,
            #[serde(default)]
            expected_catalog_sha256: Option<String>,
        }
        // Bound the complete wire object before cloning/deserializing its store.
        if serde_json::to_vec(arguments)?.len()
            > mneme_app::discovery_registration::MAX_REGISTRATION_BYTES + 1024
        {
            return Err("discovery_register request exceeds its bounded registration size".into());
        }
        let wire: Wire = serde_json::from_value(arguments.clone())?;
        if wire.db.is_empty() || wire.db.chars().count() > 256 {
            return Err(
                "discovery_register db must be a nonempty owner alias of at most 256 characters"
                    .into(),
            );
        }
        let expected_db_id = optional_expected_db_id(arguments)?
            .ok_or("discovery_register requires canonical expected_db_id")?;
        debug_assert_eq!(wire.expected_db_id, expected_db_id.to_string());
        let registration = RegistrationRequest {
            label: wire.label,
            store: wire.store,
            expected_catalog_sha256: wire.expected_catalog_sha256,
        };
        registration.validate()?;
        Ok(Self {
            db: wire.db,
            expected_db_id,
            registration,
        })
    }
}

pub(crate) async fn register(registry: &Registry, request: Request) -> Result<Value, AnyErr> {
    let slot = registry.slot(&request.db)?.clone();
    crate::require_expected_db_id(
        &request.db,
        slot.status()?.db_id,
        Some(request.expected_db_id),
    )?;
    let source = registry.discovery_catalogs.get(&request.db)
        .ok_or("discovery_register is not configured for this owner; ask the operator to configure --discovery-catalog ALIAS=PATH and --discovery-catalog-writable ALIAS")?;
    if !source.writable {
        return Err("discovery_register publication is read-only; ask the operator to explicitly enable --discovery-catalog-writable ALIAS".into());
    }
    // The alias is reusable; the admitted source is not. Neither request pins
    // nor a replacement owner can rebind this path to a different publisher.
    crate::require_expected_db_id(
        &request.db,
        source.publisher_db_id,
        Some(request.expected_db_id),
    )?;
    let publisher_db_id = source.publisher_db_id;
    let path = source.path.clone();
    tokio::task::spawn_blocking(move || {
        slot.with_retained_identity(publisher_db_id, || {
            Ok(serde_json::to_value(register_catalog(
                &path,
                &publisher_db_id.to_string(),
                &request.registration,
            )?)?)
        })
    })
    .await?
}

fn closed(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","additionalProperties":false,"properties":properties,"required":required})
}
fn nullable(schema: Value) -> Value {
    json!({"anyOf":[schema,{"type":"null"}]})
}
fn sparse(properties: Value) -> Value {
    let properties = properties
        .as_object()
        .expect("schema properties")
        .iter()
        .map(|(key, value)| (key.clone(), nullable(value.clone())))
        .collect::<serde_json::Map<_, _>>();
    closed(Value::Object(properties), &[])
}

/// Describe the existing closed StoreDeclaration DTO, not another registration
/// dialect. Shared configuration validation additionally enforces imported-policy
/// semantics (canonical IDs, bounded paths, no secret references, safe routes).
fn store_schema() -> Value {
    let text = json!({"type":"string"});
    let boolean = json!({"type":"boolean"});
    let strings = json!({"type":"array","items":text});
    let owner = closed(
        json!({
            "url":text,"db":text,
            "ssh_mcp_port":nullable(json!({"type":"integer","minimum":1,"maximum":65535})),
            "path":nullable(text.clone()),"token_env":{"type":"null"}
        }),
        &["url", "db"],
    );
    let scope = sparse(json!({"roots":strings,"subtree_roots":strings,"excluded_roots":strings}));
    let mut properties = json!({
        "owner":nullable(owner.clone()),
        "discovery":nullable(boolean.clone()),
        "active":nullable(sparse(json!({
            "enabled":boolean,"workspace":{"type":"string","enum":["any","project","no-project"]},
            "roots":strings,"subtree_roots":strings,"profiles":strings,"excluded_roots":strings
        }))),
        "permissions":nullable(sparse(json!({"read":boolean,"write":boolean}))),
        "delivery":nullable(sparse(json!({"core":boolean,"context":boolean,"contradiction_notice":boolean}))),
        "background":nullable(sparse(json!({"recall":boolean,"record":boolean,"maintenance":boolean,"core_cleanup":boolean}))),
        "guidance":nullable(sparse(json!({"tag_guide":closed(json!({"id":expected_db_id_prop(),"content":{"type":"string","enum":["summary"]}}), &["id","content"])}))),
        "cache":nullable(closed(json!({
            "library":text,"mode":{"type":"string","enum":["owner-first"]},
            "populate":nullable(closed(json!({"refresh_after_secs":{"type":"integer"},"max_snapshot_bytes":{"type":"integer"}}), &[]))
        }), &["library"])),
        "retrieval":nullable(closed(json!({"tags":strings}), &["tags"])),
        "applicability":nullable(sparse(json!({"core":scope,"context":scope,"recall":scope,"record":scope,"maintenance":scope}))),
        "capture":nullable(sparse(json!({
            "intent":{"type":"string","enum":["ordinary","collaboration-preferences","cross-workspace"]},
            "kinds":{"type":"array","items":{"type":"string","enum":["lesson","episode","possibility"]}},
            "evidence_kinds":{"type":"array","items":{"type":"string","enum":["user_statement","assistant_assertion","tool_call","tool_result","memory_delivery"]}},
            "required_tags":strings,"allow_associations":boolean,
            "max_summary_bytes":{"type":"integer","maximum":4294967295_u64},
            "max_body_bytes":{"type":"integer","maximum":4294967295_u64}
        }))),
        "clear":strings
    });
    properties["capture"]["anyOf"][0]["properties"]["required_tags"] = strings;
    // Device routes are complete bundles, not sparse owner overrides.
    let mut device_properties = properties.clone();
    device_properties["owner"] = nullable(owner);
    properties["devices"] = json!({"type":"object","maxProperties":32,"additionalProperties":closed(device_properties,&[])});
    properties["id"] = expected_db_id_prop();
    closed(properties, &["id"])
}

pub(crate) fn tool_schema() -> Value {
    json!({
        "name":"discovery_register",
        "description":"Add one sparse store declaration to an explicitly writable hub catalog. Operator profile and --discovery-catalog-writable ALIAS are required; publishing a read catalog alone does not permit writes. Requires canonical publisher expected_db_id; no caller-selected catalog path, database checkout, inference, or new caller authentication. Released owners retain registration identity; replacement owners cannot inherit the source. Add-only: exact declaration retries return already_present even with a stale expected revision; conflicting identities, labels or content never overwrite existing declarations. Result reports status, publisher_db_id, label, before_catalog_sha256, catalog_sha256, durable and warning. Read discovery_catalog to inspect a conflict before retrying.",
        "inputSchema":{
            "type":"object","additionalProperties":false,
            "properties":{
                "db":{"type":"string","minLength":1,"maxLength":256},
                "expected_db_id":expected_db_id_prop(),
                "label":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_.-]+$"},
                "store":store_schema(),
                "expected_catalog_sha256":{"type":"string","minLength":64,"maxLength":64,"pattern":"^[0-9a-f]{64}$"}
            },
            "required":["db","expected_db_id","label","store"]
        }
    })
}
