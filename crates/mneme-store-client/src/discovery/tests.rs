use super::{
    ResolveOptions, acquire, fetch_catalog,
    fixture::{Fixture, Options},
    replay_prepared, resolve_prepared,
};
use crate::OperationBudget;
use mneme_config::{
    ConfigDeclarations, DiscoverySeed, Owner, PublishedCatalog, SelectionContext, SelectionRequest,
    SelectionRequestMode, WorkspaceDeclarations, prepare_workspace,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

const A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA1";
const B: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA2";
const C: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA3";
const D: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA4";
const E: &str = "01ARZ3NDEKTSV4RRFFQ69G5FA5";
const TARGET: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAT";

struct Local {
    root: PathBuf,
    prepared: WorkspaceDeclarations,
}
impl Local {
    fn new(stores: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("mneme-discovery-acquisition-{}", ulid::Ulid::new()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let prepared = WorkspaceDeclarations {
            local: ConfigDeclarations::parse(&format!("schema='mneme.config.v1'\n{stores}"))
                .unwrap(),
            context: SelectionContext::new(root.clone()),
            request: SelectionRequest {
                mode: SelectionRequestMode::Explicit,
                user_path: None,
                cwd: root.clone(),
                requested_profile: None,
                lexical_pwd: None,
                user_codex_dir: None,
            },
        };
        Self { root, prepared }
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn budget() -> OperationBudget {
    OperationBudget::new(1024 * 1024, Duration::from_secs(5)).unwrap()
}
fn document(publisher: &str, stores: &str) -> Value {
    serde_json::to_value(
        PublishedCatalog::parse_toml(
            &format!("schema='mneme.discovery-catalog.v1'\n{stores}"),
            publisher,
        )
        .unwrap(),
    )
    .unwrap()
}
fn fixture(id: &'static str) -> Fixture {
    Fixture::with_options(
        document(id, ""),
        Options {
            database_id: id,
            ..Options::default()
        },
    )
}
fn declaration(label: &str, id: &str, url: &str, discovery: bool, policy: &str) -> String {
    format!(
        "[stores.{label}]\nid='{id}'\nowner={{url='{url}',db='project'}}\ndiscovery={discovery}\n{policy}\n"
    )
}
fn seed(fixture: &Fixture, id: &str) -> DiscoverySeed {
    DiscoverySeed {
        label: "seed".into(),
        id: id.into(),
        owner: Owner {
            url: fixture.url.clone(),
            db: "project".into(),
            token_env: None,
            ssh_mcp_port: None,
            path: None,
        },
    }
}
fn count(fixture: &Fixture) -> usize {
    fixture.calls.load(Ordering::Relaxed)
}

struct Network(BTreeMap<&'static str, Fixture>);
impl Network {
    fn new(ids: &[&'static str]) -> Self {
        Self(ids.iter().map(|&id| (id, fixture(id))).collect())
    }
    fn declaration(&self, id: &str) -> String {
        declaration(
            &format!("publisher_{}", id.chars().last().unwrap()),
            id,
            &self.0[id].url,
            true,
            "",
        )
    }
    fn publish(&self, id: &str, links: &[&str], target: Option<&str>) {
        let mut stores = links
            .iter()
            .map(|id| self.declaration(id))
            .collect::<String>();
        if let Some(policy) = target {
            stores.push_str(&declaration(
                "target",
                TARGET,
                "http://127.0.0.1:1",
                false,
                policy,
            ));
        }
        self.0[id].set_document(document(id, &stores));
    }
    fn local(&self) -> Local {
        Local::new(&declaration(
            "seed",
            A,
            &self.0[A].url,
            true,
            "active={workspace='any'}",
        ))
    }
    fn assert_once(&self) {
        for (id, owner) in &self.0 {
            assert_eq!(count(owner), 1, "publisher {id}");
            let requests = owner.requests.lock().unwrap();
            assert!(requests.iter().all(|request| matches!(
                request["name"].as_str(),
                Some("databases" | "discovery_catalog")
            )));
            let call = requests
                .iter()
                .find(|request| request["name"] == "discovery_catalog")
                .unwrap();
            assert_eq!(
                call["arguments"],
                json!({"db":"project","expected_db_id":id})
            );
        }
    }
}

#[tokio::test]
async fn cycles_self_edges_and_multiple_seeds_fetch_one_snapshot_per_canonical_id() {
    let network = Network::new(&[A, B]);
    network.publish(A, &[A, B], None);
    network.publish(B, &[A, B], None);
    let local = Local::new(&(network.declaration(A) + &network.declaration(B)));
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    assert_eq!(graph.seeds, [A, B]);
    assert_eq!(graph.catalogs.len(), 2);
    assert_eq!(graph.edges[A], [A, B]);
    assert_eq!(graph.edges[B], [A, B]);
    assert!(graph.unavailable.is_empty());
    network.assert_once();
}

#[tokio::test]
async fn acquired_diamond_preserves_the_unblocked_bypass_and_false_permission() {
    let network = Network::new(&[A, B, C, D]);
    network.publish(A, &[B, C], None);
    network.publish(B, &[D], Some("permissions={read=true,write=false}"));
    network.publish(C, &[D], None);
    network.publish(D, &[], Some("permissions={read=false,write=true}"));
    let local = network.local();
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    let resolved = local.prepared.resolve_graph(&graph).unwrap();
    let read = &resolved.report.stores[TARGET].fields["permissions.read"];
    assert_eq!(read.effective, Some(json!(false)));
    assert_eq!(
        read.contributors
            .iter()
            .map(|c| c.publisher_db_id.as_str())
            .collect::<Vec<_>>(),
        [B, D]
    );
    assert!(read.shadowed.iter().any(|c| c.publisher_db_id == D));
    let store = resolved
        .config
        .stores
        .values()
        .find(|store| store.id == TARGET)
        .unwrap();
    assert!(!store.permissions.read && !store.permissions.write);
    assert!(graph.unavailable.is_empty());
    network.assert_once();
}

#[tokio::test]
async fn acquired_pentagon_does_not_promote_the_shallower_branch() {
    let network = Network::new(&[A, B, C, D, E]);
    network.publish(A, &[B, C], None);
    network.publish(B, &[D], Some("permissions={read=true}"));
    network.publish(C, &[E], None);
    network.publish(E, &[D], Some("permissions={read=false}"));
    network.publish(
        D,
        &[],
        Some("permissions={read=true}\ndelivery={context=false}"),
    );
    let local = network.local();
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    let resolved = local.prepared.resolve_graph(&graph).unwrap();
    let read = &resolved.report.stores[TARGET].fields["permissions.read"];
    assert_eq!(read.effective, Some(json!(false)));
    assert_eq!(
        read.contributors
            .iter()
            .map(|c| c.publisher_db_id.as_str())
            .collect::<Vec<_>>(),
        [B, E]
    );
    assert_eq!(
        resolved.report.stores[TARGET].fields["delivery.context"].effective,
        Some(json!(false))
    );
    assert!(graph.unavailable.is_empty());
    network.assert_once();
}

#[tokio::test]
async fn local_seed_ceilings_prevent_any_catalog_or_database_contact() {
    let owner = fixture(A);
    for case in [
        "discovery",
        "disabled",
        "excluded",
        "profile",
        "isolated",
        "allow-list",
        "legacy-allow-list",
    ] {
        let mut local = Local::new(&declaration(
            "seed",
            A,
            &owner.url,
            true,
            "active={workspace='any'}",
        ));
        let store = local.prepared.local.stores.get_mut("seed").unwrap();
        match case {
            "discovery" => store.discovery = Some(false),
            "disabled" => store.active.as_mut().unwrap().enabled = Some(false),
            "excluded" => {
                store.active.as_mut().unwrap().excluded_roots = Some(vec![local.root.clone()])
            }
            "profile" => store.active.as_mut().unwrap().profiles = Some(vec!["private".into()]),
            "isolated" => local.prepared.context.profile = "isolated".into(),
            "allow-list" => {
                local.prepared.local.workspace.allow_stores = Some(vec!["other".into()])
            }
            "legacy-allow-list" => {
                local.prepared.context.project.legacy_allow_stores = Some(vec!["other".into()])
            }
            _ => unreachable!(),
        }
        let graph = acquire(&local.prepared, &budget()).await.unwrap();
        assert!(
            graph.seeds.is_empty() && graph.catalogs.is_empty(),
            "{case}"
        );
        assert_eq!(count(&owner), 0, "{case}");
        assert_eq!(owner.catalog_calls.load(Ordering::Relaxed), 0, "{case}");
        assert_eq!(owner.database_calls.load(Ordering::Relaxed), 0, "{case}");
    }
}

#[tokio::test]
async fn local_discovery_denial_blocks_an_imported_opt_in_before_contact() {
    let network = Network::new(&[A, B]);
    network.publish(A, &[B], None);
    let mut local = network.local();
    local.prepared.local.stores.insert(
        "blocked".into(),
        mneme_config::StoreDeclaration {
            id: B.into(),
            discovery: Some(false),
            ..Default::default()
        },
    );
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    assert_eq!(graph.catalogs.len(), 1);
    assert!(graph.edges[A].is_empty());
    assert_eq!(count(&network.0[A]), 1);
    assert_eq!(count(&network.0[B]), 0);
    assert_eq!(network.0[B].database_calls.load(Ordering::Relaxed), 0);
    assert!(graph.unavailable.is_empty());
}

#[tokio::test]
async fn explicit_imported_true_is_acquired_even_when_effective_discovery_is_false() {
    let network = Network::new(&[A, B, C]);
    let denied_b = declaration("publisher_2", B, &network.0[B].url, false, "");
    network.0[A].set_document(document(A, &(denied_b + &network.declaration(C))));
    network.publish(C, &[B], None);
    let local = network.local();
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    let resolved = local.prepared.resolve_graph(&graph).unwrap();
    assert_eq!(
        resolved.report.stores[B].fields["discovery"].effective,
        Some(json!(false))
    );
    assert_eq!(graph.edges[A], [C]);
    assert_eq!(graph.edges[C], [B]);
    assert!(graph.unavailable.is_empty());
    network.assert_once();
}

#[tokio::test]
async fn ambiguous_peer_routes_make_no_contact_until_a_full_local_route_selects_one() {
    let network = Network::new(&[A, B, C]);
    let alternative = fixture(C);
    network.publish(A, &[C], None);
    network.0[B].set_document(document(
        B,
        &declaration("publisher_3", C, &alternative.url, true, ""),
    ));
    let seeds = network.declaration(A) + &network.declaration(B);
    let local = Local::new(&seeds);
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    assert!(graph.unavailable.contains_key(C));
    assert!(!graph.catalogs.contains_key(C));
    assert_eq!(graph.edges[A], [C]);
    assert_eq!(graph.edges[B], [C]);
    assert!(
        local.prepared.resolve_graph(&graph).unwrap().report.stores[C].fields["owner"].conflict
    );
    assert_eq!(network.0[C].database_calls.load(Ordering::Relaxed), 0);
    assert_eq!(alternative.database_calls.load(Ordering::Relaxed), 0);
    assert_eq!(count(&network.0[C]), 0);
    assert_eq!(count(&alternative), 0);

    let overridden = Local::new(&(seeds + &declaration("chosen", C, &network.0[C].url, true, "")));
    let graph = acquire(&overridden.prepared, &budget()).await.unwrap();
    assert!(graph.unavailable.is_empty());
    assert_eq!(graph.catalogs.len(), 3);
    assert_eq!(count(&network.0[C]), 1);
    assert_eq!(alternative.database_calls.load(Ordering::Relaxed), 0);
    assert_eq!(count(&alternative), 0);
    let resolved = overridden.prepared.resolve_graph(&graph).unwrap();
    assert!(resolved.report.stores[C].fields["owner"].local_override);
    assert_eq!(resolved.routes[C].url, network.0[C].url);
    overridden.prepared.validate_frozen_graph(&graph).unwrap();
}

#[tokio::test]
async fn local_id_only_opt_in_acquires_a_route_despite_remote_false_or_omission() {
    for remote_discovery in ["discovery=false\n", ""] {
        let network = Network::new(&[A, B]);
        network.0[A].set_document(document(A, &format!(
            "[stores.remote_label]\nid='{B}'\nowner={{url='{}',db='project'}}\n{remote_discovery}",
            network.0[B].url,
        )));
        let local = Local::new(
            &(network.declaration(A)
                + &format!("[stores.locally_named]\nid='{B}'\ndiscovery=true\n")),
        );
        assert_eq!(local.prepared.discovery_seeds().unwrap().len(), 1);
        let graph = acquire(&local.prepared, &budget()).await.unwrap();
        assert_eq!(graph.seeds, [A]);
        assert_eq!(graph.edges[A], [B]);
        assert!(graph.unavailable.is_empty());
        let resolved = local.prepared.resolve_graph(&graph).unwrap();
        let discovery = &resolved.report.stores[B].fields["discovery"];
        assert_eq!(discovery.effective, Some(json!(true)));
        assert!(discovery.local_override);
        assert_eq!(resolved.report.stores[B].label, "locally_named");
        network.assert_once();
        local.prepared.validate_frozen_graph(&graph).unwrap();
    }
}

#[tokio::test]
async fn selected_device_discovery_gates_seeds_referrals_and_local_overrides() {
    let network = Network::new(&[A, B, C, D, E]);
    let referrals = [
        (B, true, false),
        (C, false, true),
        (D, true, true),
        (E, false, false),
    ]
    .into_iter()
    .map(|(id, base, device)| {
        let label = format!("publisher_{}", id.chars().last().unwrap());
        declaration(&label, id, &network.0[id].url, base, "")
            + &format!("[stores.{label}.devices.laptop]\ndiscovery={device}\n")
    })
    .collect::<String>();
    network.0[A].set_document(document(A, &referrals));
    let local_stores = declaration("seed", A, &network.0[A].url, false, "")
        + "[stores.seed.devices.laptop]\ndiscovery=true\n"
        + &format!(
            "[stores.denied]\nid='{D}'\ndiscovery=true\n[stores.denied.devices.laptop]\ndiscovery=false\n"
        )
        + &format!(
            "[stores.enabled]\nid='{E}'\ndiscovery=false\n[stores.enabled.devices.laptop]\ndiscovery=true\n"
        );
    let mut local = Local::new(&local_stores);
    local.prepared.local.device = Some("laptop".into());
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    assert_eq!(graph.seeds, [A]);
    assert_eq!(graph.edges[A], [C, E]);
    assert_eq!(
        graph
            .catalogs
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [A, C, E]
    );
    assert!(graph.unavailable.is_empty());
    for id in [A, C, E] {
        assert_eq!(count(&network.0[id]), 1, "{id}");
    }
    for id in [B, D] {
        assert_eq!(count(&network.0[id]), 0, "{id}");
        assert_eq!(
            network.0[id].database_calls.load(Ordering::Relaxed),
            0,
            "{id}"
        );
    }
    let resolved = local.prepared.resolve_graph(&graph).unwrap();
    assert_eq!(
        resolved.report.stores[D].fields["discovery"].effective,
        Some(json!(false))
    );
    assert_eq!(
        resolved.report.stores[E].fields["discovery"].effective,
        Some(json!(true))
    );
    local.prepared.validate_frozen_graph(&graph).unwrap();
}

#[tokio::test]
async fn device_owner_routes_contact_only_selected_seed_and_referral_endpoints() {
    let selected = Network::new(&[A, B]);
    let base = Network::new(&[A, B]);
    selected.0[A].set_document(document(
        A,
        &(declaration("referral", B, &base.0[B].url, true, "")
            + &format!(
                "[stores.referral.devices.laptop]\nowner={{url='{}',db='project'}}\n",
                selected.0[B].url,
            )),
    ));
    let local = Local::new(
        &("device='laptop'\n".to_owned()
            + &declaration("seed", A, &base.0[A].url, true, "")
            + &format!(
                "[stores.seed.devices.laptop]\nowner={{url='{}',db='project'}}\n",
                selected.0[A].url,
            )),
    );
    let graph = acquire(&local.prepared, &budget()).await.unwrap();
    assert_eq!(graph.seeds, [A]);
    assert_eq!(graph.edges[A], [B]);
    assert!(graph.unavailable.is_empty());
    assert!(graph.incomplete.is_none());
    let resolved = local.prepared.resolve_graph(&graph).unwrap();
    for id in [A, B] {
        assert_eq!(resolved.routes[id].url, selected.0[id].url);
        assert_eq!(selected.0[id].initialize_calls.load(Ordering::Relaxed), 1);
        assert_eq!(base.0[id].initialize_calls.load(Ordering::Relaxed), 0);
        assert_eq!(count(&base.0[id]), 0, "base catalog for {id}");
        assert_eq!(base.0[id].database_calls.load(Ordering::Relaxed), 0);
        assert_eq!(base.0[id].catalog_calls.load(Ordering::Relaxed), 0);
    }
    selected.assert_once();
    local.prepared.validate_frozen_graph(&graph).unwrap();
}

#[tokio::test]
async fn device_or_device_route_edits_invalidate_pinned_replay_without_contact() {
    let selected = fixture(A);
    let base = fixture(A);
    let local = Local::new("");
    let config = local.root.join("memory.toml");
    let receipt = local.root.join("cache/receipt.json");
    let contents = |device: &str, laptop_url: &str| {
        format!(
            "schema='mneme.config.v1'\ndevice='{device}'\nread_default='seed'\n{}\
             [stores.seed.devices.laptop]\nowner={{url='{laptop_url}',db='project'}}\n\
             [stores.seed.devices.desktop]\nowner={{url='{}',db='project'}}\n",
            declaration("seed", A, &base.url, true, "active={workspace='any'}"),
            base.url,
        )
    };
    let original = contents("laptop", &selected.url);
    std::fs::write(&config, &original).unwrap();
    let prepare = || {
        prepare_workspace(Some(&config), &local.root, None)
            .unwrap()
            .unwrap()
    };
    let pinned = resolve_prepared(prepare(), ResolveOptions::default(), &budget(), &receipt)
        .await
        .unwrap()
        .selection
        .unwrap();
    assert_eq!(pinned.config.stores["seed"].owner.url, selected.url);
    let receipt_bytes = std::fs::read(&receipt).unwrap();
    assert_eq!(
        replay_prepared(prepare(), Some(&receipt), &pinned.digest)
            .unwrap()
            .digest,
        pinned.digest,
    );
    for edited in [
        contents("desktop", &selected.url),
        contents("laptop", &base.url),
    ] {
        std::fs::write(&config, edited).unwrap();
        assert!(pinned.revalidate_exact().is_err());
        assert!(replay_prepared(prepare(), Some(&receipt), &pinned.digest).is_err());
        assert_eq!(std::fs::read(&receipt).unwrap(), receipt_bytes);
        std::fs::write(&config, &original).unwrap();
        assert_eq!(
            replay_prepared(prepare(), Some(&receipt), &pinned.digest)
                .unwrap()
                .digest,
            pinned.digest,
        );
    }
    assert_eq!(selected.initialize_calls.load(Ordering::Relaxed), 1);
    assert_eq!(count(&selected), 1);
    assert_eq!(selected.database_calls.load(Ordering::Relaxed), 1);
    assert_eq!(selected.catalog_calls.load(Ordering::Relaxed), 1);
    assert_eq!(base.initialize_calls.load(Ordering::Relaxed), 0);
    assert_eq!(count(&base), 0);
    assert_eq!(base.database_calls.load(Ordering::Relaxed), 0);
    assert_eq!(base.catalog_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn retained_maintenance_owner_can_supply_metadata_without_opening_memory() {
    let owner = Fixture::with_options(
        document(A, ""),
        Options {
            database_id: A,
            database_state: "maintenance",
            profile: "read-only",
            ..Options::default()
        },
    );
    let catalog = fetch_catalog(&seed(&owner, A), &budget()).await.unwrap();
    assert_eq!(catalog.publisher_db_id, A);
    assert_eq!(count(&owner), 1);
    assert!(
        owner
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| matches!(
                request["name"].as_str(),
                Some("databases" | "discovery_catalog")
            ))
    );
}

#[tokio::test]
async fn wrong_native_identity_hidden_legacy_tool_and_router_fail_before_discovery() {
    for (case, options) in [
        (
            "identity",
            Options {
                database_id: B,
                ..Options::default()
            },
        ),
        (
            "legacy",
            Options {
                database_id: A,
                advertised_discovery: false,
                ..Options::default()
            },
        ),
        (
            "router",
            Options {
                database_id: A,
                server_name: "mneme-router",
                ..Options::default()
            },
        ),
    ] {
        let owner = Fixture::with_options(document(A, ""), options);
        assert!(
            fetch_catalog(&seed(&owner, A), &budget()).await.is_err(),
            "{case}"
        );
        assert_eq!(count(&owner), 0, "{case}");
        if case == "router" {
            assert_eq!(owner.database_calls.load(Ordering::Relaxed), 0);
        }
    }
}

#[tokio::test]
async fn wrong_publisher_and_invalid_documents_cannot_become_empty_successes() {
    for (case, value) in [
        ("publisher", document(B, "")),
        ("schema", json!({"schema":"mneme.discovery-catalog.v0"})),
        ("revision", {
            let mut value = document(A, "");
            value["catalog_sha256"] = json!("0".repeat(64));
            value
        }),
        ("oversize", json!({"padding":"x".repeat(70_000)})),
    ] {
        let owner = fixture(A);
        owner.set_document(value);
        let local = Local::new(&declaration("seed", A, &owner.url, true, ""));
        let graph = acquire(&local.prepared, &budget()).await.unwrap();
        assert!(graph.catalogs.is_empty(), "{case}");
        assert!(graph.unavailable.contains_key(A), "{case}");
        assert!(
            local.prepared.resolve_graph(&graph).unwrap().report.stores[A].blocked,
            "{case}"
        );
        assert_eq!(count(&owner), 1, "{case}");
    }
}

#[tokio::test]
async fn a_remote_secret_reference_is_rejected_even_with_a_valid_revision() {
    let owner = fixture(A);
    let mut poisoned: PublishedCatalog = serde_json::from_value(document(
        A,
        &declaration("target", TARGET, "http://127.0.0.1:1", true, ""),
    ))
    .unwrap();
    poisoned
        .stores
        .get_mut("target")
        .unwrap()
        .owner
        .as_mut()
        .unwrap()
        .token_env = Some("MNEME_FIXTURE_MUST_NOT_RESOLVE_THIS_SECRET".into());
    poisoned.catalog_sha256 = mneme_config::sha256(&serde_json::to_vec(&poisoned.stores).unwrap());
    owner.set_document(serde_json::to_value(poisoned).unwrap());
    let error = fetch_catalog(&seed(&owner, A), &budget())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("secret references"), "{error}");
    assert_eq!(count(&owner), 1);
}

#[tokio::test]
async fn receive_allowance_is_shared_across_publishers_and_failed_attempts() {
    let network = Network::new(&[A, B]);
    let measurement = budget();
    fetch_catalog(&seed(&network.0[A], A), &measurement)
        .await
        .unwrap();
    let allowance = measurement.bytes_used() + measurement.bytes_used() / 2;
    let shared = OperationBudget::new(allowance, Duration::from_secs(5)).unwrap();
    let local = Local::new(&(network.declaration(A) + &network.declaration(B)));
    let graph = acquire(&local.prepared, &shared).await.unwrap();
    assert!(graph.catalogs.contains_key(A));
    assert!(!graph.catalogs.contains_key(B));
    assert!(graph.unavailable.contains_key(B));
    assert_eq!(shared.bytes_remaining(), 0);
    assert!(shared.bytes_used() > allowance);
    let before = network.0[B].requests.lock().unwrap().len();
    assert!(
        fetch_catalog(&seed(&network.0[B], B), &shared)
            .await
            .is_err()
    );
    assert_eq!(network.0[B].requests.lock().unwrap().len(), before);
}

#[tokio::test]
async fn exhausted_deadline_cannot_restart_for_a_second_publisher() {
    let delayed = Fixture::with_options(
        document(A, ""),
        Options {
            database_id: A,
            response_delay: Duration::from_secs(5),
            ..Options::default()
        },
    );
    let next = fixture(B);
    let shared = OperationBudget::new(1024 * 1024, Duration::from_millis(250)).unwrap();
    let started = Instant::now();
    assert!(fetch_catalog(&seed(&delayed, A), &shared).await.is_err());
    assert_eq!(count(&delayed), 1);
    assert!(fetch_catalog(&seed(&next, B), &shared).await.is_err());
    assert_eq!(next.catalog_calls.load(Ordering::Relaxed), 0);
    assert_eq!(next.database_calls.load(Ordering::Relaxed), 0);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn inflated_handshake_catalog_is_charged_before_discovery() {
    let owner = Fixture::with_options(
        document(A, ""),
        Options {
            database_id: A,
            catalog_padding_bytes: 16 * 1024,
            ..Options::default()
        },
    );
    let shared = OperationBudget::new(4096, Duration::from_secs(5)).unwrap();
    assert!(fetch_catalog(&seed(&owner, A), &shared).await.is_err());
    assert_eq!(owner.catalog_calls.load(Ordering::Relaxed), 1);
    assert_eq!(count(&owner), 0);
    assert_eq!(shared.bytes_remaining(), 0);
}
