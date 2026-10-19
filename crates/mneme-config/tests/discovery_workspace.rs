use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::PathBuf;

use mneme_config::{
    Config, DiscoveryBinding, DiscoveryGraph, DiscoveryReceipt, MAX_DISCOVERY_RECEIPT_BYTES,
    PublishedCatalog, SelectionSnapshot, WorkspaceDeclarations, finish_workspace,
    prepare_workspace, read_discovery_receipt, select_workspace, sha256,
};

const SEED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const IMPORTED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
const MISC: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA3";

struct Workspace {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    config: PathBuf,
}

impl Workspace {
    fn new(text: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let cwd = root.join("work");
        let config = root.join("memory.toml");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(&config, text).unwrap();
        Self {
            _temp: temp,
            root,
            cwd,
            config,
        }
    }

    fn prepare(&self, profile: Option<&str>) -> WorkspaceDeclarations {
        prepare_workspace(Some(&self.config), &self.cwd, profile)
            .unwrap()
            .unwrap()
    }

    fn write_receipt(
        &self,
        prepared: &WorkspaceDeclarations,
        graph: DiscoveryGraph,
    ) -> (DiscoveryReceipt, DiscoveryBinding) {
        let receipt = DiscoveryReceipt::new(prepared.boundary_sha256().unwrap(), graph).unwrap();
        let bytes = serde_json::to_vec(&receipt).unwrap();
        let path = self.root.join("discovery.json");
        std::fs::write(&path, &bytes).unwrap();
        let binding = DiscoveryBinding {
            path: path.canonicalize().unwrap(),
            sha256: sha256(&bytes),
        };
        (receipt, binding)
    }

    fn pin(
        &self,
        prepared: WorkspaceDeclarations,
        graph: DiscoveryGraph,
    ) -> (SelectionSnapshot, DiscoveryReceipt, DiscoveryBinding) {
        let (receipt, binding) = self.write_receipt(&prepared, graph);
        let snapshot = finish_workspace(prepared, Some((&receipt, binding.clone()))).unwrap();
        (snapshot, receipt, binding)
    }
}

fn discovery_config(extra: &str) -> String {
    format!(
        "schema='mneme.config.v1'\n\
         [stores.seed]\nid='{SEED}'\n\
         owner={{url='http://127.0.0.1:1',db='seed'}}\n\
         discovery=true\nactive={{workspace='any'}}\n{extra}",
    )
}

fn graph() -> DiscoveryGraph {
    let catalog = PublishedCatalog::parse_toml(
        &format!(
            "schema='mneme.discovery-catalog.v1'\n\
             [stores.imported]\nid='{IMPORTED}'\n\
             owner={{url='http://127.0.0.1:1',db='imported'}}\n\
             active={{workspace='any'}}\npermissions={{write=true}}",
        ),
        SEED,
    )
    .unwrap();
    DiscoveryGraph {
        seeds: vec![SEED.to_owned()],
        catalogs: BTreeMap::from([(SEED.to_owned(), catalog)]),
        edges: BTreeMap::from([(SEED.to_owned(), vec![])]),
        unavailable: BTreeMap::new(),
        ..DiscoveryGraph::default()
    }
}

fn referral_graph(discovery: Option<bool>) -> DiscoveryGraph {
    let policy = discovery
        .map(|enabled| format!("discovery={enabled}\n"))
        .unwrap_or_default();
    let catalog = PublishedCatalog::parse_toml(
        &format!(
            "schema='mneme.discovery-catalog.v1'\n\
             [stores.imported]\nid='{IMPORTED}'\n{policy}\
             owner={{url='http://127.0.0.1:1',db='imported'}}\n\
             active={{workspace='any'}}",
        ),
        SEED,
    )
    .unwrap();
    let target =
        PublishedCatalog::parse_toml("schema='mneme.discovery-catalog.v1'", IMPORTED).unwrap();
    DiscoveryGraph {
        seeds: vec![SEED.to_owned()],
        catalogs: BTreeMap::from([(SEED.to_owned(), catalog), (IMPORTED.to_owned(), target)]),
        edges: BTreeMap::from([
            (SEED.to_owned(), vec![IMPORTED.to_owned()]),
            (IMPORTED.to_owned(), vec![]),
        ]),
        ..DiscoveryGraph::default()
    }
}

