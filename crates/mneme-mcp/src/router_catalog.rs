//! One tool entry per operation, with equivalent DB contracts grouped together.
use crate::{CapabilityPolicy, CapabilityProfile, host::AnyErr};
use serde_json::{Value, json};

pub(super) const TOOLS: &[&str] = &[
    "core",
    "recall_context",
    "get",
    "list",
    "neighbors",
    "save",
    "retag",
    "edit_body",
    "edit_summary",
    "episode",
    "link",
    "supersede",
    "forget",
    "status",
];
pub(super) struct RouteCatalog<'a> {
    pub name: &'a str,
    pub replica: bool,
    pub tools: &'a [Value],
}

pub(super) fn ordinary_catalog(
    client: &mneme_mcp_client::RemoteClient,
    policy: CapabilityPolicy,
) -> Result<Vec<Value>, AnyErr> {
    let admitted = crate::tool_schemas(policy);
    let mut tools = Vec::new();
    for native in client.tool_catalog() {
        let name = native["name"]
            .as_str()
            .ok_or("upstream tool missing name")?;
        if !TOOLS.contains(&name) || !admitted.iter().any(|tool| tool["name"] == name) {
            continue;
        }
        if name == "edit_body" && !client.supports_edit_body() {
            continue;
        }
        if name == "edit_summary" && !client.supports_edit_summary() {
            continue;
        }
        if name == "retag" && !client.supports_retag() {
            continue;
        }
        // Do not publish a stronger guard contract than the checked client can
        // forward. A genuinely old tag-only retag remains available unchanged.
        if name == "retag"
            && ["expected_content_fingerprint", "guard_nodes"]
                .iter()
                .any(|field| native["inputSchema"]["properties"].get(field).is_some())
            && !client.supports_retag_content_guards()
        {
            continue;
        }
        if name == "list"
            && schema_mentions_kind(&native["inputSchema"], "tags")
            && !client.supports_list_tags()
        {
            continue;
        }
        let mut tool = native.clone();
        if name == "episode" && policy.profile == CapabilityProfile::ReadOnly {
            let allowed = &mneme_app::episode::EpisodeAction::READ_ONLY;
            let allowed: Vec<_> = allowed.iter().map(|action| action.as_str()).collect();
            tool["inputSchema"]["properties"]["action"]["enum"]
                .as_array_mut()
                .ok_or("episode catalog missing actions")?
                .retain(|action| {
                    action
                        .as_str()
                        .is_some_and(|action| allowed.contains(&action))
                });
            tool["inputSchema"]["oneOf"]
                .as_array_mut()
                .ok_or("episode catalog missing branches")?
                .retain(|branch| {
                    branch["properties"]["action"]["const"]
                        .as_str()
                        .is_some_and(|action| allowed.contains(&action))
                });
        }
        let guard_supported = match name {
            "episode" => tool
                .pointer("/inputSchema/properties/action/enum")
                .and_then(Value::as_array)
                .is_some_and(|actions| {
                    actions.iter().all(|action| {
                        action.as_str().is_some_and(|action| {
                            client.supports_expected_db_id(name, Some(action))
                        })
                    })
                }),
            "save" => ["note", "episode"]
                .iter()
                .filter(|kind| client.supports_save_kind(kind))
                .all(|kind| client.supports_expected_db_id(name, Some(kind))),
            _ => client.supports_expected_db_id(name, None),
        };
        if !guard_supported {
            return Err("upstream ordinary tool lacks canonical database identity guards".into());
        }
        // Validate the known routing envelope even before catalog publication.
        normalize_schema(&tool["inputSchema"], name, false)?;
        tools.push(tool);
    }
    if tools.is_empty() {
        return Err("owner advertises no supported ordinary tools".into());
    }
    Ok(tools)
}

fn schema_mentions_kind(schema: &Value, kind: &str) -> bool {
    schema["properties"]["kind"]["const"] == kind
        || schema["properties"]["kind"]["enum"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|value| value == kind))
        || ["oneOf", "anyOf", "allOf"].iter().any(|union| {
            schema[union].as_array().is_some_and(|branches| {
                branches
                    .iter()
                    .any(|branch| schema_mentions_kind(branch, kind))
            })
        })
}

