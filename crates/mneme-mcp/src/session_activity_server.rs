//! Local diagnostic MCP mode. This adapter has no owner registry, configured
//! client, inference runtime, or network path. Its only authority is the two
//! filesystem selectors fixed at startup; tool arguments cannot broaden them.

use std::path::PathBuf;

use mneme_app::session_activity::{self, ActivityRequest};
use serde_json::{Value, json};

use crate::host::AnyErr;

pub(crate) struct ActivityServer {
    reader: session_activity::PreparedActivity,
}

impl ActivityServer {
    pub(crate) fn new(manifest: PathBuf, cwd: PathBuf) -> Result<Self, AnyErr> {
        // Shared admission pins canonical scope, manifest configuration, and the
        // no-follow root handle once. Later edits cannot broaden this server's
        // view; missing or invalid setup remains a status until restart.
        Ok(Self {
            reader: session_activity::PreparedActivity::new(&manifest, &cwd)
                .map_err(|error| format!("session_activity: {}", error.code()))?,
        })
    }

    pub(crate) async fn dispatch(&self, method: &str, params: Value) -> Result<Value, AnyErr> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": crate::negotiated_protocol(&params)?,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "mneme-mcp-session-activity",
                    "version": env!("CARGO_PKG_VERSION"),
                    "capabilityProfile": "local-session-activity",
                },
                "capabilityProfile": "local-session-activity",
                "instructions": "Read-only local session observations, not memory graph activity. Supply the actual host session ID when known. Omit session to get a chooser; no session is inferred as current. Missing observations do not prove nothing happened. Reports do not contact memory owners or cause memory work; ordinary host prompt hooks may still run independently.",
            })),
            "tools/list" => Ok(json!({ "tools": [tool_schema()] })),
            "tools/call" => {
                // Deliberately closed before argument handling or file reads.
                if params.get("name").and_then(Value::as_str) != Some("session_activity") {
                    return Ok(crate::response::tool_error(
                        "unknown local session activity tool",
                    ));
                }
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                let request = match parse_request(&arguments) {
                    Ok(request) => request,
                    Err(error) => return Ok(crate::response::tool_error(error)),
                };
                let report = match self.reader.report(request) {
                    Ok(report) => report,
                    Err(error) => {
                        // Only the shared stable code crosses the MCP boundary;
                        // never echo supplied arguments or filesystem error text.
                        return Ok(crate::response::tool_error(&format!(
                            "session_activity: {}",
                            error.code()
                        )));
                    }
                };
                match serde_json::to_string(&report) {
                    Ok(encoded) if encoded.len() <= session_activity::MAX_OUTPUT_BYTES => {
                        // The shared workflow's compact JSON is also the CLI wire
                        // form. Existing framing wraps it without pretty expansion.
                        Ok(crate::response::tool_success(&Value::String(encoded)))
                    }
                    _ => Ok(crate::response::tool_error(
                        "session_activity: output_limit",
                    )),
                }
            }
            "ping" => Ok(json!({})),
            _ => Err("unknown local session activity MCP method".into()),
        }
    }
}

fn parse_request(arguments: &Value) -> Result<ActivityRequest, &'static str> {
    let object = arguments
        .as_object()
        .ok_or("invalid_arguments: expected an object")?;
    if object
        .keys()
        .any(|name| !matches!(name.as_str(), "session" | "details"))
    {
        return Err("invalid_arguments: only session and details are accepted");
    }
    let session = match object.get("session") {
        None => None,
        Some(Value::String(value))
            if !value.is_empty()
                && value.len() <= session_activity::MAX_SESSION_SELECTOR_BYTES
                && !value.chars().any(char::is_control) =>
        {
            Some(value.clone())
        }
        _ => return Err("invalid_arguments: session must be a nonempty bounded string"),
    };
    let details = match object.get("details") {
        None => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err("invalid_arguments: details must be a boolean"),
    };
    Ok(ActivityRequest { session, details })
}