#[test]
fn frozen_discovery_snapshot_replays_without_contacting_an_owner() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let text = discovery_config("").replace(
        "http://127.0.0.1:1",
        &format!("http://{}", listener.local_addr().unwrap()),
    );
    let workspace = Workspace::new(&text);
    let prepared = workspace.prepare(None);
    let seeds = prepared.discovery_seeds().unwrap();
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].id, SEED);
    let (snapshot, receipt, binding) = workspace.pin(prepared, graph());
    assert_eq!(snapshot.discovery.as_ref().unwrap().binding, binding);
    assert_eq!(
        snapshot.discovery.as_ref().unwrap().boundary_sha256,
        receipt.boundary_sha256,
    );
    assert_eq!(snapshot.config.stores["imported"].id, IMPORTED);
    assert_eq!(snapshot.active.len(), 2);
    snapshot.revalidate_exact().unwrap();

    let serialized = serde_json::to_vec(&snapshot).unwrap();
    let restored: SelectionSnapshot = serde_json::from_slice(&serialized).unwrap();
    assert_eq!(restored, snapshot);
    restored.revalidate_exact().unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "selection and exact replay must not attempt owner acquisition",
    );
}

#[test]
fn replacing_receipt_bytes_invalidates_snapshot_even_for_an_equivalent_graph() {
    let workspace = Workspace::new(&discovery_config(""));
    let (snapshot, receipt, binding) = workspace.pin(workspace.prepare(None), graph());
    snapshot.revalidate_exact().unwrap();

    let bytes = serde_json::to_vec_pretty(&receipt).unwrap();
    assert_ne!(sha256(&bytes), binding.sha256);
    assert_eq!(DiscoveryReceipt::parse(&bytes).unwrap(), receipt);
    std::fs::write(&binding.path, &bytes).unwrap();
    let error = snapshot.revalidate_exact().unwrap_err().to_string();
    assert!(
        error.contains("pinned discovery receipt changed"),
        "{error}"
    );

    let refreshed = finish_workspace(
        workspace.prepare(None),
        Some((
            &receipt,
            DiscoveryBinding {
                path: binding.path,
                sha256: sha256(&bytes),
            },
        )),
    )
    .unwrap();
    assert_eq!(refreshed.config, snapshot.config);
    assert_ne!(refreshed.digest, snapshot.digest);
    refreshed.revalidate_exact().unwrap();
}

