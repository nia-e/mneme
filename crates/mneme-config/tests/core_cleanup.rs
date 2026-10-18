use mneme_config::{Config, SelectionContext, select};

fn config(background: &str) -> Config {
    Config::parse(&format!(
        "schema='mneme.config.v1'\n[stores.memory]\nid='01ARZ3NDEKTSV4RRFFQ69G5FAV'\nowner={{url='http://127.0.0.1:1',db='project'}}\nactive={{workspace='any'}}\n{background}"
    )).unwrap()
}

#[test]
fn default_core_cleanup_preserves_existing_configuration_and_selection_bytes() {
    let absent = config("");
    let explicit = config("background={core_cleanup=true}");
    assert!(absent.stores["memory"].background.core_cleanup);
    assert_eq!(
        serde_json::to_vec(&absent).unwrap(),
        serde_json::to_vec(&explicit).unwrap()
    );
    assert!(
        serde_json::to_value(&absent).unwrap()["stores"]["memory"]["background"]
            .get("core_cleanup")
            .is_none()
    );
    let dir = tempfile::tempdir().unwrap();
    let context = SelectionContext::new(dir.path().to_owned());
    let before = select(&absent, &context).unwrap();
    let after = select(&explicit, &context).unwrap();
    assert_eq!(
        serde_json::to_vec(&before).unwrap(),
        serde_json::to_vec(&after).unwrap()
    );
}

#[test]
fn foreground_only_core_cleanup_changes_policy_not_native_operation_grants() {
    let cfg = config("background={core_cleanup=false}");
    assert!(!cfg.stores["memory"].background.core_cleanup);
    let dir = tempfile::tempdir().unwrap();
    let selected = select(&cfg, &SelectionContext::new(dir.path().to_owned())).unwrap();
    let owner = &selected.active[0];
    assert!(owner.eligible.maintenance && owner.eligible.record && owner.eligible.write);
    assert_eq!(
        serde_json::to_value(&cfg).unwrap()["stores"]["memory"]["background"]["core_cleanup"],
        false
    );
    let default = select(&config(""), &SelectionContext::new(dir.path().to_owned())).unwrap();
    assert_ne!(default.digest, selected.digest);
    for value in ["'false'", "0", "[]"] {
        let raw = format!(
            "schema='mneme.config.v1'\n[stores.memory]\nid='01ARZ3NDEKTSV4RRFFQ69G5FAV'\nowner={{url='http://127.0.0.1:1',db='project'}}\nbackground={{core_cleanup={value}}}"
        );
        assert!(Config::parse(&raw).is_err());
    }
}
