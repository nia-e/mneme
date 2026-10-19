use std::collections::BTreeMap;

use mneme_config::{
    Config, ConfigDeclarations, DiscoveryGraph, DiscoveryResolution, PublishedCatalog, Store,
    resolve_discovery,
};

const A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
const C: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA3";
const D: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA4";
const E: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA5";
const TARGET: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAT";
const GUIDE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

fn name(id: &str) -> String {
    format!("store_{}", id.chars().last().unwrap())
}

fn route(id: &str) -> String {
    format!("{{ url='http://127.0.0.1:1', db='{}' }}", name(id))
}

fn local(extra: &str) -> ConfigDeclarations {
    ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\n\
         [stores.seed]\nid='{A}'\nowner={}\ndiscovery=true\n{extra}",
        route(A),
    ))
    .unwrap()
}

fn document(links: &[&str], target: Option<&str>) -> String {
    let mut text = "schema='mneme.discovery-catalog.v1'\n".to_owned();
    for id in links {
        text.push_str(&format!(
            "[stores.{}]\nid='{id}'\nowner={}\ndiscovery=true\n",
            name(id),
            route(id),
        ));
    }
    if let Some(fields) = target {
        text.push_str(&format!(
            "[stores.target]\nid='{TARGET}'\nowner={}\n{fields}\n",
            route(TARGET),
        ));
    }
    text
}

fn graph(nodes: &[(&str, &[&str], Option<&str>)]) -> DiscoveryGraph {
    let mut catalogs = BTreeMap::new();
    let mut edges = BTreeMap::new();
    for (publisher, links, fields) in nodes {
        catalogs.insert(
            (*publisher).to_owned(),
            PublishedCatalog::parse_toml(&document(links, *fields), publisher).unwrap(),
        );
        edges.insert(
            (*publisher).to_owned(),
            links.iter().map(|id| (*id).to_owned()).collect(),
        );
    }
    DiscoveryGraph {
        seeds: vec![A.to_owned()],
        catalogs,
        edges,
        unavailable: BTreeMap::new(),
        ..DiscoveryGraph::default()
    }
}

fn resolve(nodes: &[(&str, &[&str], Option<&str>)]) -> DiscoveryResolution {
    resolve_discovery(&local(""), &graph(nodes)).unwrap()
}

fn target(resolution: &DiscoveryResolution) -> &Store {
    resolution
        .config
        .stores
        .values()
        .find(|store| store.id == TARGET)
        .expect("target should remain resolved")
}

fn no_target(resolution: &DiscoveryResolution) {
    assert!(
        resolution
            .config
            .stores
            .values()
            .all(|store| store.id != TARGET),
        "an unresolved target must not regain defaults in the executable config",
    );
    assert!(resolution.report.stores[TARGET].blocked);
}

#[test]
fn no_discovery_preserves_legacy_defaults_and_serialization() {
    for policy in ["", "capture={required_tags=[' Rust:Libs ', 'rust-libs']}"] {
        let raw = format!(
            "schema='mneme.config.v1'\n[stores.memory]\nid='{TARGET}'\nowner={}\n{policy}",
            route(TARGET),
        );
        let declarations = ConfigDeclarations::parse(&raw).unwrap();
        let graph = DiscoveryGraph {
            seeds: vec![],
            catalogs: BTreeMap::new(),
            edges: BTreeMap::new(),
            unavailable: BTreeMap::new(),
            ..DiscoveryGraph::default()
        };
        let resolved = resolve_discovery(&declarations, &graph).unwrap();
        assert_eq!(
            serde_json::to_value(&resolved.config).unwrap(),
            serde_json::to_value(Config::parse(&raw).unwrap()).unwrap(),
        );
    }
}

#[test]
fn same_branch_first_declaration_wins_and_deeper_omissions_fill() {
    let result = resolve(&[
        (A, &[B], Some("permissions={read=true}")),
        (
            B,
            &[D],
            Some("permissions={read=false,write=false}\ndelivery={context=false}"),
        ),
        (
            D,
            &[],
            Some("permissions={read=false,write=true}\ndelivery={context=true,core=false}"),
        ),
    ]);
    let store = target(&result);
    assert!(store.permissions.read);
    assert!(!store.permissions.write);
    assert!(!store.delivery.context);
    assert!(!store.delivery.core);
    let read = &result.report.stores[TARGET].fields["permissions.read"];
    assert_eq!(read.contributors.len(), 1);
    assert_eq!(read.contributors[0].publisher_db_id, A);
    assert!(read.shadowed.iter().any(|entry| entry.publisher_db_id == B));
    assert!(read.shadowed.iter().any(|entry| entry.publisher_db_id == D));
}