#[test]
fn missing_pinned_receipt_never_silently_falls_back_to_local_only_selection() {
    let workspace = Workspace::new(&discovery_config(""));
    let (snapshot, _, binding) = workspace.pin(workspace.prepare(None), graph());
    std::fs::remove_file(binding.path).unwrap();
    let error = snapshot.revalidate_exact().unwrap_err().to_string();
    assert!(
        error.contains("pinned discovery receipt is missing"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn receipt_symlink_is_rejected_even_when_it_resolves_to_the_exact_pinned_bytes() {
    use std::os::unix::fs::symlink;

    let workspace = Workspace::new(&discovery_config(""));
    let (snapshot, _, binding) = workspace.pin(workspace.prepare(None), graph());
    let target = workspace.root.join("same-receipt.json");
    let original = std::fs::read(&binding.path).unwrap();
    std::fs::rename(&binding.path, &target).unwrap();
    symlink(&target, &binding.path).unwrap();
    assert_eq!(std::fs::read(&binding.path).unwrap(), original);
    assert_eq!(sha256(&original), binding.sha256);

    let error = read_discovery_receipt(&binding.path)
        .unwrap_err()
        .to_string();
    assert!(error.contains("non-symlink"), "{error}");
    assert!(binding.load().is_err());
    assert!(snapshot.revalidate_exact().is_err());
}

#[cfg(unix)]
#[test]
fn writable_by_others_receipt_or_parent_invalidates_retained_selection() {
    use std::os::unix::fs::PermissionsExt;

    for (entry, mode) in [
        ("file", 0o620),
        ("file", 0o602),
        ("parent", 0o720),
        ("parent", 0o702),
        ("parent", 0o722),
    ] {
        let workspace = Workspace::new(&discovery_config(""));
        let (snapshot, _, binding) = workspace.pin(workspace.prepare(None), graph());
        let path = if entry == "file" {
            &binding.path
        } else {
            &workspace.root
        };
        let original_mode = std::fs::metadata(path).unwrap().permissions();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();

        let error = read_discovery_receipt(&binding.path)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not writable by others"),
            "{entry} mode {mode:o}: {error}",
        );
        assert!(binding.load().is_err(), "{entry} mode {mode:o}");
        assert!(
            snapshot.revalidate_exact().is_err(),
            "{entry} mode {mode:o}"
        );

        std::fs::set_permissions(path, original_mode).unwrap();
        snapshot.revalidate_exact().unwrap();
    }
}

#[test]
fn receipt_rejects_duplicate_graph_and_catalog_map_keys_instead_of_collapsing_them() {
    let workspace = Workspace::new(&discovery_config(""));
    let (_, receipt, binding) = workspace.pin(workspace.prepare(None), graph());
    let encoded = serde_json::to_string(&receipt).unwrap();
    let catalog = serde_json::to_string(&receipt.graph.catalogs[SEED]).unwrap();
    let imported = serde_json::to_string(&receipt.graph.catalogs[SEED].stores["imported"]).unwrap();
    let duplicate_catalog = encoded.replacen(
        "\"catalogs\":{",
        &format!("\"catalogs\":{{\"{SEED}\":{catalog},"),
        1,
    );
    let duplicate_edge = encoded.replacen("\"edges\":{", &format!("\"edges\":{{\"{SEED}\":[],"), 1);
    let duplicate_store = encoded.replacen(
        "\"stores\":{",
        &format!("\"stores\":{{\"imported\":{imported},"),
        1,
    );
    for text in [duplicate_catalog, duplicate_edge, duplicate_store] {
        assert_ne!(text, encoded);
        // Ordinary map deserialization silently accepts these equivalent
        // duplicate values. Native receipt parsing must reject the ambiguity.
        assert_eq!(
            serde_json::from_str::<DiscoveryReceipt>(&text).unwrap(),
            receipt
        );
        assert!(DiscoveryReceipt::parse(text.as_bytes()).is_err());
        std::fs::write(&binding.path, &text).unwrap();
        assert!(read_discovery_receipt(&binding.path).is_err());
        let current_binding = DiscoveryBinding {
            path: binding.path.clone(),
            sha256: sha256(text.as_bytes()),
        };
        assert!(current_binding.load().is_err());
    }
}

#[test]
fn malformed_or_oversized_receipt_is_an_error_not_a_local_route_fallback() {
    for bytes in [b"{".to_vec(), vec![b' '; MAX_DISCOVERY_RECEIPT_BYTES + 1]] {
        let workspace = Workspace::new(&discovery_config(""));
        let (snapshot, _, binding) = workspace.pin(workspace.prepare(None), graph());
        std::fs::write(&binding.path, &bytes).unwrap();

        assert!(DiscoveryReceipt::parse(&bytes).is_err());
        assert!(read_discovery_receipt(&binding.path).is_err());
        assert!(binding.load().is_err());
        assert!(snapshot.revalidate_exact().is_err());
        assert!(finish_workspace(workspace.prepare(None), None).is_err());
    }
}

#[test]
fn local_source_edit_invalidates_the_discovery_boundary_even_if_semantics_match() {
    let original = discovery_config("");
    let workspace = Workspace::new(&original);
    let (snapshot, receipt, binding) = workspace.pin(workspace.prepare(None), graph());
    std::fs::write(&workspace.config, format!("{original}\n# edited locally\n")).unwrap();
    let prepared = workspace.prepare(None);
    assert_ne!(prepared.boundary_sha256().unwrap(), receipt.boundary_sha256);
    let error = snapshot.revalidate_exact().unwrap_err().to_string();
    assert!(
        error.contains("local discovery boundary changed"),
        "{error}"
    );
    assert!(finish_workspace(prepared, Some((&receipt, binding))).is_err());
}

#[test]
fn newly_created_privacy_boundary_invalidates_existing_discovery_admission() {
    let workspace = Workspace::new(&discovery_config(""));
    let (snapshot, _, _) = workspace.pin(workspace.prepare(None), graph());
    std::fs::create_dir(workspace.cwd.join(".mneme")).unwrap();
    std::fs::write(
        workspace.cwd.join(".mneme/profile.json"),
        r#"{"schema":"mneme.profile.v2","mode":"isolated"}"#,
    )
    .unwrap();

    let error = snapshot.revalidate_exact().unwrap_err().to_string();
    assert!(
        error.contains("local discovery boundary changed"),
        "{error}"
    );
    let prepared = workspace.prepare(None);
    assert!(prepared.discovery_seeds().unwrap().is_empty());
    assert!(!prepared.catalog_contact_allowed(SEED, "seed").unwrap());
    assert!(
        !prepared
            .catalog_contact_allowed(IMPORTED, "imported")
            .unwrap()
    );
}

#[test]
fn local_only_snapshot_keeps_defaults_normalization_and_legacy_wire_shape() {
    for policy in ["", "capture={required_tags=[' Rust:Libs ', 'rust-libs']}"] {
        let text = format!(
            "schema='mneme.config.v1'\n\
             [stores.memory]\nid='{SEED}'\n\
             owner={{url='http://127.0.0.1:1',db='memory'}}\n\
             active={{workspace='any'}}\n{policy}",
        );
        let workspace = Workspace::new(&text);
        let snapshot = select_workspace(&workspace.config, &workspace.cwd, None).unwrap();
        assert_eq!(snapshot.config, Config::parse(&text).unwrap());
        assert!(snapshot.discovery.is_none());
        snapshot.revalidate_exact().unwrap();
        let serialized = serde_json::to_value(&snapshot).unwrap();
        assert!(serialized.get("discovery").is_none());
        let restored: SelectionSnapshot = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored, snapshot);
        restored.revalidate_exact().unwrap();
    }
}

#[test]
fn local_only_missing_owner_refuses_fresh_selection_and_a_retained_default() {
    for read_default in ["", "read_default='memory'\n"] {
        let original = format!(
            "schema='mneme.config.v1'\n{read_default}\
             [stores.memory]\nid='{SEED}'\n\
             owner={{url='http://127.0.0.1:1',db='memory'}}\n\
             active={{workspace='any'}}",
        );
        let workspace = Workspace::new(&original);
        let retained = select_workspace(&workspace.config, &workspace.cwd, None).unwrap();
        assert!(retained.discovery.is_none());
        retained.revalidate_exact().unwrap();
        let missing_owner = original.replace("owner={url='http://127.0.0.1:1',db='memory'}\n", "");
        std::fs::write(&workspace.config, &missing_owner).unwrap();

        assert!(Config::parse(&missing_owner).is_err());
        assert!(select_workspace(&workspace.config, &workspace.cwd, None).is_err());
        assert!(finish_workspace(workspace.prepare(None), None).is_err());
        assert!(retained.revalidate_exact().is_err());
    }
}

#[test]
fn local_privacy_ceilings_deny_catalog_contact_before_acquisition() {
    for case in [
        "requested-isolated",
        "workspace-isolated",
        "empty-allowlist",
        "excluded-root",
        "discovery-disabled",
        "device-discovery-disabled",
        "store-disabled",
        "profile-excluded",
    ] {
        let workspace = Workspace::new("schema='mneme.config.v1'");
        let policy = match case {
            "workspace-isolated" => "[workspace]\nprofile='isolated'\n".to_owned(),
            "empty-allowlist" => "[workspace]\nallow_stores=[]\n".to_owned(),
            _ => String::new(),
        };
        let active = match case {
            "excluded-root" => format!(
                "active={{workspace='any',excluded_roots=['{}']}}",
                workspace.cwd.display(),
            ),
            "store-disabled" => "active={workspace='any',enabled=false}".into(),
            "profile-excluded" => "active={workspace='any',profiles=['private']}".into(),
            _ => "active={workspace='any'}".into(),
        };
        let discovery = case != "discovery-disabled";
        let device = if case == "device-discovery-disabled" {
            "device='laptop'\n"
        } else {
            ""
        };
        let device_policy = if case == "device-discovery-disabled" {
            "\n[stores.seed.devices.laptop]\ndiscovery=false"
        } else {
            ""
        };
        let text = format!(
            "schema='mneme.config.v1'\n{device}{policy}\
             [stores.seed]\nid='{SEED}'\n\
             owner={{url='http://127.0.0.1:1',db='seed'}}\n\
             discovery={discovery}\n{active}{device_policy}",
        );
        std::fs::write(&workspace.config, text).unwrap();
        let profile = (case == "requested-isolated").then_some("isolated");
        let prepared = workspace.prepare(profile);
        assert!(prepared.discovery_seeds().unwrap().is_empty(), "{case}");
        for label in ["seed", "catalog_alias"] {
            assert!(
                !prepared.catalog_contact_allowed(SEED, label).unwrap(),
                "{case}: a catalog label must not bypass the canonical local policy",
            );
        }
    }
}

#[test]
fn isolated_explicit_allowlist_permits_only_the_locally_named_catalog() {
    let text = discovery_config("").replacen(
        "[stores.seed]",
        "[workspace]\nprofile='isolated'\nallow_stores=['seed']\n[stores.seed]",
        1,
    );
    let workspace = Workspace::new(&text);
    let prepared = workspace.prepare(None);
    assert_eq!(prepared.discovery_seeds().unwrap().len(), 1);
    assert!(
        prepared
            .catalog_contact_allowed(SEED, "catalog_alias")
            .unwrap()
    );
    assert!(
        !prepared
            .catalog_contact_allowed(IMPORTED, "imported")
            .unwrap()
    );
}

#[test]
fn project_overlay_cannot_replace_a_user_store_under_another_label() {
    let workspace = Workspace::new(&discovery_config(""));
    std::fs::create_dir(workspace.cwd.join(".mneme")).unwrap();
    std::fs::write(
        workspace.cwd.join(".mneme/memory.toml"),
        format!(
            "schema='mneme.config.v1'\n\
             [stores.not_seed]\nid='{SEED}'\n\
             owner={{url='http://127.0.0.1:1',db='replacement'}}",
        ),
    )
    .unwrap();
    let error = prepare_workspace(Some(&workspace.config), &workspace.cwd, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("canonical identity"), "{error}");
}

#[test]
fn id_only_local_override_uses_imported_route_and_keeps_local_policy() {
    let workspace = Workspace::new(&discovery_config(&format!(
        "[stores.local_name]\nid='{IMPORTED}'\npermissions={{write=false}}",
    )));
    let prepared = workspace.prepare(None);
    let graph = graph();
    let resolved = prepared.resolve_graph(&graph).unwrap();
    assert!(resolved.report.stores[IMPORTED].fields["permissions.write"].local_override);
    let (snapshot, _, _) = workspace.pin(prepared, graph);
    assert!(!snapshot.config.stores.contains_key("imported"));
    let imported = &snapshot.config.stores["local_name"];
    assert_eq!(imported.id, IMPORTED);
    assert_eq!(imported.owner.db, "imported");
    assert!(!imported.permissions.write);
    assert!(imported.permissions.read);
    assert!(
        !snapshot
            .explicit_store("local_name")
            .unwrap()
            .eligible
            .write
    );
    snapshot.revalidate_exact().unwrap();
}

#[test]
fn omitted_admitted_referral_requires_incomplete_marker_and_keeps_imports_blocked() {
    let workspace = Workspace::new(&discovery_config(""));
    let prepared = workspace.prepare(None);
    let mut graph = referral_graph(Some(true));
    graph.edges.get_mut(SEED).unwrap().clear();
    graph.edges.remove(IMPORTED);
    graph.catalogs.remove(IMPORTED);
    // Every supplied catalog and edge remains internally valid. The omission
    // only becomes visible against the complete local referral admission rule.
    graph.validate().unwrap();
    let error = prepared
        .validate_frozen_graph(&graph)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("omits or changes an admitted referral"),
        "{error}"
    );
    let (receipt, binding) = workspace.write_receipt(&prepared, graph.clone());
    assert!(finish_workspace(prepared.clone(), Some((&receipt, binding))).is_err());

    graph.incomplete = Some("publisher budget exhausted".into());
    prepared.validate_frozen_graph(&graph).unwrap();
    let resolved = prepared.resolve_graph(&graph).unwrap();
    assert_eq!(resolved.report.incomplete, graph.incomplete);
    assert!(resolved.report.stores[IMPORTED].blocked);
    assert!(resolved.report.stores[SEED].blocked);
    assert!(
        !resolved
            .config
            .stores
            .values()
            .any(|store| store.id == IMPORTED)
    );
    assert_eq!(resolved.routes[IMPORTED].db, "imported");
    let (snapshot, _, _) = workspace.pin(prepared, graph);
    assert!(snapshot.active.is_empty());
    assert!(snapshot.explicit_store("seed").is_err());
    assert!(snapshot.explicit_store("imported").is_err());
    snapshot.revalidate_exact().unwrap();
}

