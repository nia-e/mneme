use mneme_config::{
    Config, DiscoveryGraph, DiscoveryReceipt, Operation, PublishedCatalog, SelectionContext,
    SelectionPurpose, finish_workspace, prepare_workspace, read_discovery_receipt, select,
};
use std::collections::BTreeMap;

const SEED: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const TARGET: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAT";

#[test]
fn browse_ignores_all_workspace_applicability_but_grants_only_explicit_read() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let mut config = Config::parse(&format!(
        "schema='mneme.config.v1'\n[stores.memory]\nid='{TARGET}'\n\
         owner={{url='http://127.0.0.1:1',db='project'}}\n\
         active={{enabled=false,workspace='project',roots=['/another/device'],excluded_roots=['{}']}}\n\
         [stores.memory.applicability]\n\
         core={{roots=['/other'],excluded_roots=['{}']}}\n\
         context={{roots=['/other'],excluded_roots=['{}']}}\n\
         recall={{roots=['/other'],excluded_roots=['{}']}}\n\
         record={{roots=['/other'],excluded_roots=['{}']}}\n\
         maintenance={{roots=['/other'],excluded_roots=['{}']}}",
        cwd.display(), cwd.display(), cwd.display(), cwd.display(), cwd.display(), cwd.display(),
    )).unwrap();
    let mut context = SelectionContext::new(cwd);
    assert!(
        select(&config, &context)
            .unwrap()
            .explicit_store("memory")
            .is_err()
    );
    context.purpose = SelectionPurpose::Browse;
    let browse = select(&config, &context).unwrap();
    assert!(browse.active.is_empty());
    let memory = browse.explicit_store("memory").unwrap();
    memory.require_operation(Operation::ExplicitRead).unwrap();
    for operation in [
        Operation::ExplicitWrite,
        Operation::Core,
        Operation::Context,
        Operation::Recall,
        Operation::Record,
        Operation::Maintenance,
    ] {
        assert!(
            memory.require_operation(operation).is_err(),
            "{operation:?}"
        );
    }
    config.stores.get_mut("memory").unwrap().permissions.read = false;
    assert!(
        select(&config, &context)
            .unwrap()
            .explicit_store("memory")
            .unwrap()
            .require_operation(Operation::ExplicitRead)
            .is_err()
    );
}

#[test]
fn browse_preserves_profile_and_explicit_allowlist_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config::parse(&format!(
        "schema='mneme.config.v1'\n[stores.memory]\nid='{TARGET}'\n\
         owner={{url='http://127.0.0.1:1',db='project'}}\nactive={{profiles=['private']}}",
    ))
    .unwrap();
    let mut context = SelectionContext::new(temp.path().canonicalize().unwrap());
    context.purpose = SelectionPurpose::Browse;
    assert!(
        select(&config, &context)
            .unwrap()
            .explicit_store("memory")
            .is_err()
    );
    context.profile = "private".into();
    assert!(
        select(&config, &context)
            .unwrap()
            .explicit_store("memory")
            .is_ok()
    );
    config.workspace.allow_stores = Some(vec![]);
    assert!(
        select(&config, &context)
            .unwrap()
            .explicit_store("memory")
            .is_err()
    );
    context.profile = "isolated".into();
    config.workspace.allow_stores = Some(vec!["memory".into()]);
    assert!(
        select(&config, &context)
            .unwrap()
            .explicit_store("memory")
            .is_ok()
    );
}