#[test]
fn diamond_independent_permission_branches_conservatively_meet_at_false() {
    let result = resolve(&[
        (A, &[B, C], None),
        (B, &[D], Some("permissions={read=true,write=false}")),
        (C, &[D], Some("permissions={read=false,write=true}")),
        (D, &[], Some("permissions={read=true,write=true}")),
    ]);
    assert!(!target(&result).permissions.read);
    assert!(!target(&result).permissions.write);
    let read = &result.report.stores[TARGET].fields["permissions.read"];
    let publishers: Vec<_> = read
        .contributors
        .iter()
        .map(|entry| entry.publisher_db_id.as_str())
        .collect();
    assert_eq!(publishers, [B, C]);
    assert!(read.conflict);
    assert!(!result.report.stores[TARGET].blocked);
    assert!(
        result.report.conflicts.iter().any(|conflict| {
            conflict.store_id == TARGET && conflict.field == "permissions.read"
        })
    );
    assert!(
        read.contributors
            .iter()
            .all(|entry| entry.witness.first().map(String::as_str) == Some(A))
    );
}

#[test]
fn unequal_branch_depths_do_not_establish_precedence() {
    let result = resolve(&[
        (A, &[B, C], None),
        (B, &[D], Some("active={profiles=['default']}")),
        (C, &[E], None),
        (E, &[D], Some("active={profiles=['private']}")),
        (D, &[], Some("active={profiles=['isolated']}")),
    ]);
    no_target(&result);
    let profiles = &result.report.stores[TARGET].fields["active.profiles"];
    assert!(profiles.conflict);
    assert_eq!(
        profiles
            .contributors
            .iter()
            .map(|entry| entry.publisher_db_id.as_str())
            .collect::<Vec<_>>(),
        [B, E],
    );
}

#[test]
fn bypass_preserves_a_declaration_shadowed_on_another_branch() {
    let result = resolve(&[
        (A, &[B, C], None),
        (B, &[E], Some("active={profiles=['default']}")),
        (C, &[E], None),
        (E, &[], Some("active={profiles=['private']}")),
    ]);
    no_target(&result);
    let profiles = &result.report.stores[TARGET].fields["active.profiles"];
    assert_eq!(profiles.contributors.len(), 2);
    let bypass = profiles
        .contributors
        .iter()
        .find(|entry| entry.publisher_db_id == E)
        .unwrap();
    assert_eq!(bypass.witness, [A, C, E]);
}

#[test]
fn collective_barriers_shadow_without_a_single_dominator() {
    let result = resolve(&[
        (A, &[B, C], None),
        (B, &[E], Some("active={profiles=['default']}")),
        (C, &[E], Some("active={profiles=['default']}")),
        (E, &[], Some("active={profiles=['private']}")),
    ]);
    assert_eq!(target(&result).active.profiles, ["default"]);
    let profiles = &result.report.stores[TARGET].fields["active.profiles"];
    assert_eq!(profiles.contributors.len(), 2);
    assert!(
        profiles
            .contributors
            .iter()
            .all(|entry| entry.publisher_db_id != E)
    );
    assert!(
        profiles
            .shadowed
            .iter()
            .any(|entry| entry.publisher_db_id == E)
    );
}

#[test]
fn cycles_terminate_and_deeper_fields_still_fill() {
    let nodes = [
        (A, &[B, C][..], Some("delivery={core=false}")),
        (B, &[A, B][..], Some("permissions={write=false}")),
        (C, &[B][..], Some("delivery={context=false}")),
    ];
    let graph = graph(&nodes);
    let first = resolve_discovery(&local(""), &graph).unwrap();
    let mut reversed = graph.clone();
    for links in reversed.edges.values_mut() {
        links.reverse();
    }
    let second = resolve_discovery(&local(""), &reversed).unwrap();
    assert_eq!(first.config, second.config);
    assert!(!target(&first).delivery.core);
    assert!(!target(&first).delivery.context);
    assert!(!target(&first).permissions.write);
}

#[test]
fn publisher_self_description_is_policy_not_implicit_local_override() {
    let mut graph = graph(&[(A, &[B], None), (B, &[A], None)]);
    for (publisher, link, policy) in [
        (A, B, "permissions={write=false}"),
        (B, A, "permissions={write=true}\ndelivery={context=false}"),
    ] {
        // B's link back to A already declares A's complete route; place its
        // policy in that declaration instead of adding a duplicate identity.
        let mut text = document(&[link], None);
        if publisher == A {
            text.push_str(&format!(
                "[stores.self_description]\nid='{A}'\nowner={}\n{policy}\n",
                route(A),
            ));
        } else {
            text.push_str(&format!("{policy}\n"));
        }
        graph.catalogs.insert(
            publisher.to_owned(),
            PublishedCatalog::parse_toml(&text, publisher).unwrap(),
        );
    }
    let result = resolve_discovery(&local(""), &graph).unwrap();
    let seed = &result.config.stores["seed"];
    assert!(!seed.permissions.write);
    assert!(!seed.delivery.context);
    let write = &result.report.stores[A].fields["permissions.write"];
    assert!(!write.local_override);
    assert_eq!(write.contributors[0].publisher_db_id, A);
}

