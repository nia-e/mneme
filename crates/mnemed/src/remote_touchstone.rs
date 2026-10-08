//! Fail-closed optional touchstone feature negotiation. Old servers sometimes
//! ignore unknown arguments; tool-name presence alone cannot authorize a write.
use serde_json::Value;

pub(crate) fn advertised(catalog: &[Value], name: &str) -> bool {
    catalog.iter().any(|tool| {
        if tool["name"] != name {
            return false;
        }
        let schema = &tool["inputSchema"];
        if name == "list" {
            return list_kind_advertised(schema, "touchstones", 32);
        }
        let touchstone = &schema["properties"]["touchstone"];
        let reference = &touchstone["properties"]["references"]["items"];
        touchstone["type"] == "object"
            && touchstone["additionalProperties"] == false
            && touchstone["properties"]["subject"]["type"] == "string"
            && touchstone["properties"]["references"]["type"] == "array"
            && reference["type"] == "object"
            && reference["additionalProperties"] == false
            && ["db_id", "id", "expected_snapshot_sha256"]
                .iter()
                .all(|&key| {
                    reference["properties"][key]["type"] == "string"
                        && reference["required"]
                            .as_array()
                            .is_some_and(|required| required.iter().any(|value| value == key))
                })
    })
}

pub(crate) fn inventory_advertised(catalog: &[Value]) -> bool {
    catalog.iter().any(|tool| {
        tool["name"] == "list" && list_kind_advertised(&tool["inputSchema"], "nodes", 64)
    })
}

fn list_kind_advertised(schema: &Value, kind: &str, maximum: u64) -> bool {
    let matches = |variant: &Value| {
        variant["properties"]["kind"]["const"] == kind
            && variant["properties"]["after"]["type"] == "string"
            && variant["properties"]["limit"]["maximum"] == maximum
    };
    matches(schema)
        || schema["oneOf"]
            .as_array()
            .is_some_and(|variants| variants.iter().any(matches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_reference_schema_is_required_not_just_tool_or_field_presence() {
        let schema = mneme_app::capture::properties();
        let good = json!({"name":"capture","inputSchema":{"properties":schema}});
        assert!(advertised(&[good.clone()], "capture"));
        let mut wrong = good.clone();
        wrong["inputSchema"]["properties"]["touchstone"]["properties"]["references"]["items"]["properties"]
            ["expected_snapshot_sha256"]["type"] = json!("number");
        assert!(!advertised(&[wrong], "capture"));
        assert!(!advertised(
            &[json!({"name":"capture","inputSchema":{}})],
            "capture"
        ));
        let save = json!({"name":"save","inputSchema":mneme_app::save::input_schema()});
        assert!(advertised(&[save], "save"));
    }

    #[test]
    fn inventory_and_touchstone_discovery_distinguish_old_and_new_owners() {
        let current = json!({"name":"list","inputSchema":mneme_app::list::list_input_schema()});
        assert!(inventory_advertised(std::slice::from_ref(&current)));
        assert!(advertised(&[current], "list"));
        let old = json!({"name":"list","inputSchema":mneme_app::touchstone::list_input_schema()});
        assert!(!inventory_advertised(std::slice::from_ref(&old)));
        assert!(advertised(&[old], "list"));
    }
}
