//! Read-only MCP coordinator mode. This module deliberately has no owner
//! registry, database checkout, inference runtime, or mutation capability.

use std::path::Path;

use mneme_library::{LibraryRuntime, tool_definitions};
use serde_json::{Value, json};

use crate::host::AnyErr;

pub struct LibraryServer {
    runtime: LibraryRuntime,
}

impl LibraryServer {
    pub fn from_path(path: &Path) -> Result<Self, AnyErr> {
        Ok(Self {
            runtime: LibraryRuntime::from_path(path)?,
        })
    }

    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, AnyErr> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": crate::negotiated_protocol(&params)?,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "mneme-mcp-library",
                    "version": env!("CARGO_PKG_VERSION"),
                    "capabilityProfile": "library-read-only",
                },
                "capabilityProfile": "library-read-only",
                "instructions": format!("Mneme library. Profile: library-read-only. {}", mneme_app::episode::MEMORY_TIME_GUIDANCE),
            })),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                // Closed-world name check before runtime dispatch: future owner
                // names cannot become reachable by a generic pass-through.
                if !matches!(
                    name,
                    "library_catalog" | "library_query" | "library_recall_context" | "library_get"
                ) {
                    return Ok(crate::response::tool_error(&format!(
                        "unknown library tool {name:?}"
                    )));
                }
                if !args.is_object() {
                    return Ok(crate::response::tool_error(
                        "library tool arguments must be an object",
                    ));
                }
                let operation = self.runtime.dispatch(name, args);
                fn assert_send<T: Send>(_: &T) {}
                assert_send(&operation);
                match operation.await {
                    Ok(value) => Ok(crate::response::tool_success(&value)),
                    Err(error) => Ok(crate::response::tool_error(&error.to_string())),
                }
            }
            "ping" => Ok(json!({})),
            other => Err(format!("unknown MCP method {other:?}").into()),
        }
    }
}