#[test]
fn explicit_local_true_false_and_empty_lists_override_imports_by_identity() {
    let declarations = local(&format!(
        "[stores.local_label]\nid='{TARGET}'\n\
         permissions={{read=true,write=false}}\nactive={{profiles=[]}}",
    ));
    let result = resolve_discovery(
        &declarations,
        &graph(&[(
            A,
            &[],
            Some("permissions={read=false,write=true}\nactive={profiles=['private']}"),
        )]),
    )
    .unwrap();
    assert_eq!(result.config.stores["local_label"].id, TARGET);
    let store = target(&result);
    assert!(store.permissions.read);
    assert!(!store.permissions.write);
    assert!(store.active.profiles.is_empty());
    for field in ["permissions.read", "permissions.write", "active.profiles"] {
        assert!(result.report.stores[TARGET].fields[field].local_override);
    }
}

#[test]
fn explicit_clear_removes_optional_policy_instead_of_recovering_imports() {
    let declarations = local(&format!(
        "[stores.target]\nid='{TARGET}'\nclear=['cache','retrieval','guidance.tag_guide']",
    ));
    let policy = format!(
        "cache={{library='archive'}}\nretrieval={{tags=['lesson']}}\n\
         guidance={{tag_guide={{id='{GUIDE}',content='summary'}}}}",
    );
    let result = resolve_discovery(&declarations, &graph(&[(A, &[], Some(&policy))])).unwrap();
    let store = target(&result);
    assert!(store.cache.is_none());
    assert!(store.retrieval.is_none());
    assert!(store.guidance.tag_guide.is_none());
}

#[test]
fn duplicate_unsupported_or_simultaneous_clear_is_rejected() {
    for fields in [
        "clear=['retrieval','retrieval']",
        "clear=['permissions.read']",
        "clear=['owner']",
        "clear=['retrieval']\nretrieval={tags=['lesson']}",
    ] {
        let raw = format!("schema='mneme.config.v1'\n[stores.target]\nid='{TARGET}'\n{fields}");
        assert!(
            ConfigDeclarations::parse(&raw).is_err(),
            "accepted {fields}"
        );
    }
}

#[test]
fn conflicting_non_boolean_policy_omits_store_without_permissive_default() {
    let result = resolve(&[
        (A, &[B, C], None),
        (
            B,
            &[],
            Some("capture={max_summary_bytes=64}\npermissions={write=false}"),
        ),
        (C, &[], Some("capture={max_summary_bytes=128}")),
    ]);
    no_target(&result);
    assert!(result.report.conflicts.iter().any(|conflict| {
        conflict.store_id == TARGET && conflict.field == "capture.max_summary_bytes"
    }));
    assert_eq!(
        result.report.stores[TARGET].fields["permissions.write"].effective,
        Some(serde_json::json!(false)),
    );
}

fn conflicting_routes() -> DiscoveryGraph {
    let mut result = graph(&[(A, &[B, C], None), (B, &[], None), (C, &[], None)]);
    for (publisher, endpoint, db) in [
        (B, "http://127.0.0.1:2", "one"),
        (C, "http://127.0.0.1:3", "two"),
    ] {
        let text = format!(
            "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='{TARGET}'\n\
             owner={{url='{endpoint}',db='{db}'}}",
        );
        result.catalogs.insert(
            publisher.to_owned(),
            PublishedCatalog::parse_toml(&text, publisher).unwrap(),
        );
    }
    result
}

#[test]
fn conflicting_owner_routes_are_not_spliced_and_full_local_route_overrides() {
    let graph = conflicting_routes();
    let unresolved = resolve_discovery(&local(""), &graph).unwrap();
    no_target(&unresolved);
    assert!(
        unresolved
            .report
            .conflicts
            .iter()
            .any(|conflict| conflict.store_id == TARGET && conflict.field == "owner")
    );

    let full = local(&format!(
        "[stores.target]\nid='{TARGET}'\nowner={{url='http://127.0.0.1:4',db='local'}}",
    ));
    let result = resolve_discovery(&full, &graph).unwrap();
    assert_eq!(target(&result).owner.url, "http://127.0.0.1:4");
    assert_eq!(target(&result).owner.db, "local");
    assert!(result.report.stores[TARGET].fields["owner"].local_override);
}

#[test]
fn partial_local_route_does_not_hide_remaining_imported_route_conflict() {
    let declarations = local(&format!(
        "[stores.target]\nid='{TARGET}'\nowner={{db='local'}}",
    ));
    let result = resolve_discovery(&declarations, &conflicting_routes()).unwrap();
    no_target(&result);
}

#[test]
fn partial_local_route_resolves_when_resulting_atomic_bundles_agree() {
    let declarations = local(&format!(
        "[stores.target]\nid='{TARGET}'\nowner={{db='local'}}",
    ));
    let mut graph = conflicting_routes();
    let text = format!(
        "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='{TARGET}'\n\
         owner={{url='http://127.0.0.1:2',db='two'}}",
    );
    graph.catalogs.insert(
        C.to_owned(),
        PublishedCatalog::parse_toml(&text, C).unwrap(),
    );
    let result = resolve_discovery(&declarations, &graph).unwrap();
    assert_eq!(target(&result).owner.url, "http://127.0.0.1:2");
    assert_eq!(target(&result).owner.db, "local");
}

