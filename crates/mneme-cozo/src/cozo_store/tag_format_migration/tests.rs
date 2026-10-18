use super::*;
use mneme_core::ports::EpisodeStore;
use mneme_core::{
    BoundedTagSet, CaptureRequestCodec, CaptureSource, EpisodeCommit, EpisodeFacet, EpisodeTime,
    EpisodeWriteExpectation, OccurrenceSpan,
};
use std::process::Command;

struct Fixture {
    path: std::path::PathBuf,
    db_id: Ulid,
    original: crate::StoreExport,
    options: TagFormatMigrationOptions,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
    }
}

async fn fixture() -> Fixture {
    let dir = std::env::temp_dir().join(format!("mneme-tag-format-{}", Ulid::new()));
    std::fs::create_dir(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let path = dir.join("memory.db");
    let store = CozoStore::open(path.to_str().unwrap(), 4).unwrap();
    let mut target = crate::touchstone_tests::target(NodeId(Ulid::new()), "historical target");
    target.set_status(NodeStatus::Archived);
    store.put_node(&target).await.unwrap();
    store
        .upsert(target.id(), &[1.0, 0.0, 0.0, 0.0])
        .await
        .unwrap();
    let input = crate::touchstone_tests::input(store.db_id(), &target);
    let (owner, proof) = crate::touchstone_tests::authored("tag-format", &input);
    crate::touchstone_tests::capture(&store, &owner, &proof, &input)
        .await
        .unwrap();
    let source = CaptureSource::new_with_codec(
        "tag-format-test",
        "episode",
        "test://episode",
        None,
        None,
        [17; 32],
        CaptureRequestCodec::EpisodeV1,
    )
    .unwrap();
    let episode = Node::try_new(
        source.node_id(),
        "immutable edition",
        mneme_core::BodyRef::new("inline://edition").unwrap(),
        ["scene"],
        Provenance::External { source },
        0.5,
        0.5,
        NodeStatus::Active,
        1,
    )
    .unwrap();
    let facet = EpisodeFacet::initial(
        episode.id(),
        OccurrenceSpan::Unknown,
        None,
        EpisodeTime::new(episode.created()).unwrap(),
    )
    .unwrap();
    let episode = episode.with_episode(facet).unwrap();
    store
        .commit_episode(EpisodeCommit {
            node: &episode,
            embedding: &[1.0, 0.0, 0.0, 0.0],
            links: &[],
            expectation: EpisodeWriteExpectation::NewRoot,
        })
        .await
        .unwrap();
    // Construct exact predecessor materialization without weakening any public
    // current writer or ordinary episode/touchstone edit guard.
    for (node, tags) in [
        (&target, vec!["Rust Compiler", "rust_compiler"]),
        (&owner, vec!["TouchStone", "Core"]),
        (&episode, vec!["C++", "Core"]),
    ] {
        let mut predecessor = node.clone();
        predecessor.replace_tags(BoundedTagSet::try_from_iter(tags).unwrap());
        let tx = store.db.multi_transaction(true);
        tx_run(
            &tx,
            "?[id,data,status] <- [[$id,$data,$status]] :put node {id=>data,status}",
            BTreeMap::from([
                ("id".into(), dv_str(&node.id().0.to_string())),
                (
                    "data".into(),
                    dv_str(&encode_historical_canonical_node(&predecessor).unwrap()),
                ),
                ("status".into(), dv_str(status_str(node.status()))),
            ]),
        )
        .unwrap();
        if predecessor.is_semantic() {
            maintenance::tx_sync_tag_projection(&tx, &[&predecessor]).unwrap();
        }
        tx.commit().unwrap();
    }
    store
        .put_meta(
            crate::tag_projection::META_KEY,
            crate::tag_projection::LEGACY_META_VALUE,
        )
        .unwrap();
    let original = store.export().await.unwrap();
    let db_id = store.db_id();
    store.prepare_for_file_move().unwrap();
    drop(store);
    Fixture {
        path,
        db_id,
        original,
        options: TagFormatMigrationOptions {
            aliases: BTreeMap::from([
                ("C++".into(), "cpp".into()),
                ("Core".into(), "historical-core".into()),
            ]),
        },
    }
}

fn migration(fixture: &Fixture) -> SqliteTagFormatMigration {
    let lease = StoreLease::acquire(&fixture.path).unwrap();
    SqliteTagFormatMigration::open_legacy(&fixture.path, fixture.db_id, lease).unwrap()
}

async fn empty_fixture(dimension: usize) -> Fixture {
    let dir = std::env::temp_dir().join(format!("mneme-tag-empty-fixture-{}", Ulid::new()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.canonicalize().unwrap().join("memory.db");
    let store = CozoStore::open(path.to_str().unwrap(), dimension).unwrap();
    let db_id = store.db_id();
    store
        .put_meta(
            crate::tag_projection::META_KEY,
            crate::tag_projection::LEGACY_META_VALUE,
        )
        .unwrap();
    let original = store.export().await.unwrap();
    store.prepare_for_file_move().unwrap();
    drop(store);
    Fixture {
        path,
        db_id,
        original,
        options: TagFormatMigrationOptions::default(),
    }
}

#[tokio::test]
async fn tag_format_migration_preserves_witnesses_and_indexes_only_semantics() {
    let fixture = fixture().await;
    let lease = StoreLease::acquire(&fixture.path).unwrap();
    assert!(CozoStore::open_existing_current(&fixture.path, lease).is_err());
    let mut migration = migration(&fixture);
    let blocked = migration
        .plan(&TagFormatMigrationOptions::default())
        .unwrap();
    assert!(!blocked.ready);
    assert!(
        blocked
            .blockers
            .iter()
            .any(|blocker| blocker.original == "Core" && blocker.affected_nodes == 2)
    );
    let plan = migration.plan(&fixture.options).unwrap();
    assert!(plan.ready);
    assert_eq!(plan.nodes_scanned, 3);
    assert_eq!(plan.nodes_changed, 3);
    assert_eq!(plan.episode_nodes_changed, 1);
    assert_eq!(plan.collisions[0].replacement, "rust-compiler");
    let before = fixture.original.clone().canonical_value().unwrap();
    migration
        .apply(&fixture.options, &plan.plan_sha256)
        .unwrap();
    let actual = migration.store.export().await.unwrap();
    let mut expected = fixture.original.clone();
    let snapshot_plan =
        crate::tag_format_migration::plan_tag_format_export(&expected, &fixture.options).unwrap();
    crate::tag_format_migration::normalize_tag_format_export(
        &mut expected,
        &fixture.options,
        fixture.db_id,
        &snapshot_plan.plan_sha256,
    )
    .unwrap();
    assert_eq!(
        actual.clone().canonical_value().unwrap(),
        expected.canonical_value().unwrap()
    );
    assert_eq!(actual.touchstones, fixture.original.touchstones);
    // Independent preservation oracle: restoring only the authored tag fields
    // must recover the entire exact pre-migration export, not merely a second
    // result calculated by the same migration implementation.
    let mut restored = actual.clone();
    for node in &mut restored.nodes {
        let original = fixture
            .original
            .nodes
            .iter()
            .find(|old| old.id() == node.id())
            .unwrap();
        node.replace_tags(original.tag_set().clone());
    }
    assert_eq!(restored.canonical_value().unwrap(), before);
    let after = actual.canonical_value().unwrap();
    assert_ne!(before["nodes"], after["nodes"]);
    let membership = migration
        .store
        .run(
            "?[tag,status,sample_hash,id] := *node_tag_v2{tag,status,sample_hash,id}",
            BTreeMap::new(),
            false,
        )
        .unwrap();
    assert_eq!(membership.rows.len(), 3);
    assert!(
        membership
            .rows
            .iter()
            .all(|row| want_str(&row[0]).unwrap() != "cpp")
    );
    assert!(migration.plan(&fixture.options).is_err());
    migration.checkpoint().unwrap();
    let lease = StoreLease::acquire(&fixture.path).unwrap();
    let current = CozoStore::open_existing_current(&fixture.path, lease).unwrap();
    assert_eq!(current.db_id(), fixture.db_id);
    let owner = &fixture.original.touchstones[0];
    let current_owner = current.get_node(owner.owner()).await.unwrap().unwrap();
    assert!(current_owner.has_tag("historical-core"));
    assert!(!current_owner.has_tag("core"));
    let desired = BoundedTagSet::try_from_iter(["replacement"]).unwrap();
    assert!(
        current
            .compare_replace_node_tags(current_owner.id(), current_owner.tag_set(), &desired)
            .await
            .is_err()
    );
    let episode = fixture
        .original
        .nodes
        .iter()
        .find(|node| !node.is_semantic())
        .unwrap();
    let current_episode = current.get_node(episode.id()).await.unwrap().unwrap();
    assert!(current_episode.has_tag("historical-core"));
    assert!(!current_episode.has_tag("core"));
    assert!(
        current
            .compare_replace_node_tags(episode.id(), episode.tag_set(), &desired)
            .await
            .is_err()
    );
    current.prepare_for_file_move().unwrap();
}

#[tokio::test]
async fn tag_format_migration_refuses_bad_plans_and_rolls_back_every_cut() {
    let fixture = fixture().await;
    let mut migration = migration(&fixture);
    let no_aliases = migration
        .plan(&TagFormatMigrationOptions::default())
        .unwrap();
    assert!(!no_aliases.ready);
    let plan = migration.plan(&fixture.options).unwrap();
    assert!(migration.apply(&fixture.options, &"0".repeat(64)).is_err());
    for at in [1, 2, 3, usize::MAX] {
        assert!(
            migration
                .apply_inner(
                    &fixture.options,
                    &plan.plan_sha256,
                    FailureInjection {
                        after_write: Some(at),
                        exit_process: false
                    }
                )
                .is_err()
        );
        assert_eq!(migration.plan(&fixture.options).unwrap(), plan);
        assert_eq!(
            migration
                .store
                .export()
                .await
                .unwrap()
                .canonical_value()
                .unwrap(),
            fixture.original.clone().canonical_value().unwrap()
        );
    }
    migration.checkpoint().unwrap();
}

#[tokio::test]
async fn tag_format_migration_lost_lease_inode_aborts_precommit_without_changes() {
    let fixture = fixture().await;
    let mut migration = migration(&fixture);
    let plan = migration.plan(&fixture.options).unwrap();
    let lock = mneme_store_path::store_lock_path(&fixture.path).unwrap();
    migration.before_commit_hook = Some(Box::new(move || {
        std::fs::remove_file(&lock).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .unwrap();
    }));
    assert!(
        migration
            .apply(&fixture.options, &plan.plan_sha256)
            .is_err()
    );
    assert_eq!(
        migration
            .store
            .read_meta(crate::tag_projection::META_KEY)
            .unwrap()
            .as_deref(),
        Some(crate::tag_projection::LEGACY_META_VALUE)
    );
    assert_eq!(
        migration
            .store
            .export()
            .await
            .unwrap()
            .canonical_value()
            .unwrap(),
        fixture.original.clone().canonical_value().unwrap()
    );
    assert!(
        migration.plan(&fixture.options).is_err(),
        "invalidated namespace authority must not prepare another plan"
    );
    // Do not call production checkpoint through lost path authority. Dropping
    // this disposable handle releases only its original retained descriptor.
}

#[tokio::test]
async fn tag_format_migration_fences_zero_rename_store_and_raw_writes() {
    let dir = std::env::temp_dir().join(format!("mneme-tag-empty-{}", Ulid::new()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.canonicalize().unwrap().join("memory.db");
    let store = CozoStore::open(path.to_str().unwrap(), 4).unwrap();
    let mut invalid = crate::touchstone_tests::target(NodeId(Ulid::new()), "raw caller");
    invalid.replace_tags(BoundedTagSet::try_from_iter(["Uppercase"]).unwrap());
    assert!(store.put_node(&invalid).await.is_err());
    store
        .put_meta(
            crate::tag_projection::META_KEY,
            crate::tag_projection::LEGACY_META_VALUE,
        )
        .unwrap();
    let id = store.db_id();
    store.prepare_for_file_move().unwrap();
    drop(store);
    let lease = StoreLease::acquire(&path).unwrap();
    let mut migration = SqliteTagFormatMigration::open_legacy(&path, id, lease).unwrap();
    let options = TagFormatMigrationOptions::default();
    let plan = migration.plan(&options).unwrap();
    assert_eq!(plan.nodes_changed, 0);
    migration.apply(&options, &plan.plan_sha256).unwrap();
    migration.checkpoint().unwrap();
    let lease = StoreLease::acquire(&path).unwrap();
    let current = CozoStore::open_existing_current(&path, lease).unwrap();
    current.prepare_for_file_move().unwrap();
    drop(current);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[tokio::test]
#[ignore = "subprocess crash helper; called only with an exact disposable source"]
async fn tag_format_forced_exit_child() {
    let path = std::path::PathBuf::from(std::env::var_os("MNEME_TAG_CRASH_PATH").unwrap());
    let db_id = std::env::var("MNEME_TAG_CRASH_DB")
        .unwrap()
        .parse()
        .unwrap();
    let at = std::env::var("MNEME_TAG_CRASH_AT")
        .unwrap()
        .parse()
        .unwrap();
    let lease = StoreLease::acquire(&path).unwrap();
    let mut migration = SqliteTagFormatMigration::open_legacy(&path, db_id, lease).unwrap();
    let options = TagFormatMigrationOptions {
        aliases: BTreeMap::from([
            ("C++".into(), "cpp".into()),
            ("Core".into(), "historical-core".into()),
        ]),
    };
    let plan = migration.plan(&options).unwrap();
    migration
        .apply_inner(
            &options,
            &plan.plan_sha256,
            FailureInjection {
                after_write: Some(at),
                exit_process: true,
            },
        )
        .unwrap();
    panic!("forced exit did not fire");
}

#[tokio::test]
async fn tag_format_crash_never_commits_partial_tags_or_new_marker() {
    for at in [1, usize::MAX] {
        let fixture = fixture().await;
        let original_main = std::fs::read(&fixture.path).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cozo_store::tag_format_migration::tests::tag_format_forced_exit_child",
                "--ignored",
            ])
            .env("MNEME_TAG_CRASH_PATH", &fixture.path)
            .env("MNEME_TAG_CRASH_DB", fixture.db_id.to_string())
            .env("MNEME_TAG_CRASH_AT", at.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(91));
        assert_eq!(std::fs::read(&fixture.path).unwrap(), original_main);
        // Test-only SQLite rollback recovery on this disposable crash residue.
        // Production admission must not infer clean state from these raw bytes;
        // its recovery route retains/restores the reviewed DB/body backup pair.
        let recovered = DbInstance::new("sqlite", &fixture.path, "").unwrap();
        let marker = recovered
            .run_script(
                "?[v] := *meta{k:'tag_projection_v2',v}",
                BTreeMap::new(),
                cozo::ScriptMutability::Immutable,
            )
            .unwrap();
        assert_eq!(
            want_str(&marker.rows[0][0]).unwrap(),
            crate::tag_projection::LEGACY_META_VALUE
        );
        recovered.prepare_sqlite_for_file_move().unwrap();
        drop(recovered);
        let migration = migration(&fixture);
        assert_eq!(
            migration
                .store
                .export()
                .await
                .unwrap()
                .canonical_value()
                .unwrap(),
            fixture.original.clone().canonical_value().unwrap()
        );
        migration.checkpoint().unwrap();
    }
}

#[tokio::test]
#[ignore = "requires MNEME_OLD_TAG_WRITER naming a genuine retained mnemed artifact"]
async fn tag_format_genuine_old_binary_refuses_migrated_store() {
    let old =
        std::env::var_os("MNEME_OLD_TAG_WRITER").expect("supply genuine retained mnemed artifact");
    // Match the real retained artifact's vector space; the witness tests its
    // tag-policy fence, not an unrelated vector-dimension refusal.
    let fixture = empty_fixture(768).await;
    let before = Command::new(&old)
        .arg("--db")
        .arg(&fixture.path)
        .args(["--json", "list", "--limit", "1"])
        .output()
        .unwrap();
    assert!(
        before.status.success(),
        "old artifact must first establish legacy source compatibility: {}",
        String::from_utf8_lossy(&before.stderr)
    );
    let mut migration = migration(&fixture);
    let plan = migration.plan(&fixture.options).unwrap();
    migration
        .apply(&fixture.options, &plan.plan_sha256)
        .unwrap();
    migration.checkpoint().unwrap();
    let before_old_write = std::fs::read(&fixture.path).unwrap();
    let sidecars = |path: &Path| {
        ["-wal", "-shm", "-journal"].map(|suffix| {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            std::fs::read(std::path::PathBuf::from(name)).ok()
        })
    };
    let before_sidecars = sidecars(&fixture.path);
    let after = Command::new(&old)
        .arg("--db")
        .arg(&fixture.path)
        .arg("decay")
        .output()
        .unwrap();
    assert!(!after.status.success());
    let diagnostic = String::from_utf8_lossy(&after.stderr);
    assert!(
        diagnostic.contains("existing_store_admission_failed")
            || diagnostic.contains(
                "episode_context_upgrade_required_or_invalid: exact current admission failed"
            ),
        "old writer must refuse its documented exact source admission, not fail for an unrelated reason: {diagnostic}"
    );
    assert_eq!(std::fs::read(&fixture.path).unwrap(), before_old_write);
    assert_eq!(sidecars(&fixture.path), before_sidecars);
    let lease = StoreLease::acquire(&fixture.path).unwrap();
    let current = CozoStore::open_existing_current(&fixture.path, lease).unwrap();
    current.prepare_for_file_move().unwrap();
}

#[tokio::test]
#[ignore = "requires MNEME_OLD_TAG_MCP naming a genuine retained MCP owner"]
async fn tag_format_genuine_already_admitted_owner_excludes_migration() {
    use std::io::{BufRead, Write};
    let old =
        std::env::var_os("MNEME_OLD_TAG_MCP").expect("supply genuine retained MCP owner artifact");
    let dir = std::env::temp_dir().join(format!("mneme-tag-old-owner-{}", Ulid::new()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.canonicalize().unwrap().join("memory.db");
    // Empty old materialization; fingerprint preparation remains metadata-only.
    // 768 is the actual retained owner's default vector dimension. No query,
    // embedding, provider or recording operation is sent.
    let store = CozoStore::open(path.to_str().unwrap(), 768).unwrap();
    let id = store.db_id();
    store
        .put_meta(
            crate::tag_projection::META_KEY,
            crate::tag_projection::LEGACY_META_VALUE,
        )
        .unwrap();
    store.prepare_for_file_move().unwrap();
    drop(store);
    let mut child = Command::new(&old)
        .args(["--capability-profile", "operator", "--db"])
        .arg(format!("project={}", path.display()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                if send.send(value).is_err() {
                    break;
                }
            }
        }
    });
    let send_request = |child: &mut std::process::Child, value: serde_json::Value| {
        let input = child.stdin.as_mut().unwrap();
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
    };
    let response = |id| {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let value = receive
                .recv_timeout(until.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if value["id"] == id {
                break value;
            }
        }
    };
    send_request(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"tag-format-fixture","version":"1"}}}),
    );
    assert!(response(1)["error"].is_null());
    send_request(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    send_request(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list","arguments":{"db":"project","kind":"nodes","limit":1}}}),
    );
    let listed = response(2);
    assert!(
        listed["error"].is_null() && listed["result"]["isError"] != true,
        "old owner must first be genuinely admitted: {listed}"
    );
    assert!(
        StoreLease::acquire(&path).is_err(),
        "migration must not coexist with an already-admitted genuine old owner"
    );
    send_request(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list","arguments":{"db":"project","kind":"nodes","limit":1}}}),
    );
    assert!(
        response(3)["result"]["isError"] != true,
        "failed migration admission must not evict the old owner"
    );
    drop(child.stdin.take());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= until {
            child.kill().unwrap();
            child.wait().unwrap();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    reader.join().unwrap();
    let lease = StoreLease::acquire(&path).unwrap();
    let mut migration = SqliteTagFormatMigration::open_legacy(&path, id, lease).unwrap();
    let options = TagFormatMigrationOptions::default();
    let plan = migration.plan(&options).unwrap();
    migration.apply(&options, &plan.plan_sha256).unwrap();
    migration.checkpoint().unwrap();
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