#[test]
fn answered_referral_requires_a_catalog_or_explicit_unavailability_evidence() {
    let workspace = Workspace::new(&discovery_config(""));
    let prepared = workspace.prepare(None);
    let mut graph = referral_graph(Some(true));
    graph.catalogs.remove(IMPORTED);
    graph.edges.remove(IMPORTED);
    graph.validate().unwrap();
    let error = prepared
        .validate_frozen_graph(&graph)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unanswered publishers"), "{error}");
    let (receipt, binding) = workspace.write_receipt(&prepared, graph.clone());
    assert!(finish_workspace(prepared.clone(), Some((&receipt, binding))).is_err());

    graph
        .unavailable
        .insert(IMPORTED.into(), "owner unavailable".into());
    prepared.validate_frozen_graph(&graph).unwrap();
    let resolved = prepared.resolve_graph(&graph).unwrap();
    assert_eq!(resolved.report.unavailable[IMPORTED], "owner unavailable");
    assert!(resolved.report.stores[IMPORTED].blocked);
    let (snapshot, _, _) = workspace.pin(prepared, graph);
    assert!(snapshot.explicit_store("imported").is_err());
    snapshot.revalidate_exact().unwrap();
}

#[test]
fn local_discovery_false_rejects_referral_even_when_acquisition_is_incomplete() {
    let workspace = Workspace::new(&discovery_config(&format!(
        "[stores.local_name]\nid='{IMPORTED}'\ndiscovery=false",
    )));
    let prepared = workspace.prepare(None);
    for incomplete in [None, Some("publisher budget exhausted".to_owned())] {
        let mut graph = referral_graph(Some(true));
        graph.incomplete = incomplete;
        graph.validate().unwrap();
        let referral = &graph.catalogs[SEED].stores["imported"];
        assert!(
            !prepared
                .catalog_referral_allowed("imported", referral)
                .unwrap()
        );
        assert!(prepared.validate_frozen_graph(&graph).is_err());
        let (receipt, binding) = workspace.write_receipt(&prepared, graph);
        assert!(finish_workspace(prepared.clone(), Some((&receipt, binding))).is_err());
    }
}

