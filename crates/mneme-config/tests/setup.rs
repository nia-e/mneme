use mneme_config::{
    ConfigDeclarations, MAX_STORES, PublishedCatalog, SetupDiscoveryMode, prepare_workspace,
    render_setup_advertise_url, setup_advertised_owner,
};

const HUB: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const OTHER: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";

fn parse_setup(fields: &str) -> mneme_config::Result<ConfigDeclarations> {
    ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\n[setup.discovery]\n{fields}",
    ))
}

#[test]
fn absent_setup_preserves_exact_declaration_serialization() {
    let config = ConfigDeclarations::parse("schema='mneme.config.v1'").unwrap();
    assert!(config.setup.is_none());
    assert_eq!(
        serde_json::to_string(&config).unwrap(),
        r#"{"schema":"mneme.config.v1","workspace":{"profile":null,"allow_stores":null,"allow_libraries":null},"stores":{},"libraries":{}}"#,
    );
    assert_eq!(config, ConfigDeclarations::default());
}

#[test]
fn setup_modes_are_closed_and_omission_stays_sparse() {
    let config = parse_setup("").unwrap();
    let discovery = config.setup.unwrap().discovery.unwrap();
    assert_eq!(discovery.mode, None);
    assert_eq!(discovery.mode(), SetupDiscoveryMode::Off);
    assert_eq!(discovery.hubs, None);
    assert_eq!(discovery.advertise_url, None);
    assert_eq!(
        serde_json::to_value(&discovery).unwrap(),
        serde_json::json!({})
    );

    for (text, expected) in [
        ("auto", SetupDiscoveryMode::Auto),
        ("ask", SetupDiscoveryMode::Ask),
        ("off", SetupDiscoveryMode::Off),
    ] {
        let config = parse_setup(&format!("mode='{text}'")).unwrap();
        let discovery = config.setup.unwrap().discovery.unwrap();
        assert_eq!(discovery.mode, Some(expected));
        assert_eq!(discovery.mode(), expected);
    }
    for fields in [
        "mode='automatic'",
        "mode='AUTO'",
        "mode=true",
        "mode=1",
        "mode=['auto']",
        "mode='auto'\nmode='ask'",
        "unknown='value'",
        "hubs='home'",
        "hubs=[1]",
        "advertise_url=42",
    ] {
        assert!(parse_setup(fields).is_err(), "accepted {fields}");
    }
    assert!(ConfigDeclarations::parse("schema='mneme.config.v1'\n[setup]\nunknown=true").is_err());
}

#[test]
fn setup_hub_lists_are_bounded_unique_names_and_explicit_empty_is_retained() {
    let names = (0..MAX_STORES)
        .map(|index| format!("'hub-{index}'"))
        .collect::<Vec<_>>();
    assert!(parse_setup(&format!("hubs=[{}]", names.join(","))).is_ok());
    assert!(parse_setup(&format!("hubs=[{},'overflow']", names.join(","))).is_err());
    for fields in [
        "hubs=['home','home']",
        "hubs=['']",
        "hubs=['not a label']",
        "hubs=['../home']",
        "hubs=['ümlaut']",
    ] {
        assert!(parse_setup(fields).is_err(), "accepted {fields}");
    }
    assert!(parse_setup(&format!("hubs=['{}']", "x".repeat(129))).is_err());
    let config = parse_setup("hubs=[]").unwrap();
    let discovery = config.setup.unwrap().discovery.unwrap();
    assert_eq!(discovery.hubs, Some(vec![]));
    assert_eq!(
        serde_json::to_value(discovery).unwrap(),
        serde_json::json!({"hubs": []})
    );
}

