//! Declarative coordinator: discovery metadata is frozen at startup; memory contacts are lazy.
use crate::{host::AnyErr, router_catalog, router_response};
use mneme_store_client::{ConfiguredClient, OperationBudget, RequestAccess, request_access};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub struct ConfiguredServer {
    client: ConfiguredClient,
}
impl ConfiguredServer {
    pub async fn from_path(path: &Path, cwd: &Path) -> Result<Self, AnyErr> {
        Ok(Self {
            client: ConfiguredClient::new(
                mneme_store_client::discovery::select_workspace(
                    path,
                    cwd,
                    None,
                    mneme_store_client::discovery::ResolveOptions::default(),
                    &OperationBudget::new(1024 * 1024, Duration::from_secs(15))?,
                )
                .await?,
            )?,
        })
    }
    pub fn dispatch<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, AnyErr>> + Send + 'a>>
    {
        Box::pin(self.dispatch_inner(method, params))
    }
    async fn dispatch_inner(&self, method: &str, params: Value) -> Result<Value, AnyErr> {
        self.client.selection().revalidate_exact()?;
        if !params.is_object()
            || serde_json::to_vec(&params)?.len() > crate::MAX_STDIO_REQUEST_BYTES
        {
            return Err("configured MCP params exceed typed request envelope".into());
        }
        match method {
            "initialize" => Ok(
                json!({"protocolVersion":crate::negotiated_protocol(&params)?,"capabilities":{"tools":{}},"serverInfo":{"name":"mneme-mcp-router","version":env!("CARGO_PKG_VERSION"),"capabilityProfile":"configured-databases"},"capabilityProfile":"configured-databases","selectionDigest":self.client.selection().digest,"instructions":"Configured Mneme stores. db selects an admitted logical store. Reads use the metadata-selected default; writes require explicit db. Owner outage is not absence. Retain _mneme_source unchanged for cached continuations; caches never authorize writes."}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let mut budget = OperationBudget::new(2 * 1024 * 1024, Duration::from_secs(15))?;
                let mut contracts = Vec::new();
                let mut omissions = Vec::new();
                for selected in &self.client.selection().active {
                    if !selected.eligible.read && !selected.eligible.write {
                        continue;
                    }
                    match self.client.catalog_with_budget(&selected.name,&mut budget).await{Ok(tools)=>contracts.push((selected.name.clone(),tools)),Err(_)=>omissions.push(json!({"db":selected.name,"reason":"owner/catalog unavailable or refused; other selected stores remain usable"}))}
                }
                let routes: Vec<_> = contracts
                    .iter()
                    .map(|(name, tools)| router_catalog::RouteCatalog {
                        name,
                        replica: false,
                        tools,
                    })
                    .collect();
                let default = self
                    .client
                    .selection()
                    .read_default
                    .as_deref()
                    .unwrap_or("");
                let mut tools = router_catalog::build(&routes, default)?;
                if !self.client.selection().libraries.is_empty() {
                    let names: Vec<_> = self.client.selection().libraries.keys().cloned().collect();
                    for mut tool in mneme_library::tool_definitions()
                        .as_array()
                        .ok_or("invalid library tool definitions")?
                        .iter()
                        .cloned()
                    {
                        add_library_selector(&mut tool["inputSchema"], &names)?;
                        tools.push(tool);
                    }
                }
                tools.push(json!({"name":"config_select","description":"Return the bounded frozen native selection and digest. Metadata-only; no owner contact or activation changes.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}));
                for tool in &mut tools {
                    if tool["name"]
                        .as_str()
                        .is_some_and(|name| router_catalog::TOOLS.contains(&name))
                    {
                        add_source(&mut tool["inputSchema"]);
                    }
                }
                Ok(
                    json!({"tools":tools,"omissions":omissions,"selectionDigest":self.client.selection().digest}),
                )
            }
            "tools/call" => {
                let tool = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("tool name must be a string")?;
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                if matches!(
                    tool,
                    "library_catalog" | "library_query" | "library_recall_context" | "library_get"
                ) {
                    let mut args = args;
                    let Some(object) = args.as_object_mut() else {
                        return Ok(crate::response::tool_error(
                            "library arguments must be an object",
                        ));
                    };
                    let library = match object.remove("library") {
                        Some(Value::String(name)) => name,
                        None if self.client.selection().libraries.len() == 1 => self
                            .client
                            .selection()
                            .libraries
                            .keys()
                            .next()
                            .unwrap()
                            .clone(),
                        _ => {
                            return Ok(crate::response::tool_error(
                                "library must name one admitted configured collection",
                            ));
                        }
                    };
                    return Ok(match self.client.library(&library) {
                        Ok(runtime) => match runtime.dispatch(tool, args).await {
                            Ok(result) => crate::response::tool_success(&result),
                            Err(error) => crate::response::tool_error(&error.to_string()),
                        },
                        Err(error) => crate::response::tool_error(&error.to_string()),
                    });
                }
                if matches!(tool, "config_select" | "databases") {
                    return Ok(if args != json!({}) {
                        crate::response::tool_error("metadata tool accepts no arguments")
                    } else {
                        crate::response::tool_success(&if tool == "config_select" {
                            json!(self.client.selection())
                        } else {
                            self.client.databases()
                        })
                    });
                }
                let Some(object) = args.as_object() else {
                    return Ok(crate::response::tool_error(
                        "tool arguments must be an object",
                    ));
                };
                let alias = match object.get("db") {
                    Some(Value::String(name)) => name.as_str(),
                    None => self
                        .client
                        .selection()
                        .read_default
                        .as_deref()
                        .unwrap_or(""),
                    _ => {
                        return Ok(crate::response::tool_error(
                            "db must be a logical store string",
                        ));
                    }
                };
                let outcome:Result<Value,AnyErr>=async{
                    if !router_catalog::TOOLS.contains(&tool){return Err("unknown configured MCP tool; use tools/list for the native ordinary-work surface".into());}
                    let selected=self.client.selected(alias)?;
                    let mut native=args.clone();native.as_object_mut().unwrap().remove("_mneme_source");
                    native["db"]=json!(alias);native["expected_db_id"]=json!(selected.store.id);
                    let prepared=crate::ValidatedToolArguments::parse(tool,&native)?;
                    if prepared.kind().requires_explicit_db() && !object.contains_key("db"){return Err("mutations require explicit logical db; no operation was sent".into());}
                    // Complete native typed admission precedes owner/replica contacts.
                    if request_access(tool,&native)?==RequestAccess::Write && !selected.eligible.write{return Err("configured write grant denied; no owner was contacted".into());}
                    let mut forwarded=args.clone();forwarded["db"]=json!(alias);
                    Box::pin(self.client.call(alias,tool,forwarded)).await
                }.await;
                Ok(match outcome {
                    Ok(result) => {
                        let wrapped = router_response::envelope(
                            alias,
                            result.get("_mneme_source"),
                            result.clone(),
                        );
                        if tool == "core" {
                            crate::response::core_tool_success(&wrapped)
                        } else {
                            crate::response::tool_success(&wrapped)
                        }
                    }
                    Err(error) => router_response::refusal(alias, None, &error.to_string()),
                })
            }
            _ => Err("unknown configured MCP method".into()),
        }
    }
    pub async fn close(&self) {
        self.client.close().await;
    }
}
fn add_library_selector(schema: &mut Value, names: &[String]) -> Result<(), AnyErr> {
    schema["properties"]["library"] = json!({"type":"string","enum":names,"description":"Admitted configured collection; required unless exactly one collection is admitted."});
    if names.len() != 1 {
        // JSON Schema permits an absent required list (e.g. library_catalog).
        // Only absence means empty; a present malformed value must still fail.
        if schema.get("required").is_none() {
            schema["required"] = json!([]);
        }
        schema["required"]
            .as_array_mut()
            .ok_or("library schema required list invalid")?
            .push(json!("library"));
    }
    Ok(())
}

fn add_source(schema: &mut Value) {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert("_mneme_source".into(),json!({"type":"object","description":"Exact cached reference returned by the first read; reuse unchanged for continuation.","properties":{"owner_db_id":{"type":"string"},"source_device_id":{"type":"string"},"generation":{"type":"string"},"captured_at":{"type":"integer","minimum":0},"database":{"type":"string"},"resolved_path":{"type":"string"}},"required":["owner_db_id","source_device_id","generation","captured_at","database","resolved_path"],"additionalProperties":false}));
    }
    for union in ["oneOf", "anyOf", "allOf"] {
        if let Some(branches) = schema.get_mut(union).and_then(Value::as_array_mut) {
            for branch in branches {
                add_source(branch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn library_selector_defaults_only_an_absent_required_list() {
        let names = vec!["offline".into(), "personal".into()];
        for required in [None, Some(json!([])), Some(json!(["text"]))] {
            let mut schema = json!({"type":"object","additionalProperties":false});
            if let Some(required) = &required {
                schema["required"] = required.clone();
            }
            add_library_selector(&mut schema, &names).unwrap();
            let mut expected = required
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            expected.push(json!("library"));
            assert_eq!(schema["required"], json!(expected));
        }
        for malformed in [
            Value::Null,
            json!("text"),
            json!({}),
            json!(false),
            json!(1),
        ] {
            let mut schema = json!({"type":"object","required":malformed});
            assert_eq!(
                add_library_selector(&mut schema, &names)
                    .unwrap_err()
                    .to_string(),
                "library schema required list invalid"
            );
            assert_eq!(schema["required"], malformed);
        }
        let mut schema = json!({"type":"object","additionalProperties":false});
        add_library_selector(&mut schema, &names[..1]).unwrap();
        assert!(schema.get("required").is_none());
    }

    #[tokio::test]
    async fn configured_library_catalog_and_dispatch_agree_for_zero_one_and_two_collections() {
        use crate::tests::{schema_accepts, tool_input_schema};

        let definitions = mneme_library::tool_definitions();
        let definitions = definitions.as_array().unwrap();
        let requests = [
            ("library_catalog", json!({})),
            ("library_query", json!({"text":"needle"})),
            ("library_recall_context", json!({"text":"needle"})),
            (
                "library_get",
                json!({"project_id":"missing","id":"node","source_device_id":"owner"}),
            ),
        ];
        let mut metadata_tools = None;
        for names in [vec![], vec!["personal"], vec!["offline", "personal"]] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let scratch = Scratch(
                std::env::temp_dir()
                    .canonicalize()
                    .unwrap()
                    .join(format!("mneme-configured-libraries-{}", ulid::Ulid::new())),
            );
            std::fs::create_dir(&scratch.0).unwrap();
            let path = scratch.0.join("memory.toml");
            let mut config = format!(
                "schema='mneme.config.v1'\n[stores.fixture]\nid='01M3CPEVDTM9PP4HJTMMBZ8HR9'\nowner={{url='http://{}',db='project'}}\nactive={{workspace='any'}}\npermissions={{read=false,write=false}}\n",
                listener.local_addr().unwrap()
            );
            for name in &names {
                config.push_str(&format!(
                    "[libraries.{name}]\ncatalog='{}'\n",
                    scratch.0.join(format!("{name}.json")).display()
                ));
            }
            std::fs::write(&path, config).unwrap();
            let server = ConfiguredServer::from_path(&path, &scratch.0)
                .await
                .unwrap();

            // Listing depends on admitted metadata, not readable owners or the
            // existence of any library catalog file.
            let listed = server.dispatch("tools/list", json!({})).await.unwrap();
            assert_eq!(listed["omissions"], json!([]));
            let tools = listed["tools"].as_array().unwrap();
            assert_eq!(tools.len(), 2 + if names.is_empty() { 0 } else { 4 });
            let metadata: Vec<_> = tools
                .iter()
                .filter(|tool| matches!(tool["name"].as_str(), Some("databases" | "config_select")))
                .cloned()
                .collect();
            assert_eq!(metadata.len(), 2);
            if let Some(baseline) = &metadata_tools {
                assert_eq!(&metadata, baseline);
            } else {
                metadata_tools = Some(metadata);
            }
            for name in ["databases", "config_select"] {
                assert!(schema_accepts(tool_input_schema(tools, name), &json!({})));
                let response = server
                    .dispatch("tools/call", json!({"name":name,"arguments":{}}))
                    .await
                    .unwrap();
                assert_eq!(response["isError"], false);
            }
            for (name, args) in &requests {
                if !names.is_empty() {
                    let schema = tool_input_schema(tools, name);
                    let native = tool_input_schema(definitions, name);
                    let mut required = native
                        .get("required")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if names.len() > 1 {
                        required.push(json!("library"));
                    }
                    assert_eq!(
                        schema.get("required").cloned().unwrap_or(json!([])),
                        json!(required)
                    );
                    assert_eq!(schema["properties"]["library"]["enum"], json!(names));
                    assert_eq!(schema_accepts(schema, args), names.len() == 1);
                    for selected in &names {
                        let mut explicit = args.clone();
                        explicit["library"] = json!(selected);
                        assert!(schema_accepts(schema, &explicit));
                        for field in native["required"].as_array().into_iter().flatten() {
                            let mut incomplete = explicit.clone();
                            incomplete
                                .as_object_mut()
                                .unwrap()
                                .remove(field.as_str().unwrap());
                            assert!(!schema_accepts(schema, &incomplete));
                        }
                    }
                }
                for selector in [
                    None,
                    Some(Value::Null),
                    Some(json!(7)),
                    Some(json!("unknown")),
                ] {
                    if selector.is_none() && names.len() == 1 {
                        continue;
                    }
                    let mut invalid = args.clone();
                    if let Some(selector) = selector {
                        invalid["library"] = selector;
                    }
                    if !names.is_empty() {
                        assert!(!schema_accepts(tool_input_schema(tools, name), &invalid));
                    }
                    let refused = server
                        .dispatch("tools/call", json!({"name":name,"arguments":invalid}))
                        .await
                        .unwrap();
                    assert_eq!(refused["isError"], true);
                    assert!(
                        refused["content"][0]["text"]
                            .as_str()
                            .unwrap()
                            .contains("library")
                    );
                }
            }
            for name in &names {
                let catalog = scratch.0.join(format!("{name}.json"));
                assert!(!catalog.exists());
                std::fs::write(
                    catalog,
                    serde_json::to_vec(&json!({"schema":"mneme.library.catalog.v1","library_id":name,"revision":1,"entries":[]})).unwrap(),
                )
                .unwrap();
                for (tool, args) in &requests {
                    for defaulted in [false, true] {
                        if defaulted && names.len() != 1 {
                            continue;
                        }
                        let mut args = args.clone();
                        if !defaulted {
                            args["library"] = json!(name);
                        }
                        let response = server
                            .dispatch("tools/call", json!({"name":tool,"arguments":args}))
                            .await
                            .unwrap();
                        if *tool == "library_get" {
                            assert_eq!(response["isError"], true);
                            assert!(
                                response["content"][0]["text"]
                                    .as_str()
                                    .unwrap()
                                    .contains("not enrolled")
                            );
                        } else {
                            assert_eq!(response["isError"], false, "{response}");
                            let result: Value = serde_json::from_str(
                                response["content"][0]["text"].as_str().unwrap(),
                            )
                            .unwrap();
                            assert_eq!(result["library_id"], *name);
                        }
                    }
                }
            }
            assert!(listener.accept().is_err(), "denied owner was contacted");
            server.close().await;
        }
    }

    #[tokio::test]
    async fn configured_catalog_selection_replays_imports_offline_and_refuses_receipt_replacement()
    {
        use mneme_config::{DiscoveryBinding, DiscoveryGraph, DiscoveryReceipt, PublishedCatalog};
        const ID: &str = "01M3CPEVDTM9PP4HJTMMBZ8HR9";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("mneme-configured-discovery-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let scratch = Scratch(root);
        let path = scratch.0.join("memory.toml");
        std::fs::write(&path,format!("schema='mneme.config.v1'\n[stores.fixture]\nid='{ID}'\nowner={{url='http://{}',db='project'}}\nactive={{workspace='any'}}\ndiscovery=true\n",listener.local_addr().unwrap())).unwrap();
        let prepared = mneme_config::prepare_workspace(Some(&path), &scratch.0, None)
            .unwrap()
            .unwrap();
        let document=PublishedCatalog::parse_toml(&format!("schema='mneme.discovery-catalog.v1'\n[stores.policy]\nid='{ID}'\npermissions={{read=false,write=false}}\n"),ID).unwrap();
        let graph = DiscoveryGraph {
            seeds: vec![ID.into()],
            catalogs: std::collections::BTreeMap::from([(ID.into(), document)]),
            edges: std::collections::BTreeMap::from([(ID.into(), vec![ID.into()])]),
            ..DiscoveryGraph::default()
        };
        let receipt = DiscoveryReceipt::new(prepared.boundary_sha256().unwrap(), graph).unwrap();
        let receipt_path = scratch.0.join("receipt.json");
        let mut bytes = serde_json::to_vec(&receipt).unwrap();
        std::fs::write(&receipt_path, &bytes).unwrap();
        let binding = DiscoveryBinding {
            path: receipt_path.clone(),
            sha256: mneme_config::sha256(&bytes),
        };
        let selection =
            mneme_config::finish_workspace(prepared, Some((&receipt, binding))).unwrap();
        assert!(
            selection
                .active
                .iter()
                .all(|store| !store.eligible.read && !store.eligible.write)
        );
        let digest = selection.digest.clone();
        let server = ConfiguredServer {
            client: ConfiguredClient::new(selection).unwrap(),
        };
        let selected = server
            .dispatch("tools/call", json!({"name":"config_select","arguments":{}}))
            .await
            .unwrap();
        assert_eq!(selected["isError"], false);
        let frozen: Value =
            serde_json::from_str(selected["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(frozen["digest"], digest);
        assert!(frozen["discovery"].is_object());
        server.dispatch("tools/list", json!({})).await.unwrap();
        assert!(
            listener.accept().is_err(),
            "frozen metadata must not contact its publisher"
        );
        // Another valid JSON byte sequence still changes the frozen receipt.
        bytes.push(b' ');
        std::fs::write(&receipt_path, &bytes).unwrap();
        assert!(
            server
                .dispatch("tools/call", json!({"name":"config_select","arguments":{}}))
                .await
                .is_err()
        );
        assert!(
            listener.accept().is_err(),
            "revalidation never refreshes a changed receipt"
        );
    }

    #[tokio::test]
    async fn configured_mode_is_lazy_denied_and_exactly_fenced() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let scratch = Scratch(
            std::env::temp_dir().join(format!("mneme-configured-mcp-{}", ulid::Ulid::new())),
        );
        std::fs::create_dir(&scratch.0).unwrap();
        let path = scratch.0.join("memory.toml");
        let config = format!(
            "schema='mneme.config.v1'\n[stores.fixture]\nid='01M3CPEVDTM9PP4HJTMMBZ8HR9'\nowner={{url='http://{}',db='project'}}\nactive={{workspace='any'}}\npermissions={{read=false,write=false}}\n",
            listener.local_addr().unwrap()
        );
        std::fs::write(&path, &config).unwrap();
        let server = ConfiguredServer::from_path(&path, &scratch.0)
            .await
            .unwrap();
        server
            .dispatch("initialize", json!({"protocolVersion":"2025-11-25"}))
            .await
            .unwrap();
        let listed = server.dispatch("tools/list", json!({})).await.unwrap();
        assert_eq!(listed["tools"].as_array().unwrap().len(), 2);
        let selection = server
            .dispatch("tools/call", json!({"name":"config_select","arguments":{}}))
            .await
            .unwrap();
        assert_eq!(selection["isError"], false);
        for tool in ["core", "forget", "database_control", "discovery_catalog"] {
            let refused=server.dispatch("tools/call",json!({"name":tool,"arguments":{"db":"fixture","id":"01M3CV5EZP4HWAK56YJCMZQG2Y"}})).await.unwrap();
            assert_eq!(refused["isError"], true);
        }
        assert!(listener.accept().is_err());
        std::fs::write(&path, config.replace("read=false", "read=true")).unwrap();
        assert!(server.dispatch("tools/list", json!({})).await.is_err());
        assert!(listener.accept().is_err());
        server.close().await;
    }
    #[test]
    fn declarative_invocation_is_explicit_and_exclusive() {
        assert!(crate::parse_args_from(["--memory-config", "/config.toml"]).is_ok());
        assert!(
            crate::parse_args_from(["--memory-config", "/config.toml", "--cwd", "/workspace"])
                .is_ok()
        );
        assert!(
            crate::parse_args_from([
                "--memory-config",
                "/config.toml",
                "--cwd",
                "/workspace",
                "--library",
                "personal"
            ])
            .is_ok()
        );
        for args in [
            vec!["--cwd", "/workspace"],
            vec!["--library", "personal"],
            vec![
                "--memory-config",
                "/config.toml",
                "--cwd",
                "/workspace",
                "--db",
                "project=/store",
            ],
            vec![
                "--memory-config",
                "/config.toml",
                "--cwd",
                "/workspace",
                "--router-config",
                "/router.json",
            ],
        ] {
            assert!(crate::parse_args_from(args).is_err());
        }
    }
}