#[test]
fn id_only_local_opt_in_admits_referral_even_without_remote_opt_in() {
    for remote_discovery in [None, Some(false)] {
        let workspace = Workspace::new(&discovery_config(&format!(
            "[stores.local_name]\nid='{IMPORTED}'\ndiscovery=true",
        )));
        let prepared = workspace.prepare(None);
        let seeds = prepared.discovery_seeds().unwrap();
        assert_eq!(
            seeds
                .iter()
                .map(|seed| seed.id.as_str())
                .collect::<Vec<_>>(),
            [SEED]
        );
        let graph = referral_graph(remote_discovery);
        let referral = &graph.catalogs[SEED].stores["imported"];
        assert!(
            prepared
                .catalog_referral_allowed("imported", referral)
                .unwrap()
        );
        prepared.validate_frozen_graph(&graph).unwrap();
        let resolved = prepared.resolve_graph(&graph).unwrap();
        assert!(!resolved.report.stores[IMPORTED].blocked);
        assert_eq!(resolved.routes[IMPORTED].db, "imported");
        let (snapshot, _, _) = workspace.pin(prepared, graph);
        assert_eq!(snapshot.config.stores["local_name"].owner.db, "imported");
        assert!(snapshot.explicit_store("local_name").unwrap().eligible.read);
        snapshot.revalidate_exact().unwrap();
    }
}