fn tool_schema() -> Value {
    json!({
        "name": "session_activity",
        "description": "Report retained, body-free Codex session observations in the startup-bound local workspace. Omit session for a chooser with IDs and timestamps; never assumes the latest session is current. Coverage states distinguish missing or incomplete evidence from no activity. Does not contact memory owners.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "session": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": session_activity::MAX_SESSION_SELECTOR_BYTES,
                    "description": "Stored full host session ID, codex:<id>, or a uniquely matching prefix when catalog coverage is complete. At most 289 UTF-8 bytes.",
                },
                "details": {
                    "type": "boolean",
                    "default": false,
                    "description": "Include bounded event times, reasons, provenance, and scoped node IDs; never note bodies or prompts.",
                },
            },
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_arguments_are_complete_typed_and_scope_closed() {
        let request = parse_request(&json!({})).unwrap();
        assert!(request.session.is_none());
        assert!(!request.details);
        let request =
            parse_request(&json!({"session":"codex:actual-host-id", "details":true})).unwrap();
        assert_eq!(request.session.as_deref(), Some("codex:actual-host-id"));
        assert!(request.details);
        for invalid in [
            Value::Null,
            json!([]),
            json!({"session":null}),
            json!({"session":7}),
            json!({"session":""}),
            json!({"session":"line\nbreak"}),
            json!({"session":"x".repeat(session_activity::MAX_SESSION_SELECTOR_BYTES + 1)}),
            json!({"details":null}),
            json!({"details":1}),
            json!({"details":"true"}),
            json!({"cwd":"/another-workspace"}),
            json!({"scope":"global"}),
            json!({"activity_config":"/another-manifest"}),
            json!({"db":"user"}),
            json!({"root":"/another-root"}),
            json!({"path":"/another-path"}),
        ] {
            assert!(parse_request(&invalid).is_err(), "admitted {invalid}");
        }
    }

    #[tokio::test]
    async fn activity_scope_and_missing_setup_are_pinned_until_restart() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mneme-activity-mcp-pinned-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let workspace = root.join("workspace");
        let activity_one = root.join("activity-one");
        let activity_two = root.join("activity-two");
        for path in [&workspace, &activity_one, &activity_two] {
            std::fs::create_dir(path).unwrap();
        }
        let manifest = root.join("activity.json");
        let initially_missing = ActivityServer::new(manifest.clone(), workspace.clone()).unwrap();
        let write_manifest = |activity: &std::path::Path, scope: &std::path::Path| {
            std::fs::write(
                &manifest,
                serde_json::to_vec(&json!({
                    "schema":"mneme.session-activity.config.v1", "root":activity,
                    "workspaces":[{"path":scope,"source":"codex"}], "device":null
                }))
                .unwrap(),
            )
            .unwrap();
        };
        write_manifest(&activity_one, &workspace);
        let configured = ActivityServer::new(manifest.clone(), workspace.clone()).unwrap();
        let params = json!({"name":"session_activity"});
        let before = configured
            .dispatch("tools/call", params.clone())
            .await
            .unwrap();
        assert_ne!(before["isError"], true);
        let before_report: Value =
            serde_json::from_str(before["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_ne!(before_report["status"], "not_configured");
        // Replace the manifest with a broader admitted ancestor and another root.
        write_manifest(&activity_two, &root);
        let after = configured
            .dispatch("tools/call", params.clone())
            .await
            .unwrap();
        assert_eq!(
            after, before,
            "a running MCP must not reload a broader scope"
        );
        let missing = initially_missing
            .dispatch("tools/call", params.clone())
            .await
            .unwrap();
        let missing_report: Value =
            serde_json::from_str(missing["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(missing_report["status"], "not_configured");
        let restarted = ActivityServer::new(manifest, workspace).unwrap();
        assert_ne!(
            restarted.dispatch("tools/call", params).await.unwrap(),
            before
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn activity_catalog_is_closed_and_missing_setup_is_reported_without_writes() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mneme-activity-mcp-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let manifest = root.join("not-configured.json");
        let server = ActivityServer::new(manifest.clone(), root.clone()).unwrap();
        let initialized = server
            .dispatch(
                "initialize",
                json!({"protocolVersion":crate::PROTOCOL_VERSION}),
            )
            .await
            .unwrap();
        assert_eq!(initialized["capabilityProfile"], "local-session-activity");
        let catalog = server.dispatch("tools/list", json!({})).await.unwrap();
        assert_eq!(catalog["tools"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["tools"][0]["name"], "session_activity");
        assert_eq!(
            catalog["tools"][0]["inputSchema"]["additionalProperties"],
            false
        );
        for tool in [
            "activity",
            "query",
            "save",
            "library_query",
            "config_select",
        ] {
            let response = server
                .dispatch("tools/call", json!({"name":tool}))
                .await
                .unwrap();
            assert_eq!(response["isError"], true, "{tool}");
        }
        let response = server
            .dispatch("tools/call", json!({"name":"session_activity"}))
            .await
            .unwrap();
        assert_ne!(response["isError"], true);
        let text = response["content"][0]["text"].as_str().unwrap();
        let report: Value = serde_json::from_str(text).unwrap();
        assert_eq!(report["status"], "not_configured");
        assert!(text.len() <= session_activity::MAX_OUTPUT_BYTES);
        assert!(!manifest.exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert_eq!(server.dispatch("ping", json!({})).await.unwrap(), json!({}));
        std::fs::remove_dir(&root).unwrap();
    }
}