#[test]
fn imported_device_paths_are_never_universally_interpreted() {
    for fields in [
        "active={roots=['/work/project']}",
        "active={subtree_roots=['/work/project']}",
        "active={excluded_roots=['/work/private']}",
        "applicability={recall={roots=['/work/project']}}",
        "applicability={record={excluded_roots=['/work/private']}}",
    ] {
        assert!(
            PublishedCatalog::parse_toml(&document(&[], Some(fields)), A).is_err(),
            "accepted {fields}"
        );
    }

    let graph = graph(&[(
        A,
        &[],
        Some(
            "[stores.target.devices.laptop.active]\nroots=['/work/project']\n\
         [stores.target.devices.laptop.applicability.recall]\nexcluded_roots=['/work/private']",
        ),
    )]);
    let mut laptop = local("");
    laptop.device = Some("laptop".into());
    let matched = resolve_discovery(&laptop, &graph).unwrap();
    assert_eq!(
        target(&matched).active.roots,
        [std::path::PathBuf::from("/work/project")]
    );
    assert_eq!(
        target(&matched).applicability.recall.excluded_roots,
        [std::path::PathBuf::from("/work/private")]
    );
    let mut desktop = local("");
    desktop.device = Some("desktop".into());
    let unmatched = resolve_discovery(&desktop, &graph).unwrap();
    if let Some(store) = unmatched
        .config
        .stores
        .values()
        .find(|store| store.id == TARGET)
    {
        assert!(store.active.roots.is_empty());
        assert!(store.applicability.recall.excluded_roots.is_empty());
    }
}

#[test]
fn imported_secret_references_and_incomplete_routes_are_rejected() {
    for owner in [
        "{url='http://127.0.0.1:1',db='memory',token_env='PRIVATE_TOKEN'}",
        "{url='http://127.0.0.1:1'}",
        "{db='memory'}",
    ] {
        let text = format!(
            "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='{TARGET}'\nowner={owner}"
        );
        assert!(PublishedCatalog::parse_toml(&text, A).is_err());
    }
}

#[test]
fn selected_device_owner_replaces_the_whole_route_and_missing_device_uses_default() {
    let publication = format!(
        "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='{TARGET}'\n\
         owner={{url='ssh://operator@owner.test',db='shared',ssh_mcp_port=18766,path='/srv/shared.db'}}\n\
         [stores.target.devices.laptop]\nowner={{url='http://127.0.0.1:18767/',db='laptop'}}\n\
         [stores.target.devices.desktop]\nowner={{url='https://desktop.test/mcp',db='desktop',path='/srv/desktop.db'}}"
    );
    let catalog = PublishedCatalog::parse_toml(&publication, A).unwrap();
    assert_eq!(
        PublishedCatalog::parse_json(&serde_json::to_vec(&catalog).unwrap()).unwrap(),
        catalog,
    );
    let graph = DiscoveryGraph {
        seeds: vec![A.into()],
        catalogs: [(A.into(), catalog)].into(),
        ..DiscoveryGraph::default()
    };
    for (device, expected_db, expected_port, expected_path) in [
        (Some("laptop"), "laptop", None, None),
        (Some("desktop"), "desktop", None, Some("/srv/desktop.db")),
        (
            Some("unlisted"),
            "shared",
            Some(18766),
            Some("/srv/shared.db"),
        ),
        (None, "shared", Some(18766), Some("/srv/shared.db")),
    ] {
        let mut declarations = local("");
        declarations.device = device.map(str::to_owned);
        let result = resolve_discovery(&declarations, &graph).unwrap();
        let owner = &target(&result).owner;
        assert_eq!(owner.db, expected_db);
        assert_eq!(owner.ssh_mcp_port, expected_port);
        assert_eq!(
            owner.path.as_deref(),
            expected_path.map(std::path::Path::new)
        );
        assert_eq!(&result.routes[TARGET], owner);
    }
}