#[test]
fn setup_configuration_does_not_enter_materialized_config_or_published_catalogs() {
    let local = parse_setup("mode='auto'\nhubs=['home']\nadvertise_url='ssh://nia@mac'").unwrap();
    let materialized = local.materialize().unwrap();
    assert!(
        serde_json::to_value(materialized)
            .unwrap()
            .get("setup")
            .is_none()
    );

    for setup in [
        "[setup]\n",
        "[setup.discovery]\nmode='auto'\nhubs=['home']\n",
    ] {
        let text = format!("schema='mneme.discovery-catalog.v1'\n{setup}");
        assert!(PublishedCatalog::parse_toml(&text, HUB).is_err());
    }
    let catalog = PublishedCatalog::parse_toml("schema='mneme.discovery-catalog.v1'", HUB).unwrap();
    let mut value = serde_json::to_value(catalog).unwrap();
    value["setup"] = serde_json::json!({"discovery": {"mode": "auto", "hubs": ["home"]}});
    assert!(PublishedCatalog::parse_json(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn setup_hubs_are_local_metadata_not_active_stores_or_discovery_imports() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let config = ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\n\
         [workspace]\nprofile='isolated'\nallow_stores=[]\n\
         [stores.home]\nid='{HUB}'\n\
         owner={{url='http://{}',db='home'}}\n\
         discovery=false\nactive={{enabled=false}}\npermissions={{read=false,write=false}}\n",
        listener.local_addr().unwrap(),
    ))
    .unwrap();
    let hubs = config.setup_discovery_hubs(&["home".into()]).unwrap();
    assert_eq!(hubs.len(), 1);
    assert_eq!(hubs[0].label, "home");
    assert_eq!(hubs[0].id, HUB);
    assert_eq!(hubs[0].owner.db, "home");
    assert_eq!(
        hubs[0].owner.url,
        format!("http://{}", listener.local_addr().unwrap())
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(config.setup_discovery_hubs(&[]).unwrap().is_empty());
    assert!(
        config
            .setup_discovery_hubs(&["imported-only".into()])
            .is_err()
    );
    assert!(
        config
            .setup_discovery_hubs(&["home".into(), "home".into()])
            .is_err()
    );
}

#[test]
fn setup_hubs_use_complete_selected_device_routes_and_preserve_requested_order() {
    let config = ConfigDeclarations::parse(&format!(
        "schema='mneme.config.v1'\ndevice='laptop'\n\
         [stores.home]\nid='{HUB}'\nowner={{url='https://hub.example/mcp',db='base'}}\n\
         [stores.home.devices.laptop]\n\
         owner={{url='ssh://nia@home',db='device-home',ssh_mcp_port=3210}}\n\
         [stores.other]\nid='{OTHER}'\nowner={{url='https://other.example/mcp',db='other'}}\n",
    ))
    .unwrap();
    let hubs = config
        .setup_discovery_hubs(&["other".into(), "home".into()])
        .unwrap();
    assert_eq!(
        hubs.iter()
            .map(|hub| hub.label.as_str())
            .collect::<Vec<_>>(),
        ["other", "home"]
    );
    assert_eq!(hubs[1].owner.url, "ssh://nia@home");
    assert_eq!(hubs[1].owner.db, "device-home");
    assert_eq!(hubs[1].owner.ssh_mcp_port, Some(3210));
    assert_eq!(hubs[1].id, HUB);
}

#[test]
fn setup_hub_lookup_rejects_incomplete_local_routes_without_import_fallback() {
    for owner in [
        "",
        "owner={db='home'}",
        "owner={url='https://hub.example/mcp'}",
    ] {
        let config = ConfigDeclarations::parse(&format!(
            "schema='mneme.config.v1'\n[stores.home]\nid='{HUB}'\ndiscovery=true\n{owner}",
        ))
        .unwrap();
        assert!(
            config.setup_discovery_hubs(&["home".into()]).is_err(),
            "accepted {owner}"
        );
    }
}

#[test]
fn advertised_owner_keeps_ssh_connection_port_separate_from_bound_mcp_port() {
    let ssh = setup_advertised_owner("ssh://nia@mac:2222", "project", 48321).unwrap();
    assert_eq!(ssh.url, "ssh://nia@mac:2222");
    assert_eq!(ssh.db, "project");
    assert_eq!(ssh.ssh_mcp_port, Some(48321));
    assert_eq!(ssh.token_env, None);
    assert_eq!(ssh.path, None);

    let http = setup_advertised_owner("https://mac:{port}/mcp", "project", 48321).unwrap();
    assert_eq!(http.url, "https://mac:48321/mcp");
    assert_eq!(http.ssh_mcp_port, None);
    let fixed =
        setup_advertised_owner("https://proxy.example/project/mcp", "project", 48321).unwrap();
    assert_eq!(fixed.url, "https://proxy.example/project/mcp");
}

#[test]
fn advertised_owner_and_config_reject_invalid_templates_without_coercion() {
    for template in [
        "https://mac:{random}/mcp",
        "https://{host}:1234/mcp",
        "https://mac:{port/mcp",
        "https://mac:1234/mcp}",
        "https://mac:1234/{port}}",
        "https://mac:0/mcp",
        "ssh://nia@mac:0",
        "https://user@mac/mcp",
        "ssh://nia:password@mac",
        "ssh://nia@mac/path",
        "https://mac/mcp?query=secret",
        "https://mac/mcp#fragment",
        "file:///tmp/catalog",
        "not a URL",
        "",
    ] {
        assert!(
            render_setup_advertise_url(template, 48321).is_err(),
            "rendered URL {template}"
        );
        assert!(
            setup_advertised_owner(template, "project", 48321).is_err(),
            "rendered {template}"
        );
        assert!(
            parse_setup(&format!("advertise_url='{template}'")).is_err(),
            "parsed {template}"
        );
    }
    assert!(setup_advertised_owner("https://mac:{port}/mcp", "project", 0).is_err());
    assert!(setup_advertised_owner("ssh://nia@mac", "project", 0).is_err());
    assert!(setup_advertised_owner("ssh://nia@mac", "not an alias", 48321).is_err());
}

#[test]
fn advertised_shared_routes_reject_loopback_unspecified_and_wildcard_hosts() {
    for host in [
        "localhost",
        "localhost.",
        "LOCALHOST",
        "nested.localhost",
        "nested.localhost.",
        "127.0.0.1",
        "127.1",
        "127.255.255.254",
        "0.0.0.0",
        "[::1]",
        "[::]",
        "[::ffff:127.0.0.1]",
        "*.example",
        "*",
    ] {
        for template in [format!("https://{host}/mcp"), format!("ssh://nia@{host}")] {
            assert!(
                setup_advertised_owner(&template, "project", 48321).is_err(),
                "advertised {template}"
            );
            assert!(
                parse_setup(&format!("advertise_url='{template}'")).is_err(),
                "configured {template}"
            );
        }
    }
}

#[test]
fn advertised_shared_routes_allow_named_hosts_and_private_network_addresses() {
    for template in [
        "https://memory.example:{port}/mcp",
        "http://192.168.1.5:{port}/mcp",
        "http://10.0.0.12:{port}/mcp",
        "http://172.16.1.8:{port}/mcp",
        "https://[fd00::12]:{port}/mcp",
        "ssh://nia@mac",
    ] {
        assert!(
            setup_advertised_owner(template, "project", 48321).is_ok(),
            "rejected {template}"
        );
        assert!(
            parse_setup(&format!("advertise_url='{template}'")).is_ok(),
            "rejected {template}"
        );
    }
}

fn overlay_setup(user_fields: &str, project: &str) -> ConfigDeclarations {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let project_root = root.join("project");
    let user = root.join("memory.toml");
    std::fs::create_dir_all(project_root.join(".mneme")).unwrap();
    std::fs::create_dir(project_root.join(".git")).unwrap();
    std::fs::write(&user, format!("schema='mneme.config.v1'\n{user_fields}")).unwrap();
    std::fs::write(
        project_root.join(".mneme/memory.toml"),
        format!("schema='mneme.config.v1'\n{project}"),
    )
    .unwrap();
    prepare_workspace(Some(&user), &project_root, None)
        .unwrap()
        .unwrap()
        .local
}

const USER_SETUP: &str =
    "[setup.discovery]\nmode='auto'\nhubs=['home','other']\nadvertise_url='ssh://nia@mac'\n";

#[test]
fn project_setup_omissions_inherit_and_explicit_values_replace_fieldwise() {
    for project in ["", "[setup]", "[setup.discovery]"] {
        let config = overlay_setup(USER_SETUP, project);
        let expected =
            ConfigDeclarations::parse(&format!("schema='mneme.config.v1'\n{USER_SETUP}")).unwrap();
        assert_eq!(config.setup, expected.setup, "project {project}");
    }
    let config = overlay_setup(USER_SETUP, "[setup.discovery]\nmode='ask'\nhubs=['chosen']");
    let discovery = config.setup.unwrap().discovery.unwrap();
    assert_eq!(discovery.mode, Some(SetupDiscoveryMode::Ask));
    assert_eq!(discovery.hubs, Some(vec!["chosen".into()]));
    assert_eq!(discovery.advertise_url.as_deref(), Some("ssh://nia@mac"));

    let config = overlay_setup(
        USER_SETUP,
        "[setup.discovery]\nadvertise_url='https://proxy.example/{port}'",
    );
    let discovery = config.setup.unwrap().discovery.unwrap();
    assert_eq!(discovery.mode, Some(SetupDiscoveryMode::Auto));
    assert_eq!(discovery.hubs, Some(vec!["home".into(), "other".into()]));
    assert_eq!(
        discovery.advertise_url.as_deref(),
        Some("https://proxy.example/{port}")
    );
}

#[test]
fn project_setup_empty_hubs_clears_and_deliberate_mode_override_is_allowed() {
    let config = overlay_setup(USER_SETUP, "[setup.discovery]\nhubs=[]");
    let discovery = config.setup.unwrap().discovery.unwrap();
    assert_eq!(discovery.hubs, Some(vec![]));
    assert_eq!(discovery.mode, Some(SetupDiscoveryMode::Auto));
    assert_eq!(discovery.advertise_url.as_deref(), Some("ssh://nia@mac"));

    let config = overlay_setup(
        "[setup.discovery]\nmode='off'",
        "[setup.discovery]\nmode='auto'\nhubs=['home']",
    );
    let discovery = config.setup.unwrap().discovery.unwrap();
    assert_eq!(discovery.mode, Some(SetupDiscoveryMode::Auto));
    assert_eq!(discovery.hubs, Some(vec!["home".into()]));
}

#[test]
fn project_setup_absence_stays_absent_and_project_can_introduce_defaults() {
    assert!(overlay_setup("", "").setup.is_none());
    let config = overlay_setup("", "[setup.discovery]\nmode='auto'\nhubs=['home']");
    let discovery = config.setup.unwrap().discovery.unwrap();
    assert_eq!(discovery.mode(), SetupDiscoveryMode::Auto);
    assert_eq!(discovery.hubs, Some(vec!["home".into()]));
    assert_eq!(discovery.advertise_url, None);
}