#[test]
fn opt_in_without_any_complete_seed_route_requires_an_explicit_incomplete_graph() {
    for owner in ["", "owner={url='http://127.0.0.1:1'}", "owner={db='seed'}"] {
        let workspace = Workspace::new(&format!(
            "schema='mneme.config.v1'\n[stores.sparse]\nid='{SEED}'\ndiscovery=true\n{owner}",
        ));
        let prepared = workspace.prepare(None);
        assert!(prepared.has_discovery());
        assert!(prepared.discovery_seeds().unwrap().is_empty());
        let mut graph = DiscoveryGraph::default();
        let inspected = prepared.resolve_graph(&graph).unwrap();
        assert!(inspected.report.stores[SEED].blocked);
        assert!(inspected.config.stores.is_empty());
        assert!(inspected.routes.is_empty());
        let error = prepared
            .validate_frozen_graph(&graph)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no complete locally routed seed"), "{error}");
        let (receipt, binding) = workspace.write_receipt(&prepared, graph.clone());
        assert!(finish_workspace(prepared.clone(), Some((&receipt, binding))).is_err());

        graph.incomplete = Some("no complete locally routed seed".into());
        prepared.validate_frozen_graph(&graph).unwrap();
        let inspected = prepared.resolve_graph(&graph).unwrap();
        assert!(inspected.report.stores[SEED].blocked);
        assert_eq!(inspected.report.incomplete, graph.incomplete);
        let (snapshot, _, _) = workspace.pin(prepared, graph);
        assert!(snapshot.config.stores.is_empty());
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.read_default, None);
        snapshot.revalidate_exact().unwrap();
    }
}