#[test]
fn device_only_owner_requires_a_matching_device_and_can_supply_a_local_seed() {
    let mut declarations = ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\ndevice='laptop'\n[stores.target]\nid='{TARGET}'\n\
         discovery=true\n[stores.target.devices.laptop]\nowner={}",
        route(TARGET),
    ))
    .unwrap();
    let seeds = declarations.discovery_seeds().unwrap();
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].id, TARGET);
    let seeded_graph = DiscoveryGraph {
        seeds: vec![TARGET.into()],
        catalogs: [(
            TARGET.into(),
            PublishedCatalog::parse_toml(&document(&[], None), TARGET).unwrap(),
        )]
        .into(),
        ..DiscoveryGraph::default()
    };
    let matched = resolve_discovery(&declarations, &seeded_graph).unwrap();
    assert_eq!(target(&matched).owner, seeds[0].owner);

    let mut imported = graph(&[(A, &[], None)]);
    imported.catalogs.insert(
        A.into(),
        PublishedCatalog::parse_toml(
            &format!(
                "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='{TARGET}'\n\
         [stores.target.devices.laptop]\nowner={}",
                route(TARGET),
            ),
            A,
        )
        .unwrap(),
    );
    for device in [None, Some("unlisted")] {
        declarations.device = device.map(str::to_owned);
        assert!(declarations.discovery_seeds().unwrap().is_empty());
        assert!(seeded_graph.validate_for(&declarations).is_err());
        no_target(&resolve_discovery(&declarations, &DiscoveryGraph::default()).unwrap());
        let mut local = local("");
        local.device = device.map(str::to_owned);
        no_target(&resolve_discovery(&local, &imported).unwrap());
    }
}

#[test]
fn device_owner_is_complete_bounded_and_cannot_import_secrets_or_local_execution() {
    for owner in [
        "{url='http://127.0.0.1:1',db='memory',token_env='PRIVATE_TOKEN'}".to_owned(),
        "{url='http://127.0.0.1:1'}".to_owned(),
        "{db='memory'}".to_owned(),
        "{url='ssh://operator@owner.test',db='memory'}".to_owned(),
        "{url='http://127.0.0.1:1',db='memory',ssh_mcp_port=18766}".to_owned(),
        "{url='https://user:secret@owner.test',db='memory'}".to_owned(),
        "{url='https://owner.test?token=secret',db='memory'}".to_owned(),
        "{url='file:///srv/memory.db',db='memory'}".to_owned(),
        "{url='http://127.0.0.1:1',db='memory',path='relative.db'}".to_owned(),
        format!(
            "{{url='https://owner.test/{}',db='memory'}}",
            "a".repeat(4096)
        ),
    ] {
        let text = format!(
            "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='{TARGET}'\n\
             [stores.target.devices.off_device]\nowner={owner}"
        );
        assert!(
            PublishedCatalog::parse_toml(&text, A).is_err(),
            "accepted {owner}"
        );
    }
    for fields in [
        "command='sh'",
        "libraries={cached={path='/tmp/library'}}",
        "owner={url='http://127.0.0.1:1',db='memory',command='sh'}",
    ] {
        assert!(
            PublishedCatalog::parse_toml(
                &document(
                    &[],
                    Some(&format!("[stores.target.devices.laptop]\n{fields}"))
                ),
                A,
            )
            .is_err()
        );
    }
}

#[test]
fn device_owner_uses_branch_conflicts_and_explicit_local_override() {
    let graph = graph(&[
        (A, &[B, C], None),
        (
            B,
            &[],
            Some("[stores.target.devices.laptop]\nowner={url='http://127.0.0.1:18767',db='left'}"),
        ),
        (
            C,
            &[],
            Some("[stores.target.devices.laptop]\nowner={url='http://127.0.0.1:18768',db='right'}"),
        ),
    ]);
    let mut declarations = local("");
    target(&resolve_discovery(&declarations, &graph).unwrap());
    declarations.device = Some("laptop".into());
    let conflicted = resolve_discovery(&declarations, &graph).unwrap();
    no_target(&conflicted);
    assert!(conflicted.report.stores[TARGET].fields["owner"].conflict);
    assert!(!conflicted.routes.contains_key(TARGET));

    declarations = local(&format!(
        "[stores.local_target]\nid='{TARGET}'\nowner={{url='https://local.test/mcp',db='chosen'}}"
    ));
    declarations.device = Some("laptop".into());
    let overridden = resolve_discovery(&declarations, &graph).unwrap();
    assert_eq!(target(&overridden).owner.db, "chosen");
    assert!(overridden.report.stores[TARGET].fields["owner"].local_override);
    assert!(!overridden.report.stores[TARGET].fields["owner"].conflict);
}

#[test]
fn unavailable_publisher_is_distinct_from_an_empty_catalog() {
    let empty = resolve(&[(A, &[], None)]);
    assert!(empty.report.unavailable.is_empty());
    let graph = DiscoveryGraph {
        seeds: vec![A.to_owned()],
        catalogs: BTreeMap::new(),
        edges: BTreeMap::new(),
        unavailable: [(A.to_owned(), "catalog unavailable".to_owned())].into(),
        ..DiscoveryGraph::default()
    };
    let unavailable = resolve_discovery(&local(""), &graph).unwrap();
    assert_eq!(unavailable.report.unavailable[A], "catalog unavailable");
}

#[test]
fn graph_edges_cannot_forge_discovery_permission() {
    let mut graph = graph(&[(A, &[], None), (B, &[], None)]);
    graph.edges.insert(A.to_owned(), vec![B.to_owned()]);
    assert!(resolve_discovery(&local(""), &graph).is_err());
}

