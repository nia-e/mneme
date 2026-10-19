use mneme_config::{Config, Delivery, SelectionContext, SelectionSnapshot, select};

fn config(delivery: &str) -> Config {
    Config::parse(&format!(
        "schema='mneme.config.v1'\n[stores.memory]\nid='01ARZ3NDEKTSV4RRFFQ69G5FAV'\nowner={{url='http://127.0.0.1:1',db='project'}}\nactive={{workspace='any'}}\n{delivery}"
    ))
    .unwrap()
}

#[test]
fn absent_and_false_notice_preserve_configuration_and_selection_bytes() {
    let absent = config("");
    let explicit = config("delivery={contradiction_notice=false}");
    assert!(!Delivery::default().contradiction_notice);
    assert!(!absent.stores["memory"].delivery.contradiction_notice);
    assert_eq!(
        serde_json::to_string(&absent.stores["memory"].delivery).unwrap(),
        r#"{"core":true,"context":true}"#
    );
    assert_eq!(
        serde_json::to_vec(&absent).unwrap(),
        serde_json::to_vec(&explicit).unwrap()
    );
    let dir = tempfile::tempdir().unwrap();
    let context = SelectionContext::new(dir.path().to_owned());
    assert_eq!(
        serde_json::to_vec(&select(&absent, &context).unwrap()).unwrap(),
        serde_json::to_vec(&select(&explicit, &context).unwrap()).unwrap()
    );
}

#[test]
fn explicit_notice_roundtrips_and_is_covered_by_selection_digest() {
    let cfg = config("delivery={contradiction_notice=true}");
    assert!(cfg.stores["memory"].delivery.contradiction_notice);
    assert_eq!(
        serde_json::to_value(&cfg).unwrap()["stores"]["memory"]["delivery"]["contradiction_notice"],
        true
    );
    let json = serde_json::to_vec(&cfg).unwrap();
    assert_eq!(serde_json::from_slice::<Config>(&json).unwrap(), cfg);
    let dir = tempfile::tempdir().unwrap();
    let context = SelectionContext::new(dir.path().to_owned());
    let selected = select(&cfg, &context).unwrap();
    let encoded = serde_json::to_vec(&selected).unwrap();
    let decoded: SelectionSnapshot = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, selected);
    decoded.verify_digest().unwrap();
    assert_ne!(
        selected.digest,
        select(&config(""), &context).unwrap().digest
    );
    let mut changed = selected;
    changed
        .config
        .stores
        .get_mut("memory")
        .unwrap()
        .delivery
        .contradiction_notice = false;
    assert!(changed.verify_digest().is_err());
}

#[test]
fn notice_preference_does_not_expand_operation_grants_or_other_owners() {
    let dir = tempfile::tempdir().unwrap();
    let context = SelectionContext::new(dir.path().to_owned());
    for read in [false, true] {
        for core in [false, true] {
            for active in [false, true] {
                let mut cfg = config("");
                let owner = cfg.stores.get_mut("memory").unwrap();
                owner.permissions.read = read;
                owner.delivery.core = core;
                owner.active.enabled = active;
                let mut sibling = owner.clone();
                sibling.id = "01M3XW4C1X5E4YW0Q7Q6V5Y4XH".into();
                cfg.stores.insert("sibling".into(), sibling);
                let before = select(&cfg, &context).unwrap();
                cfg.stores
                    .get_mut("memory")
                    .unwrap()
                    .delivery
                    .contradiction_notice = true;
                let after = select(&cfg, &context).unwrap();
                for (before, after) in before.stores.iter().zip(&after.stores) {
                    assert_eq!(before.eligible, after.eligible);
                    assert_eq!(before.admitted, after.admitted);
                    assert_eq!(before.active, after.active);
                    assert_eq!(before.reason, after.reason);
                }
                assert!(!after.config.stores["sibling"].delivery.contradiction_notice);
            }
        }
    }
}

#[test]
fn notice_rejects_malformed_boolean_without_echoing_values() {
    for value in ["'private-value'", "0", "[]", "{}"] {
        let raw = format!(
            "schema='mneme.config.v1'\n[stores.memory]\nid='01ARZ3NDEKTSV4RRFFQ69G5FAV'\nowner={{url='http://127.0.0.1:1',db='project'}}\ndelivery={{contradiction_notice={value}}}"
        );
        for result in [Config::parse(&raw), Config::parse_overlay(&raw)] {
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("private-value"));
        }
    }
    for value in [r#""private-value""#, "0", "[]", "{}", "null"] {
        let raw = format!(r#"{{"contradiction_notice":{value}}}"#);
        assert!(serde_json::from_str::<Delivery>(&raw).is_err());
    }
}