#[test]
fn privacy_gated_complete_seeds_allow_a_genuinely_empty_finished_graph() {
    for isolated in [false, true] {
        let workspace = Workspace::new(&discovery_config(""));
        if !isolated {
            let text = discovery_config("").replace(
                "active={workspace='any'}",
                &format!(
                    "active={{workspace='any',excluded_roots=['{}']}}",
                    workspace.cwd.display(),
                ),
            );
            std::fs::write(&workspace.config, text).unwrap();
        }
        let prepared = workspace.prepare(isolated.then_some("isolated"));
        assert!(prepared.has_discovery());
        assert_eq!(prepared.local.discovery_seeds().unwrap().len(), 1);
        assert!(prepared.discovery_seeds().unwrap().is_empty());
        let graph = DiscoveryGraph::default();
        prepared.validate_frozen_graph(&graph).unwrap();
        let (snapshot, receipt, _) = workspace.pin(prepared, graph);
        assert!(receipt.graph.incomplete.is_none());
        assert!(snapshot.active.is_empty());
        assert!(snapshot.explicit_store("seed").is_err());
        assert_eq!(snapshot.read_default, None);
        snapshot.revalidate_exact().unwrap();
    }
}

#[test]
fn local_boundary_changed_during_acquisition_refuses_finish_not_only_later_replay() {
    for add_profile in [false, true] {
        let original = discovery_config("");
        let workspace = Workspace::new(&original);
        let prepared = workspace.prepare(None);
        let (receipt, binding) = workspace.write_receipt(&prepared, graph());
        if add_profile {
            std::fs::create_dir(workspace.cwd.join(".mneme")).unwrap();
            std::fs::write(
                workspace.cwd.join(".mneme/profile.json"),
                r#"{"schema":"mneme.profile.v2","mode":"isolated"}"#,
            )
            .unwrap();
        } else {
            std::fs::write(
                &workspace.config,
                format!("{original}\n# acquired slowly\n"),
            )
            .unwrap();
        }
        let error = finish_workspace(prepared, Some((&receipt, binding)))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("boundary changed during acquisition"),
            "{error}"
        );
    }
}

#[test]
fn unresolved_known_project_store_still_prevents_misc_fallback() {
    let workspace = Workspace::new("schema='mneme.config.v1'");
    std::fs::write(
        &workspace.config,
        discovery_config(&format!(
            "[stores.project]\nid='{IMPORTED}'\n\
             active={{workspace='project',roots=['{}']}}\n\
             [stores.misc]\nid='{MISC}'\n\
             owner={{url='http://127.0.0.1:1',db='misc'}}\n\
             active={{workspace='no-project'}}",
            workspace.cwd.display(),
        )),
    )
    .unwrap();
    let prepared = workspace.prepare(None);
    let graph = DiscoveryGraph {
        seeds: vec![SEED.into()],
        catalogs: BTreeMap::from([(
            SEED.into(),
            PublishedCatalog::parse_toml("schema='mneme.discovery-catalog.v1'", SEED).unwrap(),
        )]),
        ..DiscoveryGraph::default()
    };
    let inspected = prepared.resolve_graph(&graph).unwrap();
    assert!(inspected.report.stores[IMPORTED].blocked);
    assert!(!inspected.config.stores.contains_key("project"));
    assert!(inspected.project_roots.contains(&workspace.cwd));
    let (snapshot, _, _) = workspace.pin(prepared, graph);
    assert!(snapshot.project.configured);
    let misc = snapshot
        .stores
        .iter()
        .find(|store| store.name == "misc")
        .unwrap();
    assert!(!misc.active);
    assert_eq!(misc.reason.as_deref(), Some("workspace-not-applicable"));
    assert!(snapshot.active.iter().all(|store| store.name != "misc"));
    assert_eq!(snapshot.read_default, None);
    snapshot.revalidate_exact().unwrap();
}