#[test]
fn publications_reject_unknown_schema_fields_and_oversized_input() {
    for text in [
        "schema='mneme.discovery-catalog.v0'",
        "schema='mneme.discovery-catalog.v1'\nread_default='project-or-misc'",
        "schema='mneme.discovery-catalog.v1'\nbackground={model='remote-choice'}",
        "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='not-a-canonical-id'",
    ] {
        assert!(PublishedCatalog::parse_toml(text, A).is_err());
    }
    let oversized = format!(
        "schema='mneme.discovery-catalog.v1'\n#{}",
        "x".repeat(65_536)
    );
    assert!(PublishedCatalog::parse_toml(&oversized, A).is_err());
}

#[test]
fn publication_json_and_toml_agree_and_content_revision_is_checked() {
    let catalog =
        PublishedCatalog::parse_toml(&document(&[B], Some("permissions={read=false}")), A).unwrap();
    let bytes = serde_json::to_vec(&catalog).unwrap();
    assert_eq!(PublishedCatalog::parse_json(&bytes).unwrap(), catalog);

    let mut changed = serde_json::to_value(&catalog).unwrap();
    changed["stores"]["target"]["permissions"]["read"] = serde_json::json!(true);
    assert!(PublishedCatalog::parse_json(&serde_json::to_vec(&changed).unwrap()).is_err());

    let mut invalid_identity = serde_json::to_value(&catalog).unwrap();
    invalid_identity["publisher_db_id"] = serde_json::json!("not-canonical");
    assert!(PublishedCatalog::parse_json(&serde_json::to_vec(&invalid_identity).unwrap()).is_err());

    let mut unknown_field = serde_json::to_value(&catalog).unwrap();
    unknown_field["shell_command"] = serde_json::json!("not-executable-config");
    assert!(PublishedCatalog::parse_json(&serde_json::to_vec(&unknown_field).unwrap()).is_err());
}

#[test]
fn graph_catalog_key_must_match_the_verified_publisher_identity() {
    let mut graph = graph(&[(A, &[], None)]);
    let catalog = PublishedCatalog::parse_toml(&document(&[], None), B).unwrap();
    graph.catalogs.insert(A.to_owned(), catalog);
    assert!(resolve_discovery(&local(""), &graph).is_err());
}

#[test]
fn incomplete_peer_coverage_cannot_activate_imported_defaults() {
    let mut graph = graph(&[
        (A, &[B, C], None),
        (B, &[], Some("permissions={read=true}")),
    ]);
    graph
        .unavailable
        .insert(C.to_owned(), "catalog unavailable".into());
    let result = resolve_discovery(&local(""), &graph).unwrap();
    no_target(&result);
    assert_eq!(result.report.unavailable[C], "catalog unavailable");
}

#[test]
fn local_materialization_refuses_unresolved_routes_and_composed_policy() {
    for fields in [
        "",
        "owner={db='memory'}",
        "owner={url='http://127.0.0.1:1'}",
        "owner={url='http://127.0.0.1:1',db='memory'}\ncache={library='missing'}",
        "owner={url='http://127.0.0.1:1',db='memory'}\ncapture={intent='collaboration-preferences'}",
    ] {
        let raw = format!("schema='mneme.config.v1'\n[stores.memory]\nid='{TARGET}'\n{fields}",);
        let declarations = ConfigDeclarations::parse(&raw).unwrap();
        assert!(
            declarations.materialize().is_err(),
            "accepted unresolved {fields}"
        );
    }
}

#[test]
fn device_qualification_is_per_field_and_obeys_branch_barriers() {
    let mut declarations = local("");
    declarations.device = Some("laptop".into());
    let graph = graph(&[
        (
            A,
            &[B],
            Some("[stores.target.devices.laptop.active]\nroots=['/work/a']"),
        ),
        (
            B,
            &[],
            Some("[stores.target.devices.desktop.active]\nroots=['/work/b']"),
        ),
    ]);
    let resolved = resolve_discovery(&declarations, &graph).unwrap();
    assert_eq!(
        target(&resolved).active.roots,
        [std::path::PathBuf::from("/work/a")],
    );
    assert!(!resolved.report.stores[TARGET].blocked);

    // A matching device patch is not a replacement for every missing scope
    // leaf: this patch changes permission, not the device-bound roots.
    let graph = self::graph(&[(
        A,
        &[],
        Some(
            "[stores.target.devices.laptop.permissions]\nwrite=false\n\
             [stores.target.devices.desktop.active]\nroots=['/work/b']",
        ),
    )]);
    no_target(&resolve_discovery(&declarations, &graph).unwrap());

    let local_only = ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\ndevice='laptop'\n[stores.target]\nid='{TARGET}'\n\
         owner={}\n[stores.target.devices.desktop.active]\nroots=['/work/b']",
        route(TARGET),
    ))
    .unwrap();
    assert!(local_only.materialize().is_err());
}

