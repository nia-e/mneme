//! Guarded native cabinet reads through the existing selected owner.
use super::*;

const UNAVAILABLE: &str = "selected owner does not advertise guarded native touchstone reads; return to Map or update this owner";
const SMALLER_PAGE: &str = "for a smaller explicit read, use mnemed --remote URL list --touchstones --limit 2 against this owner (add --user for the user database)";

pub(super) async fn page(
    connection: &mut RemoteClient,
    target: &Target,
    identity: &str,
    after: &Option<String>,
) -> Result<Response, Error> {
    if !native_touchstone_catalog(connection) {
        return Err(UNAVAILABLE.into());
    }
    let mut arguments = json!({"db":target.database,"expected_db_id":identity,
    "kind":"touchstones","limit":touchstones::PAGE_ITEMS});
    if let Some(after) = after {
        arguments["after"] = json!(after);
    }
    let page = connection
        .call_tool("list", arguments)
        .await
        .map_err(|error| format!("{error}; {SMALLER_PAGE}"))?;
    Ok(Response::Touchstones(touchstones::parse_page(
        &page, identity,
    )?))
}

pub(super) async fn annotation(
    connection: &mut RemoteClient,
    target: &Target,
    identity: &str,
    id: &str,
) -> Result<Response, Error> {
    if !native_touchstone_catalog(connection) {
        return Err(UNAVAILABLE.into());
    }
    let value = connection
        .call_tool(
            "get",
            json!({"db":target.database,
    "expected_db_id":identity,"id":id,"body":true,"max_body_bytes":BODY_BYTES}),
        )
        .await?;
    Ok(Response::Touchstone(touchstones::parse_detail(
        &value, identity, id,
    )?))
}

pub(super) async fn exact_target(
    connection: &mut RemoteClient,
    target: &Target,
    identity: &str,
    db_id: &str,
    id: &str,
) -> Result<Response, Error> {
    // Exact stored citation only: never look up its episode root or successor,
    // and never fall back to a different database or owner.
    if db_id != identity {
        return Err(
            "cited database is unavailable through this selected owner; no cross-store fallback"
                .into(),
        );
    }
    if !native_touchstone_catalog(connection) {
        return Err(UNAVAILABLE.into());
    }
    let value = connection
        .call_tool(
            "get",
            json!({"db":target.database,
    "expected_db_id":identity,"id":id,"body":true,"max_body_bytes":BODY_BYTES}),
        )
        .await;
    let target = match value {
        Ok(value) => {
            let found = touchstone_node(&value)?;
            if found.id != id
                || value["summary_snapshot"]["db_id"] != identity
                || value["summary_snapshot"]["id"] != id
            {
                return Err("cited target returned a different database or edition".into());
            }
            touchstones::ExactTarget {
                db_id: identity.to_owned(),
                node: Some(found),
                summary_partial: value["summary_truncated"] == true,
                body_partial: value["body_range"]["has_more"] == true
                    || value["body"]
                        .as_str()
                        .is_some_and(|body| body.len() > BODY_BYTES),
            }
        }
        Err(error) if error.to_string() == "remote MCP tool failed: node not found" => {
            touchstones::ExactTarget {
                db_id: identity.to_owned(),
                node: None,
                summary_partial: false,
                body_partial: false,
            }
        }
        Err(error) => return Err(error),
    };
    Ok(Response::TouchstoneTarget(target))
}

/// Names alone do not establish support: older owners may ignore unknown fields.
fn native_touchstone_catalog(connection: &RemoteClient) -> bool {
    connection.supports_expected_db_id("list", None)
        && connection.supports_expected_db_id("get", None)
        && connection.tool_catalog().iter().any(|tool| {
            tool["name"] == "list" && native_touchstone_list_schema(&tool["inputSchema"])
        })
}

fn native_touchstone_list_schema(schema: &Value) -> bool {
    let native_branch = |branch: &Value| {
        branch["additionalProperties"] == false
            && branch["properties"]["kind"]["const"] == "touchstones"
            && branch["required"]
                .as_array()
                .is_some_and(|keys| keys.contains(&json!("kind")))
            && branch["properties"]["limit"]["maximum"]
                .as_u64()
                .is_some_and(|n| n >= touchstones::PAGE_ITEMS as u64)
            && branch["properties"]["after"]["maxLength"] == 1024
    };
    // The owner may expose either the original touchstones-only schema or a
    // closed nodes/touchstones union. `db` need not be schema-required: every
    // cabinet request still sends its selected db and canonical identity guard.
    native_branch(schema)
        || schema["oneOf"]
            .as_array()
            .is_some_and(|branches| branches.iter().any(native_branch))
}