pub(super) fn support(tools: &[Value]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                let mut item = json!({"name":tool["name"]});
                if let Some(actions) = tool.pointer("/inputSchema/properties/action/enum") {
                    item["actions"] = actions.clone();
                }
                if let Some(kinds) = tool.pointer("/inputSchema/properties/kind/enum") {
                    if matches!(tool["name"].as_str(), Some("save" | "list")) {
                        item["kinds"] = kinds.clone();
                    }
                }
                item
            })
            .collect(),
    )
}

pub(super) fn advertises_action(tools: &[Value], name: &str, args: &Value) -> bool {
    let Some(tool) = tools.iter().find(|tool| tool["name"] == name) else {
        return false;
    };
    if name == "episode" {
        return args["action"].as_str().is_some_and(|action| {
            tool.pointer("/inputSchema/properties/action/enum")
                .and_then(Value::as_array)
                .is_some_and(|actions| actions.iter().any(|value| value == action))
        });
    }
    true
}

pub(super) fn build(routes: &[RouteCatalog<'_>], default: &str) -> Result<Vec<Value>, AnyErr> {
    let mut tools = vec![
        json!({"name":"databases","description":"List only this router's configured logical databases, read default, native identity, read-only status, tool/action support and current snapshot metadata. No upstream paths or unselected owners.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}),
    ];
    for name in TOOLS {
        let mut groups: Vec<(Value, Vec<String>)> = Vec::new();
        let mut template = None;
        let mut descriptions = Vec::new();
        for route in routes {
            if matches!(*name, "edit_body" | "edit_summary") && route.replica {
                continue;
            }
            let Some(native) = route.tools.iter().find(|tool| tool["name"] == *name) else {
                continue;
            };
            if template.is_none() || route.name == default {
                template = Some(native.clone());
            }
            let schema = normalize_schema(&native["inputSchema"], name, route.replica)?;
            if let Some((_, aliases)) = groups.iter_mut().find(|(other, _)| other == &schema) {
                aliases.push(route.name.into());
            } else {
                groups.push((schema, vec![route.name.into()]));
            }
            let actions = native
                .pointer("/inputSchema/properties/action/enum")
                .map(|actions| format!(" actions {actions}"))
                .unwrap_or_default();
            descriptions.push(format!(
                "{}{}{}",
                route.name,
                if route.replica {
                    " (read-only snapshot)"
                } else {
                    ""
                },
                actions
            ));
        }
        let Some(mut tool) = template else {
            continue;
        };
        let mut variants = Vec::new();
        let mut aliases = Vec::new();
        for (mut schema, names) in groups {
            set_aliases(&mut schema, name, &names)?;
            if !names.iter().any(|name| name == default) {
                require_db(&mut schema)?;
            }
            aliases.extend(names);
            variants.push(schema);
        }
        tool["inputSchema"] = if variants.len() == 1 {
            variants.remove(0)
        } else {
            json!({"type":"object","properties":{"db":{"type":"string","enum":aliases}},"oneOf":variants})
        };
        let native_description = if *name == "get" {
            "Fetch one selected-database record with its canonical summary (up to 16384 UTF-8 bytes on current owners; inspect summary_truncated on older owners) and optional bounded body page. Hosted edges:true is refused; use neighbors for local edges. Replica body continuations require the returned snapshot."
        } else if *name == "link" {
            "Assert a local edge from -> to in the selected database; optional kind, weight and paired byte-range anchor. Cross-database links are not exposed."
        } else {
            tool["description"].as_str().unwrap_or("")
        };
        tool["description"] = json!(format!(
            "Select db; omitted reads use {default:?}, writes require explicit db. Support: {}. Replica continuations require the returned snapshot. Routing guidance here overrides native owner-selection text. {native_description}",
            descriptions.join("; ")
        ));
        if let Some(output) = tool.get_mut("outputSchema") {
            *output = json!({"type":"object","properties":{"db":{"type":"string"},"snapshot":snapshot_schema(),"result":output.clone()},"required":["db","result"]});
        }
        tools.push(tool);
    }
    Ok(tools)
}

fn snapshot_schema() -> Value {
    json!({"type":"object","properties":{"source_device_id":{"type":"string","minLength":1,"maxLength":256},"generation":{"type":"string","minLength":1,"maxLength":256},"captured_at":{"type":"integer","minimum":0}},"required":["source_device_id","generation"],"additionalProperties":false})
}
fn routing_envelope(schema: &mut Value, replica: bool) -> Result<(), AnyErr> {
    if schema["type"] != "object" {
        return Err("native tool routing schema must be an object".into());
    }
    let props = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .ok_or("native routing schema missing properties")?;
    for field in ["db", "expected_db_id"] {
        if props
            .get(field)
            .and_then(|prop| prop.get("type"))
            .and_then(Value::as_str)
            != Some("string")
        {
            return Err("native tool lacks a checked database routing envelope".into());
        }
    }
    props.insert("db".into(),json!({"type":"string","description":"Logical database from this router's databases tool; reads default, mutations require explicit db."}));
    props.insert("expected_db_id".into(), crate::expected_db_id_prop());
    if replica {
        props.insert("snapshot".into(), snapshot_schema());
    }
    Ok(())
}
fn normalize_schema(native: &Value, name: &str, replica: bool) -> Result<Value, AnyErr> {
    let mut schema = native.clone();
    routing_envelope(&mut schema, replica)?;
    if name == "episode" && !schema["oneOf"].is_array() {
        return Err("native episode missing branches".into());
    }
    if matches!(name, "episode" | "list")
        && let Some(branches) = schema.get_mut("oneOf")
    {
        for branch in branches
            .as_array_mut()
            .ok_or("native action branches must be an array")?
        {
            routing_envelope(branch, replica)?;
        }
    }
    if name == "get" {
        schema["properties"]["edges"]["const"] = json!(false);
    }
    if name == "link" {
        schema["properties"]
            .as_object_mut()
            .ok_or("native link missing properties")?
            .remove("to_db");
        let branches = schema["oneOf"]
            .as_array()
            .ok_or("native link missing variants")?;
        let local: Vec<_> = branches
            .iter()
            .filter(|branch| {
                branch.pointer("/allOf/0") == Some(&crate::forbid_arguments(&["to_db"]))
            })
            .cloned()
            .collect();
        if branches.len() != 2 || local.len() != 1 {
            return Err("unsupported native link variants".into());
        }
        let mut local = local[0].clone();
        local["allOf"]
            .as_array_mut()
            .ok_or("invalid local link contract")?
            .remove(0);
        schema["oneOf"] = json!([local]);
    }
    if replica && matches!(name, "list" | "neighbors" | "episode" | "get") {
        let rule = json!({"if":{"anyOf":[{"required":["after"]},{"required":["cursor"]},{"required":["body_offset"],"properties":{"body_offset":{"minimum":1}}},{"required":["offset"],"properties":{"offset":{"minimum":1}}}]},"then":{"required":["snapshot"]}});
        let object = schema
            .as_object_mut()
            .ok_or("native schema must be object")?;
        object
            .entry("allOf")
            .or_insert(json!([]))
            .as_array_mut()
            .ok_or("native allOf must be array")?
            .push(rule);
    }
    Ok(schema)
}
fn set_aliases(schema: &mut Value, name: &str, aliases: &[String]) -> Result<(), AnyErr> {
    schema["properties"]["db"]["enum"] = json!(aliases);
    if matches!(name, "episode" | "list")
        && let Some(branches) = schema.get_mut("oneOf")
    {
        for branch in branches
            .as_array_mut()
            .ok_or("native action branches must be an array")?
        {
            branch["properties"]["db"]["enum"] = json!(aliases);
        }
    }
    Ok(())
}
fn require_db(schema: &mut Value) -> Result<(), AnyErr> {
    let required = schema
        .as_object_mut()
        .ok_or("native schema must be object")?
        .entry("required")
        .or_insert(json!([]))
        .as_array_mut()
        .ok_or("native required must be array")?;
    if !required.iter().any(|field| field == "db") {
        required.push(json!("db"));
    }
    Ok(())
}