#[test]
fn generated_import_label_cannot_overwrite_a_local_identity_binding() {
    let reserved = format!("discovered-{TARGET}");
    let declarations = ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\nread_default='{reserved}'\n\
         [workspace]\nallow_stores=['{reserved}']\n\
         [stores.{reserved}]\nid='{A}'\nowner={}\ndiscovery=true",
        route(A),
    ))
    .unwrap();
    let text = format!(
        "schema='mneme.discovery-catalog.v1'\n[stores.{reserved}]\nid='{TARGET}'\nowner={}",
        route(TARGET),
    );
    let graph = DiscoveryGraph {
        seeds: vec![A.to_owned()],
        catalogs: [(
            A.to_owned(),
            PublishedCatalog::parse_toml(&text, A).unwrap(),
        )]
        .into(),
        ..DiscoveryGraph::default()
    };
    let resolved = resolve_discovery(&declarations, &graph).unwrap();
    assert_eq!(resolved.config.stores[&reserved].id, A);
    assert_eq!(resolved.config.read_default, reserved);
    assert_eq!(resolved.config.stores.len(), 2);
    let imported_label = &resolved.report.stores[TARGET].label;
    assert_ne!(imported_label, &reserved);
    assert_eq!(resolved.config.stores[imported_label].id, TARGET);
}

#[test]
fn sparse_local_ssh_route_can_inherit_a_coherent_imported_port() {
    let declarations = local(&format!(
        "[stores.target]\nid='{TARGET}'\nowner={{url='ssh://local-host',db='local'}}",
    ));
    let text = format!(
        "schema='mneme.discovery-catalog.v1'\n[stores.target]\nid='{TARGET}'\n\
         owner={{url='ssh://remote-host',db='remote',ssh_mcp_port=18766}}",
    );
    let graph = DiscoveryGraph {
        seeds: vec![A.to_owned()],
        catalogs: [(
            A.to_owned(),
            PublishedCatalog::parse_toml(&text, A).unwrap(),
        )]
        .into(),
        ..DiscoveryGraph::default()
    };
    let resolved = resolve_discovery(&declarations, &graph).unwrap();
    let owner = &target(&resolved).owner;
    assert_eq!(owner.url, "ssh://local-host");
    assert_eq!(owner.db, "local");
    assert_eq!(owner.ssh_mcp_port, Some(18766));
}

#[test]
fn ambiguous_imported_alias_cannot_win_a_local_default_or_allowlist() {
    let declarations = ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\nread_default='shared'\n\
         [workspace]\nallow_stores=['shared']\n\
         [stores.seed]\nid='{A}'\nowner={}\ndiscovery=true",
        route(A),
    ))
    .unwrap();
    let mut graph = graph(&[(A, &[B, C], None), (B, &[], None), (C, &[], None)]);
    for (publisher, target) in [(B, D), (C, TARGET)] {
        let text = format!(
            "schema='mneme.discovery-catalog.v1'\n[stores.shared]\nid='{target}'\nowner={}",
            route(target),
        );
        graph.catalogs.insert(
            publisher.to_owned(),
            PublishedCatalog::parse_toml(&text, publisher).unwrap(),
        );
    }
    let resolved = resolve_discovery(&declarations, &graph).unwrap();
    assert!(!resolved.config.stores.contains_key("shared"));
    for id in [D, TARGET] {
        let store_report = &resolved.report.stores[id];
        assert_ne!(store_report.label, "shared");
        assert_eq!(resolved.config.stores[&store_report.label].id, id);
    }
    assert_eq!(resolved.config.read_default, "shared");
    assert_eq!(
        resolved.config.workspace.allow_stores,
        Some(vec!["shared".into()])
    );
}

#[test]
fn sparse_discovery_entries_are_not_seeds_and_secret_refs_need_complete_ssh_route() {
    let sparse = format!("[stores.target]\nid='{TARGET}'\ndiscovery=true");
    let mixed = local(&sparse);
    let seeds = mixed.discovery_seeds().unwrap();
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].id, A);
    let only_sparse =
        ConfigDeclarations::parse(&format!("schema='mneme.config.v1'\n{sparse}")).unwrap();
    assert!(only_sparse.discovery_seeds().unwrap().is_empty());

    let missing_port = format!(
        "schema='mneme.config.v1'\n[stores.target]\nid='{TARGET}'\n\
         owner={{url='ssh://local-host',db='local',token_env='LOCAL_TOKEN'}}",
    );
    assert!(ConfigDeclarations::parse(&missing_port).is_err());
    let complete = missing_port.replace("token_env=", "ssh_mcp_port=18766,token_env=");
    assert!(ConfigDeclarations::parse(&complete).is_ok());
}