#[test]
fn browse_reuses_workspace_receipt_without_activating_off_device_scopes() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let config = cwd.join("memory.toml");
    std::fs::write(
        &config,
        format!(
            "schema='mneme.config.v1'\ndevice='mac'\n[stores.seed]\nid='{SEED}'\n\
         owner={{url='http://127.0.0.1:1',db='user'}}\ndiscovery=true",
        ),
    )
    .unwrap();
    let prepared = prepare_workspace(Some(&config), &cwd, None)
        .unwrap()
        .unwrap();
    let browsing = prepared.clone().for_browsing();
    assert_eq!(
        prepared.boundary_sha256().unwrap(),
        browsing.boundary_sha256().unwrap()
    );
    assert_eq!(
        prepared.receipt_path().unwrap(),
        browsing.receipt_path().unwrap()
    );
    let catalog = PublishedCatalog::parse_toml(
        &format!(
            "schema='mneme.discovery-catalog.v1'\n[stores.remote]\nid='{TARGET}'\n\
         owner={{url='ssh://remote',db='project',ssh_mcp_port=35169}}\n\
         [stores.remote.devices.nixos.active]\nroots=['/home/remote/project']\n\
         [stores.remote.devices.nixos.applicability]\n\
         core={{roots=['/home/remote/project']}}\ncontext={{roots=['/home/remote/project']}}\n\
         recall={{roots=['/home/remote/project']}}\nrecord={{roots=['/home/remote/project']}}\n\
         maintenance={{roots=['/home/remote/project']}}",
        ),
        SEED,
    )
    .unwrap();
    let graph = DiscoveryGraph {
        seeds: vec![SEED.into()],
        catalogs: BTreeMap::from([(SEED.into(), catalog)]),
        ..Default::default()
    };
    let normal_report = prepared.resolve_graph(&graph).unwrap().report;
    assert!(normal_report.stores[TARGET].blocked);
    let receipt = DiscoveryReceipt::new(prepared.boundary_sha256().unwrap(), graph).unwrap();
    let path = cwd.join("receipt.json");
    std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let (receipt, binding) = read_discovery_receipt(&path).unwrap().unwrap();
    let workspace = finish_workspace(prepared, Some((&receipt, binding.clone()))).unwrap();
    assert!(workspace.explicit_store("remote").is_err());
    let browse = finish_workspace(browsing, Some((&receipt, binding))).unwrap();
    let remote = browse.explicit_store("remote").unwrap();
    assert_eq!(remote.store.id, TARGET);
    assert_eq!(remote.store.owner.ssh_mcp_port, Some(35169));
    assert!(remote.eligible.read);
    assert!(!remote.eligible.write && !remote.eligible.record && !remote.eligible.recall);
    assert!(browse.active.is_empty());
    browse.revalidate_exact().unwrap();
    let replay = browse
        .request
        .as_ref()
        .unwrap()
        .replay_offline(browse.discovery.as_ref())
        .unwrap()
        .unwrap();
    assert_eq!(replay, browse);
    assert_ne!(workspace.digest, browse.digest);
    let ordinary_json = serde_json::to_value(&workspace).unwrap();
    assert!(ordinary_json.get("purpose").is_none());
    assert!(ordinary_json["request"].get("purpose").is_none());

    // Applicability is ignored, not changed owner identity or route metadata.
    std::fs::write(
        &config,
        std::fs::read_to_string(&config).unwrap() + "\n# changed\n",
    )
    .unwrap();
    assert!(browse.revalidate_exact().is_err());
}

#[test]
fn browse_keeps_owner_conflicts_unresolved() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let config = cwd.join("memory.toml");
    // A sparse, incoherent route is rejected before browsing can send anything.
    std::fs::write(&config, format!(
        "schema='mneme.config.v1'\n[stores.remote]\nid='{TARGET}'\nowner={{db='project'}}",
    )).unwrap();
    let browsing = prepare_workspace(Some(&config), &cwd, None)
        .unwrap()
        .unwrap()
        .for_browsing();
    let snapshot = finish_workspace(browsing, None).unwrap();
    assert!(snapshot.explicit_store("remote").is_err());
}

#[test]
fn browse_ignores_conflicting_applicability_but_not_conflicting_owners() {
    const PEER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let path = cwd.join("memory.toml");
    std::fs::write(&path, format!(
        "schema='mneme.config.v1'\ndevice='mac'\n\
         [stores.seed]\nid='{SEED}'\nowner={{url='http://127.0.0.1:1',db='project'}}\ndiscovery=true\n\
         [stores.peer]\nid='{PEER}'\nowner={{url='http://127.0.0.1:2',db='project'}}\ndiscovery=true",
    )).unwrap();
    let prepared = prepare_workspace(Some(&path), &cwd, None).unwrap().unwrap();
    for conflicting_owner in [false, true] {
        let mut catalogs = BTreeMap::new();
        for (publisher, root, port) in [
            (SEED, "/first", 3),
            (PEER, "/second", if conflicting_owner { 4 } else { 3 }),
        ] {
            let catalog = PublishedCatalog::parse_toml(
                &format!(
                    "schema='mneme.discovery-catalog.v1'\n[stores.remote]\nid='{TARGET}'\n\
                 owner={{url='http://127.0.0.1:{port}',db='project'}}\n\
                 [stores.remote.devices.mac.active]\nroots=['{root}']",
                ),
                publisher,
            )
            .unwrap();
            catalogs.insert(publisher.into(), catalog);
        }
        let graph = DiscoveryGraph {
            seeds: vec![SEED.into(), PEER.into()],
            catalogs,
            ..Default::default()
        };
        assert!(prepared.resolve_graph(&graph).unwrap().report.stores[TARGET].blocked);
        let receipt = DiscoveryReceipt::new(prepared.boundary_sha256().unwrap(), graph).unwrap();
        let receipt_path = cwd.join("receipt.json");
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        let (receipt, binding) = read_discovery_receipt(&receipt_path).unwrap().unwrap();
        let browse =
            finish_workspace(prepared.clone().for_browsing(), Some((&receipt, binding))).unwrap();
        assert_eq!(browse.explicit_store("remote").is_err(), conflicting_owner);
        browse.revalidate_exact().unwrap();
    }
}

#[test]
fn browse_default_prefers_the_workspace_owner_but_does_not_require_applicability() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let project = cwd.join("project");
    std::fs::create_dir(&project).unwrap();
    let path = cwd.join("memory.toml");
    let text = format!(
        "schema='mneme.config.v1'\n\
         [stores.global]\nid='{SEED}'\nowner={{url='http://127.0.0.1:1',db='user'}}\nactive={{workspace='any'}}\n\
         [stores.misc]\nid='01ARZ3NDEKTSV4RRFFQ69G5FA2'\nowner={{url='http://127.0.0.1:2',db='project'}}\nactive={{workspace='no-project'}}\n\
         [stores.project]\nid='{TARGET}'\nowner={{url='http://127.0.0.1:3',db='project'}}\nactive={{workspace='project',roots=['{}']}}",
        project.display(),
    );
    std::fs::write(&path, &text).unwrap();
    for (cwd, expected) in [(&cwd, "misc"), (&project, "project")] {
        let browse = finish_workspace(
            prepare_workspace(Some(&path), cwd, None)
                .unwrap()
                .unwrap()
                .for_browsing(),
            None,
        )
        .unwrap();
        assert_eq!(browse.read_default.as_deref(), Some(expected));
        assert_eq!(browse.stores.len(), 3);
        assert!(browse.active.is_empty());
        browse.revalidate_exact().unwrap();
    }
    std::fs::write(
        &path,
        text.replacen(
            "schema='mneme.config.v1'",
            "schema='mneme.config.v1'\nread_default='project'",
            1,
        )
        .replace(
            "workspace='project',roots=",
            "enabled=false,workspace='project',roots=",
        ),
    )
    .unwrap();
    let browse = finish_workspace(
        prepare_workspace(Some(&path), &cwd, None)
            .unwrap()
            .unwrap()
            .for_browsing(),
        None,
    )
    .unwrap();
    assert_eq!(browse.read_default.as_deref(), Some("project"));
    assert!(browse.explicit_store("project").unwrap().eligible.read);
}