#[test]
fn off_device_scope_omission_allows_deeper_qualified_scope_to_fill() {
    for selected_field in ["roots", "subtree_roots"] {
        let mut declarations = local(&format!(
            "[stores.local_target]\nid='{TARGET}'\n\
             [stores.local_target.devices.desktop.active]\nroots=['/work/local-desktop']",
        ));
        declarations.device = Some("laptop".into());
        let selected =
            format!("[stores.target.devices.laptop.active]\n{selected_field}=['/work/laptop']",);
        let graph = graph(&[
            (
                A,
                &[B],
                Some("[stores.target.devices.desktop.active]\nroots=['/work/desktop']"),
            ),
            (B, &[], Some(&selected)),
        ]);
        let resolved = resolve_discovery(&declarations, &graph).unwrap();
        let store = target(&resolved);
        let selected_paths = if selected_field == "roots" {
            &store.active.roots
        } else {
            &store.active.subtree_roots
        };
        assert_eq!(selected_paths, &[std::path::PathBuf::from("/work/laptop")]);
        assert!(!resolved.report.stores[TARGET].blocked);
    }
}

#[test]
fn generated_alias_cannot_steal_a_later_publishers_original_alias() {
    const MIDDLE: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAX";
    const LAST: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAZ";
    let original_alias = format!("discovered-{TARGET}");
    let declarations = ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\nread_default='{original_alias}'\n\
         [workspace]\nallow_stores=['{original_alias}']\n\
         [stores.seed]\nid='{A}'\nowner={}\ndiscovery=true",
        route(A),
    ))
    .unwrap();
    let mut graph = graph(&[
        (A, &[B, C, D], None),
        (B, &[], None),
        (C, &[], None),
        (D, &[], None),
    ]);
    for (publisher, target, label) in [
        (B, TARGET, "shared"),
        (C, LAST, "shared"),
        (D, MIDDLE, original_alias.as_str()),
    ] {
        let text = format!(
            "schema='mneme.discovery-catalog.v1'\n[stores.{label}]\nid='{target}'\nowner={}",
            route(target),
        );
        graph.catalogs.insert(
            publisher.to_owned(),
            PublishedCatalog::parse_toml(&text, publisher).unwrap(),
        );
    }
    let resolved = resolve_discovery(&declarations, &graph).unwrap();
    assert_eq!(resolved.config.stores[&original_alias].id, MIDDLE);
    assert_eq!(resolved.config.read_default, original_alias);
    assert_ne!(resolved.report.stores[TARGET].label, original_alias);
    assert_eq!(
        resolved.config.stores[&resolved.report.stores[TARGET].label].id,
        TARGET,
    );
}

#[test]
fn publication_json_rejects_duplicate_store_and_device_map_keys() {
    let catalog = PublishedCatalog::parse_toml(
        &document(
            &[],
            Some("[stores.target.devices.laptop.permissions]\nread=false"),
        ),
        A,
    )
    .unwrap();
    let store = serde_json::to_string(&catalog.stores["target"]).unwrap();
    let duplicate_stores = format!(
        "{{\"schema\":\"mneme.discovery-catalog.v1\",\"publisher_db_id\":\"{A}\",\"catalog_sha256\":\"{}\",\"stores\":{{\"target\":{store},\"target\":{store}}}}}",
        catalog.catalog_sha256,
    );
    assert!(PublishedCatalog::parse_json(duplicate_stores.as_bytes()).is_err());

    let encoded = serde_json::to_string(&catalog).unwrap();
    let devices = "\"devices\":{\"laptop\":{\"permissions\":{\"read\":false}}}";
    assert!(encoded.contains(devices));
    let duplicate_devices = encoded.replace(
        devices,
        "\"devices\":{\"laptop\":{\"permissions\":{\"read\":false}},\"laptop\":{\"permissions\":{\"read\":false}}}",
    );
    assert!(PublishedCatalog::parse_json(duplicate_devices.as_bytes()).is_err());
}

#[test]
fn report_wire_distinguishes_explicit_clear_from_omitted_detail() {
    let resolution = resolve(&[(A, &[], Some("clear=['retrieval']"))]);
    let clear = &resolution.report.stores[TARGET].fields["retrieval"];
    assert_eq!(clear.effective, Some(serde_json::Value::Null));
    assert_eq!(clear.contributors[0].value, Some(serde_json::Value::Null));

    let encoded = serde_json::to_value(clear).unwrap();
    assert_eq!(encoded.get("effective"), Some(&serde_json::Value::Null));
    assert_eq!(
        encoded["contributors"][0].get("value"),
        Some(&serde_json::Value::Null)
    );
    let decoded: mneme_config::FieldDiscoveryReport = serde_json::from_value(encoded).unwrap();
    assert_eq!(&decoded, clear);

    let omitted = mneme_config::FieldDiscoveryReport {
        contributors: vec![mneme_config::DiscoveryContributor {
            publisher_db_id: A.to_owned(),
            value: None,
            witness: vec![],
        }],
        ..mneme_config::FieldDiscoveryReport::default()
    };
    let encoded = serde_json::to_value(&omitted).unwrap();
    assert!(encoded.get("effective").is_none());
    assert!(encoded["contributors"][0].get("value").is_none());
    let decoded: mneme_config::FieldDiscoveryReport = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, omitted);

    let bytes = serde_json::to_vec(&resolution.report).unwrap();
    let decoded: mneme_config::DiscoveryReport = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded, resolution.report);
}
