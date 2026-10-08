/*
 *  Copyright 2022, The Cozo Project Authors.
 *
 *  This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 *  If a copy of the MPL was not distributed with this file,
 *  You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 */

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use itertools::Itertools;
use log::debug;
use serde_json::json;
use smartstring::{LazyCompact, SmartString};

use crate::data::expr::Expr;
use crate::data::symb::Symbol;
use crate::data::value::DataValue;
use crate::fixed_rule::FixedRulePayload;
use crate::fts::{TokenizerCache, TokenizerConfig};
use crate::parse::SourceSpan;
use crate::runtime::callback::CallbackOp;
#[cfg(feature = "storage-rocksdb")]
use crate::runtime::db::HnswBuildTestFailPoint;
use crate::runtime::db::{Poison, TransactionPayload};
use crate::runtime::hnsw::take_last_hnsw_build_stats;
use crate::{
    DbInstance, FixedRule, PrimaryKeyScan, PrimaryKeyScanBound, PrimaryKeyScanDirection,
    RegularTempStore, ScriptMutability, MAX_PRIMARY_KEY_SCAN_ROWS,
};

#[test]
fn access_level_imperative_is_a_write_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("imperative-access.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create access_guard {id: Int}").unwrap();

    let error = db
        .run_script(
            "{::access_level read_only access_guard}",
            Default::default(),
            ScriptMutability::Immutable,
        )
        .expect_err("an access-only imperative must be classified as a write");
    assert!(
        format!("{error:?}").contains("write locks"),
        "unexpected immutable access error: {error:?}"
    );

    let error = db
        .run_default(
            "{::access_level read_only access_guard}\n\
             {?[must_be_empty] <- [[1]] :assert none}",
        )
        .expect_err("a later failure must abort the access change");
    assert!(format!("{error:?}").contains("assert"), "{error:?}");
    drop(db);

    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default("?[id] <- [[1]] :put access_guard {id}")
        .expect("failed imperative durably leaked the read-only catalogue change");

    db.run_default("{::access_level read_only access_guard}")
        .unwrap();
    drop(db);

    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    assert!(
        db.run_default("?[id] <- [[1]] :put access_guard {id}")
            .is_err(),
        "the mutable imperative did not persist its access change"
    );
}

#[test]
fn set_triggers_imperative_is_a_write_transaction() {
    let db = DbInstance::default();
    db.run_default(":create trigger_guard {id: Int}").unwrap();
    let script = "{::set_triggers trigger_guard on put { ?[id] := _new[id] }}";

    let error = db
        .run_script(script, Default::default(), ScriptMutability::Immutable)
        .expect_err("a trigger-only imperative must be classified as a write");
    assert!(
        format!("{error:?}").contains("write locks"),
        "unexpected immutable trigger error: {error:?}"
    );

    db.run_default(script).unwrap();
    assert_eq!(
        db.run_default("::show_triggers trigger_guard")
            .unwrap()
            .rows
            .len(),
        1,
        "mutable imperative did not persist its trigger"
    );
}

#[test]
fn imperative_commit_failure_uses_the_shared_rollback_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("imperative-commit.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create imperative_commit {id: Int}")
        .unwrap();
    db.fail_next_commit_for_tests();
    let error = db
        .run_default("{?[id] <- [[1]] :put imperative_commit {id}}")
        .expect_err("the injected imperative commit must fail");
    assert!(
        format!("{error:?}").contains("injected commit failure"),
        "unexpected injected commit error: {error:?}"
    );
    drop(db);

    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    assert!(
        db.run_default("?[id] := *imperative_commit{id}")
            .unwrap()
            .rows
            .is_empty(),
        "failed imperative commit leaked a row"
    );
}

#[test]
fn imperative_abort_hook_crashes_before_sqlite_commit() {
    const CHILD_PATH: &str = "MNESTIC_IMPERATIVE_ABORT_CHILD_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
        db.abort_after_imperative_statement_for_tests(1);
        let _ = db.run_default(
            "{?[id] <- [[1]] :put imperative_abort {id}}\n\
             {?[id] <- [[2]] :put imperative_abort {id}}",
        );
        panic!("imperative abort hook returned instead of terminating the process");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("imperative-abort.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create imperative_abort {id: Int}")
        .unwrap();
    drop(db);

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("runtime::tests::imperative_abort_hook_crashes_before_sqlite_commit")
        .arg("--nocapture")
        .env(CHILD_PATH, &path)
        .status()
        .unwrap();
    assert!(!status.success(), "abort-hook child unexpectedly succeeded");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            status.signal().is_some(),
            "abort-hook child exited normally instead of by signal: {status:?}"
        );
    }

    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    assert!(
        db.run_default("?[id] := *imperative_abort{id}")
            .unwrap()
            .rows
            .is_empty(),
        "process abort after statement one committed a partial imperative transaction"
    );
}

#[test]
fn rename_relation_rewrites_every_index_child() {
    let db = DbInstance::default();
    db.run_default(
        r#"
        :create indexed {
            id: Int =>
            text: String,
            embedding: <F32; 2>,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ?[id, text, embedding] <- [[1, 'aaaaaaaaaaaaaaaaaaaa', vec([1.0, 0.0])]]
        :put indexed {id => text, embedding}
        "#,
    )
    .unwrap();
    db.run_default("::index create indexed:by_text {text}")
        .unwrap();
    db.run_default(
        r#"
        ::hnsw create indexed:semantic {
            dim: 2,
            fields: embedding,
            distance: Cosine,
            m: 4,
            ef: 16,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::fts create indexed:search {
            extractor: text,
            tokenizer: Simple,
            filters: [Lowercase],
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::lsh create indexed:similar {
            extractor: text,
            tokenizer: NGram,
            n_gram: 3,
            n_perm: 32,
            target_threshold: 0.3,
        }
        "#,
    )
    .unwrap();

    db.run_default("::rename indexed -> renamed").unwrap();

    for child in [
        "renamed:by_text",
        "renamed:semantic",
        "renamed:search",
        "renamed:similar",
        "renamed:similar:inv",
    ] {
        db.run_default(&format!("::columns {child}"))
            .unwrap_or_else(|err| panic!("renamed child {child} is missing: {err:?}"));
    }
    for old_child in [
        "indexed:by_text",
        "indexed:semantic",
        "indexed:search",
        "indexed:similar",
        "indexed:similar:inv",
    ] {
        assert!(db.run_default(&format!("::columns {old_child}")).is_err());
    }

    db.run_default(
        r#"
        ?[id, text, embedding] <- [[2, 'bcdefghijklmnopqrstuv', vec([0.0, 1.0])]]
        :put renamed {id => text, embedding}
        "#,
    )
    .unwrap();
    assert_eq!(
        db.run_default("?[id] := *renamed:by_text{text: 'bcdefghijklmnopqrstuv', id}")
            .unwrap()
            .into_json()["rows"],
        json!([[2]])
    );
    assert_eq!(
        db.run_default(
            "?[id] := ~renamed:semantic{id | query: vec([0.0, 1.0]), k: 1, ef: 16}",
        )
        .unwrap()
        .into_json()["rows"],
        json!([[2]])
    );
    assert_eq!(
        db.run_default("?[id] := ~renamed:search{id | query: 'bcdefghijklmnopqrstuv', k: 1}")
            .unwrap()
            .into_json()["rows"],
        json!([[2]])
    );
    assert_eq!(
        db.run_default(
            "?[id] := ~renamed:similar{id | query: 'bcdefghijklmnopqrstuv', k: 1}",
        )
        .unwrap()
        .into_json()["rows"],
        json!([[2]])
    );

    db.run_default("::index drop renamed:by_text").unwrap();
    db.run_default("::hnsw drop renamed:semantic").unwrap();
    db.run_default("::fts drop renamed:search").unwrap();
    db.run_default("::lsh drop renamed:similar").unwrap();
    db.run_default("::remove renamed").unwrap();
}

#[test]
fn rename_relation_preflights_destination_conflicts() {
    let db = DbInstance::default();
    db.run_default(":create source {id: Int => value: String}")
        .unwrap();
    db.run_default("::index create source:by_value {value}")
        .unwrap();
    db.run_default(":create target {id: Int}").unwrap();

    assert!(db.run_default("::rename source -> target").is_err());
    db.run_default("::columns source").unwrap();
    db.run_default("::columns source:by_value").unwrap();
    db.run_default("::columns target").unwrap();
}

#[test]
fn rename_relation_supports_indexed_projection_swap() {
    let db = DbInstance::default();
    for relation in ["node_vec", "shadow_node_vec"] {
        db.run_default(&format!(
            ":create {relation} {{id: Int => e: <F32; 2>, status: String}}"
        ))
        .unwrap();
        for (index, status) in [
            ("active_idx", "active"),
            ("candidate_idx", "candidate"),
            ("archived_idx", "archived"),
        ] {
            db.run_default(&format!(
                r#"
                ::hnsw create {relation}:{index} {{
                    dim: 2,
                    fields: e,
                    distance: Cosine,
                    m: 4,
                    ef: 16,
                    filter: status == '{status}',
                }}
                "#
            ))
            .unwrap();
        }
    }
    db.run_default(
        "?[id, e, status] <- [[1, vec([1.0, 0.0]), 'active']] :put node_vec {id => e, status}",
    )
    .unwrap();
    db.run_default(
        "?[id, e, status] <- [[2, vec([0.0, 1.0]), 'active']] :put shadow_node_vec {id => e, status}",
    )
    .unwrap();

    let before = db
        .run_default("::relations")
        .unwrap()
        .rows
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect::<BTreeMap<_, _>>();

    db.run_default("::rename node_vec -> old_node_vec, shadow_node_vec -> node_vec")
        .unwrap();

    let after = db
        .run_default("::relations")
        .unwrap()
        .rows
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect::<BTreeMap<_, _>>();
    for index in ["active_idx", "candidate_idx", "archived_idx"] {
        assert_eq!(
            before[&DataValue::from(format!("node_vec:{index}"))],
            after[&DataValue::from(format!("old_node_vec:{index}"))]
        );
        assert_eq!(
            before[&DataValue::from(format!("shadow_node_vec:{index}"))],
            after[&DataValue::from(format!("node_vec:{index}"))]
        );
    }
    assert_eq!(
        db.run_default(
            "?[id] := ~node_vec:active_idx{id | query: vec([0.0, 1.0]), k: 1, ef: 16}",
        )
        .unwrap()
        .into_json()["rows"],
        json!([[2]])
    );
    assert_eq!(
        db.run_default(
            "?[id] := ~old_node_vec:active_idx{id | query: vec([1.0, 0.0]), k: 1, ef: 16}",
        )
        .unwrap()
        .into_json()["rows"],
        json!([[1]])
    );
}

#[test]
fn rename_relation_invalidates_fts_doc_stats_across_indexed_swap() {
    fn score(db: &DbInstance, index: &str) -> f64 {
        db.run_default(&format!(
            "?[id, score] := ~{index}{{id | query: 'needle', k: 1, bind_score: score}}"
        ))
        .unwrap()
        .rows[0][1]
            .get_float()
            .unwrap()
    }

    let db = DbInstance::default();
    for relation in ["short_corpus", "long_corpus"] {
        db.run_default(&format!(
            ":create {relation} {{id: Int => text: String}}"
        ))
        .unwrap();
    }
    db.run_default(
        "?[id, text] <- [[1, 'needle'], [2, 'filler']] :put short_corpus {id => text}",
    )
    .unwrap();
    db.run_default(
        "?[id, text] <- [[1, 'needle'], [2, 'filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler filler']] :put long_corpus {id => text}",
    )
    .unwrap();
    for relation in ["short_corpus", "long_corpus"] {
        db.run_default(&format!(
            "::fts create {relation}:search {{extractor: text, tokenizer: Simple}}"
        ))
        .unwrap();
    }

    let short_score = score(&db, "short_corpus:search");
    let long_score = score(&db, "long_corpus:search");
    assert_ne!(short_score.to_bits(), long_score.to_bits());

    db.run_default(
        "::rename short_corpus -> old_short_corpus, long_corpus -> short_corpus",
    )
    .unwrap();

    assert_eq!(
        score(&db, "short_corpus:search").to_bits(),
        long_score.to_bits(),
        "the destination name must not retain the displaced index's avgdl cache"
    );
    assert_eq!(
        score(&db, "old_short_corpus:search").to_bits(),
        short_score.to_bits()
    );
}

#[test]
fn test_limit_offset() {
    let db = DbInstance::default();
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([[3], [5]]));
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2 :offset 1")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([[1], [3]]));
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2 :offset 4")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([[4]]));
    let res = db
        .run_default("?[a] := a in [5,3,1,2,4] :limit 2 :offset 5")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], json!([]));
}

#[test]
fn test_normal_aggr_empty() {
    let db = DbInstance::default();
    let res = db.run_default("?[count(a)] := a in []").unwrap().rows;
    assert_eq!(res, vec![vec![DataValue::from(0)]]);
}

#[test]
fn test_meet_aggr_empty() {
    let db = DbInstance::default();
    let res = db.run_default("?[min(a)] := a in []").unwrap().rows;
    assert_eq!(res, vec![vec![DataValue::Null]]);

    let res = db
        .run_default("?[min(a), count(a)] := a in []")
        .unwrap()
        .rows;
    assert_eq!(res, vec![vec![DataValue::Null, DataValue::from(0)]]);
}

#[test]
fn test_layers() {
    let _ = env_logger::builder().is_test(true).try_init();

    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
        y[a] := a in [1,2,3]
        x[sum(a)] := y[a]
        x[sum(a)] := a in [4,5,6]
        ?[sum(a)] := x[a]
        "#,
        )
        .unwrap()
        .rows;
    assert_eq!(res[0][0], DataValue::from(21.))
}

#[test]
fn test_conditions() {
    let _ = env_logger::builder().is_test(true).try_init();
    let db = DbInstance::default();
    db.run_default(
        r#"
        {
            ?[code] <- [['a'],['b'],['c']]
            :create airport {code}
        }
        {
            ?[fr, to, dist] <- [['a', 'b', 1.1], ['a', 'c', 0.5], ['b', 'c', 9.1]]
            :create route {fr, to => dist}
        }
        "#,
    )
    .unwrap();
    debug!("real test begins");
    let res = db
        .run_default(
            r#"
        r[code, dist] := *airport{code}, *route{fr: code, dist};
        ?[dist] := r['a', dist], dist > 0.5, dist <= 1.1;
        "#,
        )
        .unwrap()
        .rows;
    assert_eq!(res[0][0], DataValue::from(1.1))
}

#[test]
fn test_classical() {
    let _ = env_logger::builder().is_test(true).try_init();
    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
parent[] <- [['joseph', 'jakob'],
             ['jakob', 'isaac'],
             ['isaac', 'abraham']]
grandparent[gcld, gp] := parent[gcld, p], parent[p, gp]
?[who] := grandparent[who, 'abraham']
        "#,
        )
        .unwrap()
        .rows;
    println!("{:?}", res);
    assert_eq!(res[0][0], DataValue::from("jakob"))
}

#[test]
fn default_columns() {
    let db = DbInstance::default();

    db.run_default(
        r#"
            :create status {uid: String, ts default now() => quitted: Bool, mood: String}
            "#,
    )
    .unwrap();

    db.run_default(
        r#"
        ?[uid, quitted, mood] <- [['z', true, 'x']]
            :put status {uid => quitted, mood}
        "#,
    )
    .unwrap();
}

#[test]
fn rm_does_not_need_all_keys() {
    let db = DbInstance::default();
    db.run_default(":create status {uid => mood}").unwrap();
    assert!(db
        .run_default("?[uid, mood] <- [[1, 2]] :put status {uid => mood}",)
        .is_ok());
    assert!(db
        .run_default("?[uid, mood] <- [[2]] :put status {uid}",)
        .is_err());
    assert!(db
        .run_default("?[uid, mood] <- [[3, 2]] :rm status {uid => mood}",)
        .is_ok());
    assert!(db.run_default("?[uid] <- [[1]] :rm status {uid}").is_ok());
}

#[test]
fn strict_checks_for_fixed_rules_args() {
    let db = DbInstance::default();
    let res = db.run_default(
        r#"
            r[] <- [[1, 2]]
            ?[] <~ PageRank(r[_, _])
        "#,
    );
    println!("{:?}", res);
    assert!(res.is_ok());

    let db = DbInstance::default();
    let res = db.run_default(
        r#"
            r[] <- [[1, 2]]
            ?[] <~ PageRank(r[a, b])
        "#,
    );
    assert!(res.is_ok());

    let db = DbInstance::default();
    let res = db.run_default(
        r#"
            r[] <- [[1, 2]]
            ?[] <~ PageRank(r[a, a])
        "#,
    );
    assert!(res.is_err());
}

#[test]
fn do_not_unify_underscore() {
    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
        r1[] <- [[1, 'a'], [2, 'b']]
        r2[] <- [[2, 'B'], [3, 'C']]

        ?[l1, l2] := r1[_ , l1], r2[_ , l2]
        "#,
        )
        .unwrap()
        .rows;
    assert_eq!(res.len(), 4);

    let res = db.run_default(
        r#"
        ?[_] := _ = 1
        "#,
    );
    assert!(res.is_err());

    let res = db
        .run_default(
            r#"
        ?[x] := x = 1, _ = 1, _ = 2
        "#,
        )
        .unwrap()
        .rows;

    assert_eq!(res.len(), 1);
}

#[test]
fn imperative_script() {
    // let db = DbInstance::default();
    // let res = db
    //     .run_default(
    //         r#"
    //     {:create _test {a}}
    //
    //     %loop
    //         %if { len[count(x)] := *_test[x]; ?[x] := len[z], x = z >= 10 }
    //             %then %return _test
    //         %end
    //         { ?[a] := a = rand_uuid_v1(); :put _test {a} }
    //         %debug _test
    //     %end
    // "#,
    //         Default::default(),
    //     )
    //     .unwrap();
    // assert_eq!(res.rows.len(), 10);
    //
    // let res = db
    //     .run_default(
    //         r#"
    //     {?[a] <- [[1], [2], [3]]
    //      :replace _test {a}}
    //
    //     %loop
    //         { ?[a] := *_test[a]; :limit 1; :rm _test {a} }
    //         %debug _test
    //
    //         %if_not _test
    //         %then %break
    //         %end
    //     %end
    //
    //     %return _test
    // "#,
    //         Default::default(),
    //     )
    //     .unwrap();
    // assert_eq!(res.rows.len(), 0);
    //
    // let res = db.run_default(
    //     r#"
    //     {:create _test {a}}
    //
    //     %loop
    //         { ?[a] := a = rand_uuid_v1(); :put _test {a} }
    //
    //         %if { len[count(x)] := *_test[x]; ?[x] := len[z], x = z < 10 }
    //             %continue
    //         %end
    //
    //         %return _test
    //         %debug _test
    //     %end
    // "#,
    //     Default::default(),
    // );
    // if let Err(err) = &res {
    //     eprintln!("{err:?}");
    // }
    // assert_eq!(res.unwrap().rows.len(), 10);
    //
    // let res = db
    //     .run_default(
    //         r#"
    //     {?[a] <- [[1], [2], [3]]
    //      :replace _test {a}}
    //     {?[a] <- []
    //      :replace _test2 {a}}
    //     %swap _test _test2
    //     %return _test
    // "#,
    //         Default::default(),
    //     )
    //     .unwrap();
    // assert_eq!(res.rows.len(), 0);
}

#[test]
fn returning_relations() {
    let db = DbInstance::default();
    let res = db
        .run_default(
            r#"
        {:create _xxz {a}}
        {?[a] := a in [5,4,1,2,3] :put _xxz {a}}
        {?[a] := *_xxz[a], a % 2 == 0 :rm _xxz {a}}
        {?[a] := *_xxz[b], a = b * 2}
        "#,
        )
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[2], [6], [10]]));
    let res = db.run_default(
        r#"
        {?[a] := *_xxz[b], a = b * 2}
        "#,
    );
    assert!(res.is_err());
}

#[test]
fn test_trigger() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();
    db.run_default(":create friends.rev {to: Int, fr: Int => data: Any}")
        .unwrap();
    db.run_default(
        r#"
        ::set_triggers friends

        on put {
            ?[fr, to, data] := _new[fr, to, data]

            :put friends.rev{ to, fr => data}
        }
        on rm {
            ?[fr, to] := _old[fr, to, data]

            :rm friends.rev{ to, fr }
        }
        "#,
    )
    .unwrap();
    db.run_default(r"?[fr, to, data] <- [[1,2,3]] :put friends {fr, to => data}")
        .unwrap();
    let ret = db
        .export_relations(["friends", "friends.rev"].into_iter())
        .unwrap();
    let frs = ret.get("friends").unwrap();
    assert_eq!(
        vec![DataValue::from(1), DataValue::from(2), DataValue::from(3)],
        frs.rows[0]
    );

    let frs_rev = ret.get("friends.rev").unwrap();
    assert_eq!(
        vec![DataValue::from(2), DataValue::from(1), DataValue::from(3)],
        frs_rev.rows[0]
    );
    db.run_default(r"?[fr, to] <- [[1,2], [2,3]] :rm friends {fr, to}")
        .unwrap();
    let ret = db
        .export_relations(["friends", "friends.rev"].into_iter())
        .unwrap();
    let frs = ret.get("friends").unwrap();
    assert!(frs.rows.is_empty());
}

#[test]
fn test_callback() {
    let db = DbInstance::default();
    let mut collected = vec![];
    let (_id, receiver) = db.register_callback("friends", None);
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();
    db.run_default(r"?[fr, to, data] <- [[1,2,3],[4,5,6]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to, data] <- [[1,2,4],[4,7,6]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to] <- [[1,9],[4,5]] :rm friends {fr, to}")
        .unwrap();
    std::thread::sleep(Duration::from_secs_f64(0.01));
    while let Ok(d) = receiver.try_recv() {
        collected.push(d);
    }
    let collected = collected;
    assert_eq!(collected[0].0, CallbackOp::Put);
    assert_eq!(collected[0].1.rows.len(), 2);
    assert_eq!(collected[0].1.rows[0].len(), 3);
    assert_eq!(collected[0].2.rows.len(), 0);
    assert_eq!(collected[1].0, CallbackOp::Put);
    assert_eq!(collected[1].1.rows.len(), 2);
    assert_eq!(collected[1].1.rows[0].len(), 3);
    assert_eq!(collected[1].2.rows.len(), 1);
    assert_eq!(
        collected[1].2.rows[0],
        vec![DataValue::from(1), DataValue::from(2), DataValue::from(3)]
    );
    assert_eq!(collected[2].0, CallbackOp::Rm);
    assert_eq!(collected[2].1.rows.len(), 2);
    assert_eq!(collected[2].1.rows[0].len(), 2);
    assert_eq!(collected[2].2.rows.len(), 1);
    assert_eq!(collected[2].2.rows[0].len(), 3);
}

#[test]
fn test_update() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => a: Any, b: Any, c: Any}")
        .unwrap();
    db.run_default("?[fr, to, a, b, c] <- [[1,2,3,4,5]] :put friends {fr, to => a, b, c}")
        .unwrap();
    let res = db
        .run_default("?[fr, to, a, b, c] := *friends{fr, to, a, b, c}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0], json!([1, 2, 3, 4, 5]));
    db.run_default("?[fr, to, b] <- [[1, 2, 100]] :update friends {fr, to => b}")
        .unwrap();
    let res = db
        .run_default("?[fr, to, a, b, c] := *friends{fr, to, a, b, c}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0], json!([1, 2, 3, 100, 5]));
}

#[test]
fn test_index() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,3],[4,5,6]] :put friends {fr, to, data}")
        .unwrap();

    assert!(db
        .run_default("::index create friends:rev {to, no}")
        .is_err());
    db.run_default("::index create friends:rev {to, data}")
        .unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,5],[6,5,7]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to] <- [[4,5]] :rm friends {fr, to}")
        .unwrap();

    let rels_data = db
        .export_relations(["friends", "friends:rev"].into_iter())
        .unwrap();
    assert_eq!(
        rels_data["friends"].clone().into_json()["rows"],
        json!([[1, 2, 5], [6, 5, 7]])
    );
    assert_eq!(
        rels_data["friends:rev"].clone().into_json()["rows"],
        json!([[2, 5, 1], [5, 7, 6]])
    );

    let rels = db.run_default("::relations").unwrap();
    assert_eq!(rels.rows[1][0], DataValue::from("friends:rev"));
    assert_eq!(rels.rows[1][1], DataValue::from(3));
    assert_eq!(rels.rows[1][2], DataValue::from("index"));

    let cols = db.run_default("::columns friends:rev").unwrap();
    assert_eq!(cols.rows.len(), 3);

    let res = db
        .run_default("?[fr, data] := *friends:rev{to: 2, fr, data}")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 5]]));

    let res = db
        .run_default("?[fr, data] := *friends{to: 2, fr, data}")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 5]]));

    let expl = db
        .run_default("::explain { ?[fr, data] := *friends{to: 2, fr, data} }")
        .unwrap();
    let joins = expl.into_json()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_array().unwrap()[5].clone())
        .collect_vec();
    assert!(joins.contains(&json!(":friends:rev")));
    db.run_default("::index drop friends:rev").unwrap();
}

#[test]
fn test_json_objects() {
    let db = DbInstance::default();
    db.run_default("?[a] := a = {'a': 1}").unwrap();
    db.run_default(
        r"?[a] := a = {
            'a': 1
        }",
    )
    .unwrap();
}

#[test]
fn test_custom_rules() {
    let db = DbInstance::default();
    struct Custom;

    impl FixedRule for Custom {
        fn arity(
            &self,
            _options: &BTreeMap<SmartString<LazyCompact>, Expr>,
            _rule_head: &[Symbol],
            _span: SourceSpan,
        ) -> miette::Result<usize> {
            Ok(1)
        }

        fn run(
            &self,
            payload: FixedRulePayload<'_, '_>,
            out: &'_ mut RegularTempStore,
            _poison: Poison,
        ) -> miette::Result<()> {
            let rel = payload.get_input(0)?;
            let mult = payload.integer_option("mult", Some(2))?;
            for maybe_row in rel.iter()? {
                let row = maybe_row?;
                let mut sum = 0;
                for col in row {
                    let d = col.get_int().unwrap_or(0);
                    sum += d;
                }
                sum *= mult;
                out.put(vec![DataValue::from(sum)])
            }
            Ok(())
        }
    }

    db.register_fixed_rule("SumCols".to_string(), Custom)
        .unwrap();
    let res = db
        .run_default(
            r#"
        rel[] <- [[1,2,3,4],[5,6,7,8]]
        ?[x] <~ SumCols(rel[], mult: 100)
    "#,
        )
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1000], [2600]]));
}

#[test]
fn test_index_short() {
    let db = DbInstance::default();
    db.run_default(":create friends {fr: Int, to: Int => data: Any}")
        .unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,3],[4,5,6]] :put friends {fr, to => data}")
        .unwrap();

    db.run_default("::index create friends:rev {to}").unwrap();

    db.run_default(r"?[fr, to, data] <- [[1,2,5],[6,5,7]] :put friends {fr, to => data}")
        .unwrap();
    db.run_default(r"?[fr, to] <- [[4,5]] :rm friends {fr, to}")
        .unwrap();

    let rels_data = db
        .export_relations(["friends", "friends:rev"].into_iter())
        .unwrap();
    assert_eq!(
        rels_data["friends"].clone().into_json()["rows"],
        json!([[1, 2, 5], [6, 5, 7]])
    );
    assert_eq!(
        rels_data["friends:rev"].clone().into_json()["rows"],
        json!([[2, 1], [5, 6]])
    );

    let rels = db.run_default("::relations").unwrap();
    assert_eq!(rels.rows[1][0], DataValue::from("friends:rev"));
    assert_eq!(rels.rows[1][1], DataValue::from(2));
    assert_eq!(rels.rows[1][2], DataValue::from("index"));

    let cols = db.run_default("::columns friends:rev").unwrap();
    assert_eq!(cols.rows.len(), 2);

    let expl = db
        .run_default("::explain { ?[fr, data] := *friends{to: 2, fr, data} }")
        .unwrap()
        .into_json();

    for row in expl["rows"].as_array().unwrap() {
        println!("{}", row);
    }

    let joins = expl["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_array().unwrap()[5].clone())
        .collect_vec();
    assert!(joins.contains(&json!(":friends:rev")));

    let res = db
        .run_default("?[fr, data] := *friends{to: 2, fr, data}")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 5]]));
}

#[test]
fn test_multi_tx() {
    let db = DbInstance::default();
    let tx = db.multi_transaction(true);
    tx.run_script(":create a {a}", Default::default()).unwrap();
    tx.run_script("?[a] <- [[1]] :put a {a}", Default::default())
        .unwrap();
    assert!(tx.run_script(":create a {a}", Default::default()).is_err());
    tx.run_script("?[a] <- [[2]] :put a {a}", Default::default())
        .unwrap();
    tx.run_script("?[a] <- [[3]] :put a {a}", Default::default())
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.run_default("?[a] := *a[a]").unwrap().into_json()["rows"],
        json!([[1], [2], [3]])
    );

    let db = DbInstance::default();
    let tx = db.multi_transaction(true);
    tx.run_script(":create a {a}", Default::default()).unwrap();
    tx.run_script("?[a] <- [[1]] :put a {a}", Default::default())
        .unwrap();
    assert!(tx.run_script(":create a {a}", Default::default()).is_err());
    tx.run_script("?[a] <- [[2]] :put a {a}", Default::default())
        .unwrap();
    tx.run_script("?[a] <- [[3]] :put a {a}", Default::default())
        .unwrap();
    tx.abort().unwrap();
    assert!(db.run_default("?[a] := *a[a]").is_err());
}

#[test]
fn catalog_system_operations_commit_and_late_abort_with_the_outer_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-system-commit.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    let tx = db.multi_transaction(true);
    tx.run_script(
        ":create indexed {id: Int => value: String}",
        Default::default(),
    )
    .unwrap();
    tx.run_sqlite_catalog_system_operation("::index create indexed:by_value {value}")
        .unwrap();
    tx.commit().unwrap();
    let indices = db.run_default("::indices indexed").unwrap();
    assert!(indices.into_json()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row.as_array().unwrap()[0] == json!("by_value")));

    let tx = db.multi_transaction(true);
    tx.run_script(":create rolled_back {id: Int}", Default::default())
        .unwrap();
    tx.run_sqlite_catalog_system_operation("::access_level read_only rolled_back")
        .unwrap();
    tx.abort().unwrap();
    assert!(db.run_default("::columns rolled_back").is_err());
}

#[test]
#[cfg(feature = "storage-sqlite")]
fn rolled_back_fts_creation_does_not_poison_same_name_reuse() {
    for fail_commit in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(if fail_commit {
            "fts-cache-failed-commit.db"
        } else {
            "fts-cache-abort.db"
        });
        let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
        db.run_default(":create docs {id: Int => body: String}")
            .unwrap();
        db.run_default("?[id, body] <- [[1, 'THE NEEDLE']] :put docs {id => body}")
            .unwrap();

        // Config A publishes both process caches while its storage writes are
        // still private. Roll it back after the sysop itself succeeded.
        let tx = db.multi_transaction(true);
        tx.run_sqlite_catalog_system_operation(
            "::fts create docs:search { extractor: body, tokenizer: Simple }",
        )
        .unwrap();
        if fail_commit {
            db.fail_next_commit_for_tests();
            let error = tx
                .commit()
                .expect_err("the injected storage commit must fail");
            assert!(
                format!("{error:?}").contains("injected commit failure"),
                "{error:?}"
            );
        } else {
            tx.abort().unwrap();
        }

        // Reuse the exact child name with config B. Without transaction-local
        // rollback, the name cache returns A's analyzer: the durable postings
        // remain uppercase and retain the stop word.
        let tx = db.multi_transaction(true);
        tx.run_sqlite_catalog_system_operation(
            "::fts create docs:search { \
             extractor: body, tokenizer: Simple, filters: [Lowercase, Stopwords('en')] \
             }",
        )
        .unwrap();
        tx.commit().unwrap();

        // Ordinary successful creation must retain both publications.
        let DbInstance::Sqlite(inner) = &db else {
            unreachable!("fixture explicitly opened SQLite")
        };
        {
            let inspector = inner.transact().unwrap();
            assert!(
                inspector
                    .tokenizers
                    .named_cache
                    .read()
                    .unwrap()
                    .contains_key("docs:search"),
                "successful FTS commit discarded its named analyzer"
            );
            assert_eq!(
                inspector
                    .fts_doc_stats_cache
                    .lock()
                    .unwrap()
                    .get("docs:search")
                    .copied(),
                Some((1, 1)),
                "successful FTS commit discarded or poisoned its corpus stats"
            );
        }

        // Reopen to discard every process cache. The lowercased posting proves
        // config B, rather than the aborted analyzer, built durable storage.
        drop(db);
        let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
        assert_eq!(
            db.run_default("?[word] := *docs:search{word}")
                .unwrap()
                .into_json()["rows"],
            json!([["needle"]])
        );
        assert_eq!(
            db.run_default("?[id] := ~docs:search{id | query: 'needle', k: 10}")
                .unwrap()
                .into_json()["rows"],
            json!([[1]])
        );
    }
}

#[test]
fn non_sqlite_fts_abort_cleanup_cannot_delete_another_publication() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create docs {id: Int => body: String}")
        .unwrap();
    db.run_default("?[id, body] <- [[1, 'needle']] :put docs {id => body}")
        .unwrap();
    db.run_default("::fts create docs:search { extractor: body, tokenizer: Simple }")
        .unwrap();

    let DbInstance::Mem(inner) = &db else {
        unreachable!("fixture explicitly opened the mem backend")
    };
    let inspector = inner.transact().unwrap();
    let analyzer = inspector
        .tokenizers
        .named_cache
        .read()
        .unwrap()
        .get("docs:search")
        .cloned()
        .expect("committed publication must retain its named analyzer");
    let stats = inspector
        .fts_doc_stats_cache
        .lock()
        .unwrap()
        .get("docs:search")
        .copied()
        .expect("committed publication must retain its corpus stats");
    drop(inspector);

    {
        let mut aborted = inner.transact_write().unwrap();
        assert!(
            !aborted.fts_abort_cache_cleanup.is_sqlite_name_only(),
            "non-SQLite transaction armed name-only cleanup"
        );
        // Exercise the same arming call used by FTS creation. Disabled means
        // Drop cannot delete a cache entry published by some other commit.
        aborted
            .fts_abort_cache_cleanup
            .arm(SmartString::from("docs:search"));
    }

    let inspector = inner.transact().unwrap();
    let retained = inspector
        .tokenizers
        .named_cache
        .read()
        .unwrap()
        .get("docs:search")
        .cloned()
        .expect("disabled abort cleanup deleted another analyzer publication");
    assert!(std::sync::Arc::ptr_eq(&analyzer, &retained));
    assert_eq!(
        inspector
            .fts_doc_stats_cache
            .lock()
            .unwrap()
            .get("docs:search")
            .copied(),
        Some(stats),
        "disabled abort cleanup altered another corpus-stats publication"
    );
}

#[test]
fn catalog_system_operation_failure_terminally_rolls_back_prior_and_partial_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-system-failure.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create durable {id: Int}").unwrap();

    let tx = db.multi_transaction(true);
    tx.run_script("?[id] <- [[1]] :put durable {id}", Default::default())
        .unwrap();
    let error = tx
        .run_sqlite_catalog_system_operation(
            "::access_level read_only durable, missing_relation",
        )
        .expect_err("later access-level refusal must be terminal");
    assert!(
        format!("{error:?}").contains("missing_relation"),
        "{error:?}"
    );
    assert!(
        tx.commit().is_err(),
        "terminal catalog failure left commit reachable"
    );
    assert!(
        db.run_default("?[id] := *durable{id}")
            .unwrap()
            .rows
            .is_empty(),
        "work staged before the catalog failure committed"
    );
    db.run_default("?[id] <- [[2]] :put durable {id}")
        .expect("partial access-level change escaped terminal rollback");
}

#[test]
fn catalog_system_operation_refuses_read_only_non_sys_multiple_and_unallowlisted_payloads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-system-refusals.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create guarded {id: Int}").unwrap();

    let tx = db.multi_transaction(false);
    let error = tx
        .run_sqlite_catalog_system_operation("::access_level read_only guarded")
        .expect_err("read-only transaction must refuse catalog mutation");
    assert!(format!("{error:?}").contains("write multi-transaction"));
    assert!(tx.commit().is_err(), "read-only refusal was not terminal");

    for payload in [
        ":create escaped {id: Int}",
        "{::access_level read_only guarded}\n{::access_level normal guarded}",
        "::relations",
    ] {
        let tx = db.multi_transaction(true);
        tx.run_sqlite_catalog_system_operation(payload)
            .expect_err("non-singleton or unallowlisted payload must refuse");
        assert!(tx.commit().is_err(), "payload refusal was not terminal");
    }
    db.run_default("?[id] <- [[1]] :put guarded {id}")
        .expect("refused catalog payload changed durable access");
    assert!(db.run_default("::columns escaped").is_err());
}

#[test]
fn catalog_system_operation_refuses_non_sqlite_backends_terminally() {
    let db = DbInstance::default();
    let tx = db.multi_transaction(true);
    let error = tx
        .run_sqlite_catalog_system_operation("::access_level read_only anything")
        .expect_err("the catalog multi-transaction seam is SQLite-only");
    assert!(format!("{error:?}").contains("supported only"), "{error:?}");
    assert!(tx.commit().is_err(), "backend refusal was not terminal");
}

#[test]
fn read_multi_transaction_later_script_cannot_rearm_deadline() {
    let db = DbInstance::default();
    // The first script consumes most of the transaction budget. The second
    // starts while it is still live, but cannot re-arm it from its own start;
    // its much longer block timeout cannot extend the original instant.
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(250))
        .unwrap();
    tx.run_script(
        "?[value] <- [[1]] :sleep 0.15",
        Default::default(),
    )
    .unwrap();
    let error = tx
        .run_script(
            "?[value] <- [[1]] :sleep 0.15 :timeout 3600",
            Default::default(),
        )
        .unwrap_err();
    assert!(
        format!("{error:?}").contains("eval::timeout"),
        "{error:?}"
    );
    // This consumes and joins the handle. Returning proves the worker has
    // exited and dropped its storage snapshot rather than merely acknowledging
    // Abort before finishing thread cleanup.
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_direct_scan_lock_wait_honours_deadline() {
    let db = DbInstance::default();
    db.run_default(":create deadline_page {bucket: String, id: Int => payload: String}")
        .unwrap();
    db.run_default(
        "?[bucket, id, payload] <- [['a', 1, 'one']] \
         :put deadline_page {bucket, id => payload}",
    )
    .unwrap();

    let relation_lock = match &db {
        DbInstance::Mem(inner) => inner.relation_lock_for_tests("deadline_page"),
        _ => panic!("test database is not Mem"),
    };
    let relation_writer = relation_lock.write().unwrap();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(20))
        .unwrap();
    let error = tx
        .scan_relation_by_primary_key(
            "deadline_page",
            PrimaryKeyScan {
                prefix: vec![DataValue::from("a")],
                lower: PrimaryKeyScanBound::Unbounded,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 1,
            },
        )
        .unwrap_err();
    assert!(
        format!("{error:?}").contains("eval::timeout"),
        "{error:?}"
    );
    drop(relation_writer);
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_live_deadline_allows_query_scan_and_joined_close() {
    let db = DbInstance::default();
    db.run_default(":create live_deadline_page {id: Int => payload: String}")
        .unwrap();
    db.run_default(
        "?[id, payload] <- [[1, 'one']] :put live_deadline_page {id => payload}",
    )
    .unwrap();

    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        tx.run_script(
            "?[payload] := *live_deadline_page{id: 1, payload}",
            Default::default(),
        )
        .unwrap()
        .rows[0][0],
        DataValue::from("one")
    );
    assert_eq!(
        tx.scan_relation_by_primary_key(
            "live_deadline_page",
            PrimaryKeyScan {
                prefix: vec![],
                lower: PrimaryKeyScanBound::Unbounded,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 1,
            },
        )
        .unwrap()
        .rows[0][0],
        DataValue::from(1)
    );
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_nonzero_deadline_interrupts_sleep() {
    let db = DbInstance::default();
    let started = Instant::now();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(25))
        .unwrap();
    let error = tx
        .run_script("?[value] <- [[1]] :sleep 0.2", Default::default())
        .unwrap_err();
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_db_default_tightens_transaction_deadline() {
    let db = DbInstance::default();
    db.set_default_query_timeout(Some(0.02));
    let started = Instant::now();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_secs(2))
        .unwrap();
    let error = tx
        .run_script("?[value] <- [[1]] :sleep 0.2", Default::default())
        .unwrap_err();
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_parse_registry_wait_honours_deadline() {
    let db = DbInstance::default();
    let DbInstance::Mem(inner) = &db else {
        panic!("default DbInstance is not Mem")
    };
    let registry = inner.fixed_rules.write().unwrap();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(20))
        .unwrap();

    let started = Instant::now();
    let error = tx
        .run_script("?[value] <- [[1]]", Default::default())
        .unwrap_err();
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(registry);
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_skips_contended_running_query_registry() {
    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(100))
        .unwrap();
    let registry = match &db {
        DbInstance::Mem(inner) => inner.running_queries.lock().unwrap(),
        _ => panic!("default DbInstance is not Mem"),
    };
    let (done_send, done_receive) = crossbeam::channel::bounded(1);
    let worker = std::thread::spawn(move || {
        let error = tx
            .run_script("?[value] <- [[1]] :sleep 0.2", Default::default())
            .unwrap_err();
        let typed_timeout = format!("{error:?}").contains("eval::timeout");
        let close = tx.close_and_join();
        let _ = done_send.send(());
        (typed_timeout, close)
    });

    let completed_while_registry_was_locked = done_receive
        .recv_timeout(Duration::from_millis(500))
        .is_ok();
    drop(registry);
    let (typed_timeout, close) = worker.join().unwrap();
    close.unwrap();
    assert!(typed_timeout);
    assert!(
        completed_while_registry_was_locked,
        "bounded query waited on the global running-query registry"
    );
}

#[test]
fn read_multi_transaction_skips_poisoned_running_query_registry() {
    let db = DbInstance::default();
    let running_queries = match &db {
        DbInstance::Mem(inner) => inner.running_queries.clone(),
        _ => panic!("default DbInstance is not Mem"),
    };
    assert!(std::thread::spawn(move || {
        let _registry = running_queries.lock().unwrap();
        panic!("poison running-query registry for bounded-read regression")
    })
    .join()
    .is_err());

    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(50))
        .unwrap();
    let error = tx
        .run_script("?[value] <- [[1]] :sleep 0.2", Default::default())
        .unwrap_err();
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_idle_expiry_drops_snapshot_and_joins() {
    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(15))
        .unwrap();
    std::thread::sleep(Duration::from_millis(40));
    let error = tx
        .run_script("?[value] <- [[1]]", Default::default())
        .unwrap_err();
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    tx.close_and_join().unwrap();

    let mut replacement = db
        .read_multi_transaction_with_timeout(Duration::from_secs(1))
        .unwrap();
    replacement
        .run_script("?[value] <- [[2]]", Default::default())
        .unwrap();
    replacement.close_and_join().unwrap();

    // A writer also proves the expired worker no longer pins the Mem snapshot.
    db.run_default(":create after_idle_expiry {id: Int}")
        .unwrap();
}

#[test]
fn read_multi_transaction_snapshot_lock_wait_honours_deadline() {
    let db = DbInstance::default();
    let writer = db.multi_transaction(true);
    writer
        .run_script("?[value] <- [[1]]", Default::default())
        .unwrap();

    let started = Instant::now();
    let error = match db.read_multi_transaction_with_timeout(Duration::from_millis(20)) {
        Ok(_) => panic!("bounded reader unexpectedly acquired a writer-held snapshot"),
        Err(error) => error,
    };
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    writer.abort().unwrap();
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn read_multi_transaction_sqlite_snapshot_lock_wait_honours_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bounded-reader-contention.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    let writer = db.multi_transaction(true);
    writer
        .run_script("?[value] <- [[1]]", Default::default())
        .unwrap();

    let started = Instant::now();
    let error = match db.read_multi_transaction_with_timeout(Duration::from_millis(20)) {
        Ok(_) => panic!("bounded SQLite reader acquired a writer-held snapshot"),
        Err(error) => error,
    };
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
    writer.abort().unwrap();
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn read_multi_transaction_sqlite_failed_startup_returns_pooled_connection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bounded-reader-startup-pool.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default("?[value] <- [[1]]").unwrap();
    let inner = match &db {
        DbInstance::Sqlite(inner) => inner,
        _ => panic!("test database is not SQLite"),
    };
    let pooled_before = inner.db.pool_lock_for_tests().len();
    assert!(pooled_before > 0);
    let snapshot_writer = inner.db.snapshot_write_lock_for_tests();

    let error = match db.read_multi_transaction_with_timeout(Duration::from_millis(20)) {
        Ok(_) => panic!("bounded SQLite reader acquired a writer-held snapshot"),
        Err(error) => error,
    };
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    drop(snapshot_writer);
    let pooled_after = inner.db.pool_lock_for_tests().len();
    assert_eq!(pooled_after, pooled_before);
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn read_multi_transaction_sqlite_pool_contention_does_not_pin_joined_teardown() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bounded-reader-pool-drop.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create pool_drop {id: Int}").unwrap();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(200))
        .unwrap();
    tx.run_script("?[id] := *pool_drop{id}", Default::default())
        .unwrap();

    let pool = match &db {
        DbInstance::Sqlite(inner) => inner.db.pool_lock_for_tests(),
        _ => panic!("test database is not SQLite"),
    };
    std::thread::sleep(Duration::from_millis(250));
    let (done_send, done_receive) = crossbeam::channel::bounded(1);
    let closer = std::thread::spawn(move || {
        let result = tx.close_and_join();
        let _ = done_send.send(());
        result
    });
    let completed_while_pool_was_locked = done_receive
        .recv_timeout(Duration::from_millis(500))
        .is_ok();
    drop(pool);
    closer.join().unwrap().unwrap();
    assert!(
        completed_while_pool_was_locked,
        "bounded SQLite teardown waited indefinitely on the connection pool mutex"
    );
}

#[test]
fn read_multi_transaction_worker_panic_beats_channel_disconnect() {
    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_secs(1))
        .unwrap();
    let error = tx.panic_worker_for_tests().unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("injected bounded read worker panic"), "{rendered}");
    assert!(!rendered.contains("result channel disconnected"), "{rendered}");
    tx.close_and_join().unwrap();
}

#[test]
fn read_multi_transaction_plain_drop_joins_in_flight_worker() {
    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_millis(25))
        .unwrap();
    let (release_send, release_recv) = crossbeam::channel::bounded(1);
    let (ready_send, ready_recv) = crossbeam::channel::bounded(1);

    let error = tx
        .block_worker_for_tests(release_recv, ready_send)
        .unwrap_err();
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
    ready_recv.recv_timeout(Duration::from_secs(1)).unwrap();

    let (started_send, started_recv) = crossbeam::channel::bounded(1);
    let (done_send, done_recv) = crossbeam::channel::bounded(1);
    tx.notify_teardown_started_for_tests(started_send);
    let dropper = std::thread::spawn(move || {
        drop(tx);
        let _ = done_send.send(());
    });

    started_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(done_recv.recv_timeout(Duration::from_millis(50)).is_err());
    release_send.send(()).unwrap();
    done_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    dropper.join().unwrap();

    // Returning from Drop proves the joined worker and its snapshot are gone.
    db.run_default(":create after_in_flight_drop {id: Int}")
        .unwrap();
}

#[test]
fn read_multi_transaction_plain_drop_joins_idle_worker() {
    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_secs(5))
        .unwrap();
    let (release_send, release_recv) = crossbeam::channel::bounded(1);
    let (ready_send, ready_recv) = crossbeam::channel::bounded(1);
    tx.pause_before_next_receive_for_tests(release_recv, ready_send, false)
        .unwrap();
    ready_recv.recv_timeout(Duration::from_secs(1)).unwrap();

    let (started_send, started_recv) = crossbeam::channel::bounded(1);
    let (done_send, done_recv) = crossbeam::channel::bounded(1);
    tx.notify_teardown_started_for_tests(started_send);
    let dropper = std::thread::spawn(move || {
        drop(tx);
        let _ = done_send.send(());
    });

    started_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(done_recv.recv_timeout(Duration::from_millis(50)).is_err());
    release_send.send(()).unwrap();
    done_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    dropper.join().unwrap();

    db.run_default(":create after_idle_drop {id: Int}").unwrap();
}

#[test]
fn read_multi_transaction_full_channels_panic_drop_is_non_panicking() {
    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_secs(5))
        .unwrap();
    let (release_send, release_recv) = crossbeam::channel::bounded(1);
    let (ready_send, ready_recv) = crossbeam::channel::bounded(1);
    tx.pause_before_next_receive_for_tests(release_recv, ready_send, true)
        .unwrap();
    // Ready is sent only after the extra response fills the result channel.
    ready_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    // The worker is gated before receive, so this fills the command channel.
    tx.queue_worker_panic_for_tests().unwrap();

    let (started_send, started_recv) = crossbeam::channel::bounded(1);
    let (done_send, done_recv) = crossbeam::channel::bounded(1);
    tx.notify_teardown_started_for_tests(started_send);
    let dropper = std::thread::spawn(move || {
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(tx))).is_err();
        let _ = done_send.send(panicked);
    });

    started_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(done_recv.recv_timeout(Duration::from_millis(50)).is_err());
    release_send.send(()).unwrap();
    assert!(!done_recv.recv_timeout(Duration::from_secs(1)).unwrap());
    dropper.join().unwrap();
}

#[test]
fn read_multi_transaction_unwinding_drop_preserves_caller_panic_and_joins_worker() {
    const CALLER_PANIC: &str = "caller panic must survive bounded transaction Drop";

    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_secs(5))
        .unwrap();
    let (release_send, release_recv) = crossbeam::channel::bounded(1);
    let (ready_send, ready_recv) = crossbeam::channel::bounded(1);
    tx.pause_before_next_receive_for_tests(release_recv, ready_send, false)
        .unwrap();
    ready_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    // The worker will panic after the caller begins unwinding. Drop must join
    // it without replacing the original caller payload with a second panic.
    tx.queue_worker_panic_for_tests().unwrap();

    let (started_send, started_recv) = crossbeam::channel::bounded(1);
    let (done_send, done_recv) = crossbeam::channel::bounded(1);
    tx.notify_teardown_started_for_tests(started_send);
    let dropper = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _tx_dropped_while_unwinding = tx;
            std::panic::panic_any(CALLER_PANIC);
        }));
        let _ = done_send.send(());
        result
    });

    started_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(done_recv.recv_timeout(Duration::from_millis(50)).is_err());
    release_send.send(()).unwrap();
    done_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    let payload = dropper.join().unwrap().unwrap_err();
    assert_eq!(payload.downcast_ref::<&str>(), Some(&CALLER_PANIC));

    // Catching the caller panic happened only after Drop joined the panicked
    // worker and destroyed its snapshot.
    db.run_default(":create after_unwinding_drop {id: Int}")
        .unwrap();
}

#[test]
fn read_multi_transaction_direct_close_reports_queued_worker_panic() {
    let db = DbInstance::default();
    let mut tx = db
        .read_multi_transaction_with_timeout(Duration::from_secs(5))
        .unwrap();
    let (release_send, release_recv) = crossbeam::channel::bounded(1);
    let (ready_send, ready_recv) = crossbeam::channel::bounded(1);
    tx.pause_before_next_receive_for_tests(release_recv, ready_send, false)
        .unwrap();
    ready_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    // No request call observes this panic: explicit close is its first and only
    // reporting boundary.
    tx.queue_worker_panic_for_tests().unwrap();

    let (started_send, started_recv) = crossbeam::channel::bounded(1);
    let (done_send, done_recv) = crossbeam::channel::bounded(1);
    tx.notify_teardown_started_for_tests(started_send);
    let closer = std::thread::spawn(move || {
        let result = tx.close_and_join();
        let _ = done_send.send(());
        result
    });

    started_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(done_recv.recv_timeout(Duration::from_millis(50)).is_err());
    release_send.send(()).unwrap();
    done_recv.recv_timeout(Duration::from_secs(1)).unwrap();
    let error = closer.join().unwrap().unwrap_err();
    let rendered = error.to_string();
    assert!(
        rendered.contains("injected bounded read worker panic"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("result channel disconnected"),
        "{rendered}"
    );
}

#[test]
fn bounded_read_close_drops_a_full_request_channel_without_blocking() {
    let (sender, receiver) = crossbeam::channel::bounded(1);
    sender
        .send(TransactionPayload::Query((
            "?[value] <- [[1]]".to_string(),
            Default::default(),
        )))
        .unwrap();

    crate::close_bounded_request_channel(sender);
    assert!(matches!(
        receiver.recv().unwrap(),
        TransactionPayload::Query(_)
    ));
    assert!(receiver.recv().is_err());
}

#[test]
fn read_multi_transaction_zero_deadline_is_typed_startup_error() {
    let db = DbInstance::default();
    let error = match db.read_multi_transaction_with_timeout(Duration::ZERO) {
        Ok(_) => panic!("zero-duration bounded reader unexpectedly started"),
        Err(error) => error,
    };
    assert!(format!("{error:?}").contains("eval::timeout"), "{error:?}");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn read_multi_transaction_deadline_overflow_fails_closed() {
    let db = DbInstance::default();
    assert!(db
        .read_multi_transaction_with_timeout(Duration::MAX)
        .is_err());
}

fn assert_bounded_primary_key_pages(db: DbInstance) {
    db.run_default(":create pk_page {bucket: String, id: Int => payload: String}")
        .unwrap();
    let mut input = Vec::with_capacity(10_003);
    for id in 0..10_000 {
        input.push(DataValue::List(vec![
            DataValue::from("a"),
            DataValue::from(id),
            DataValue::from(format!("value-{id}")),
        ]));
    }
    for id in 0..3 {
        input.push(DataValue::List(vec![
            DataValue::from("b"),
            DataValue::from(id),
            DataValue::from(format!("other-{id}")),
        ]));
    }
    db.run_script(
        "?[bucket, id, payload] <- $rows :put pk_page {bucket, id => payload}",
        [("rows".to_string(), DataValue::List(input))].into(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    let tail = db
        .scan_relation_by_primary_key(
            "pk_page",
            &PrimaryKeyScan {
                prefix: vec![DataValue::from("a")],
                lower: PrimaryKeyScanBound::Excluded(vec![DataValue::from(9_995)]),
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 3,
            },
        )
        .unwrap();
    assert_eq!(tail.rows.headers, ["bucket", "id", "payload"]);
    assert_eq!(tail.scanned, 3);
    assert_eq!(
        tail.rows.rows.iter().map(|row| row[1].clone()).collect_vec(),
        [9_996, 9_997, 9_998]
            .into_iter()
            .map(DataValue::from)
            .collect_vec()
    );

    let maximum = db
        .scan_relation_by_primary_key(
            "pk_page",
            &PrimaryKeyScan {
                prefix: vec![DataValue::from("a")],
                lower: PrimaryKeyScanBound::Unbounded,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Descending,
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(maximum.scanned, 1);
    assert_eq!(maximum.rows.rows[0][1], DataValue::from(9_999));

    let inclusive = db
        .scan_relation_by_primary_key(
            "pk_page",
            &PrimaryKeyScan {
                prefix: vec![DataValue::from("a")],
                lower: PrimaryKeyScanBound::Included(vec![DataValue::from(9_998)]),
                upper: PrimaryKeyScanBound::Included(vec![DataValue::from(9_999)]),
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 10,
            },
        )
        .unwrap();
    assert_eq!(inclusive.scanned, 2);
    assert_eq!(inclusive.rows.rows[0][1], DataValue::from(9_998));
    assert_eq!(inclusive.rows.rows[1][1], DataValue::from(9_999));

    let composite = db
        .scan_relation_by_primary_key(
            "pk_page",
            &PrimaryKeyScan {
                prefix: vec![],
                lower: PrimaryKeyScanBound::Excluded(vec![
                    DataValue::from("a"),
                    DataValue::from(9_998),
                ]),
                upper: PrimaryKeyScanBound::Included(vec![
                    DataValue::from("b"),
                    DataValue::from(1),
                ]),
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 10,
            },
        )
        .unwrap();
    assert_eq!(composite.scanned, 3);
    assert_eq!(
        composite
            .rows
            .rows
            .iter()
            .map(|row| (row[0].clone(), row[1].clone()))
            .collect_vec(),
        vec![
            (DataValue::from("a"), DataValue::from(9_999)),
            (DataValue::from("b"), DataValue::from(0)),
            (DataValue::from("b"), DataValue::from(1)),
        ]
    );

    let exact_prefix = db
        .scan_relation_by_primary_key(
            "pk_page",
            &PrimaryKeyScan {
                prefix: vec![DataValue::from("b"), DataValue::from(1)],
                lower: PrimaryKeyScanBound::Unbounded,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(exact_prefix.scanned, 1);
    assert_eq!(exact_prefix.rows.rows[0][2], DataValue::from("other-1"));

    for bad_limit in [0, MAX_PRIMARY_KEY_SCAN_ROWS + 1] {
        let err = db
            .scan_relation_by_primary_key(
                "pk_page",
                &PrimaryKeyScan {
                    prefix: vec![],
                    lower: PrimaryKeyScanBound::Unbounded,
                    upper: PrimaryKeyScanBound::Unbounded,
                    direction: PrimaryKeyScanDirection::Ascending,
                    limit: bad_limit,
                },
            )
            .unwrap_err();
        assert!(format!("{err:?}").contains("scan limit"), "{err:?}");
    }

    let bad_arity = db
        .scan_relation_by_primary_key(
            "pk_page",
            &PrimaryKeyScan {
                prefix: vec![DataValue::from("a")],
                lower: PrimaryKeyScanBound::Included(vec![]),
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 1,
            },
        )
        .unwrap_err();
    assert!(format!("{bad_arity:?}").contains("prefix leaves 1 key columns"));

    let read_tx = db.multi_transaction(false);
    let within_snapshot = read_tx
        .scan_relation_by_primary_key(
            "pk_page",
            PrimaryKeyScan {
                prefix: vec![DataValue::from("b")],
                lower: PrimaryKeyScanBound::Unbounded,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(within_snapshot.rows.len(), 2);
    assert_eq!(
        read_tx
            .run_script(
                "?[count(id)] := *pk_page{bucket: 'b', id}",
                Default::default(),
            )
            .unwrap()
            .rows[0][0],
        DataValue::from(3)
    );
    read_tx.abort().unwrap();

    let write_tx = db.multi_transaction(true);
    let error = write_tx
        .scan_relation_by_primary_key(
            "pk_page",
            PrimaryKeyScan {
                prefix: vec![],
                lower: PrimaryKeyScanBound::Unbounded,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 1,
            },
        )
        .unwrap_err();
    assert!(
        format!("{error:?}").contains("require a read-only"),
        "{error:?}"
    );
    write_tx.abort().unwrap();

    db.run_default(":create temporal_page {id: Int, tt: TxTime => payload: String}")
        .unwrap();
    let temporal_error = db
        .scan_relation_by_primary_key(
            "temporal_page",
            &PrimaryKeyScan {
                prefix: vec![],
                lower: PrimaryKeyScanBound::Unbounded,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit: 1,
            },
        )
        .unwrap_err();
    assert!(format!("{temporal_error:?}").contains("temporal relation semantics"));
}

#[test]
fn bounded_primary_key_pages_are_exact_and_limited_in_mem() {
    assert_bounded_primary_key_pages(DbInstance::new("mem", "", "").unwrap());
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn bounded_primary_key_pages_are_exact_and_limited_in_sqlite() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("primary-key-pages.db");
    assert_bounded_primary_key_pages(
        DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap(),
    );
}

#[test]
fn test_vec_types() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a {k: String => v: <F32; 8>}")
        .unwrap();
    db.run_default("?[k, v] <- [['k', [1,2,3,4,5,6,7,8]]] :put a {k => v}")
        .unwrap();
    let res = db.run_default("?[k, v] := *a{k, v}").unwrap();
    assert_eq!(
        json!([1., 2., 3., 4., 5., 6., 7., 8.]),
        res.into_json()["rows"][0][1]
    );
    let res = db
        .run_default("?[v] <- [[vec([1,2,3,4,5,6,7,8])]]")
        .unwrap();
    assert_eq!(
        json!([1., 2., 3., 4., 5., 6., 7., 8.]),
        res.into_json()["rows"][0][0]
    );
    let res = db.run_default("?[v] <- [[rand_vec(5)]]").unwrap();
    assert_eq!(5, res.into_json()["rows"][0][0].as_array().unwrap().len());
    let res = db
        .run_default(r#"
            val[v] <- [[vec([1,2,3,4,5,6,7,8])]]
            ?[x,y,z] := val[v], x=l2_dist(v, v), y=cos_dist(v, v), nv = l2_normalize(v), z=ip_dist(nv, nv)
        "#)
        .unwrap();
    println!("{}", res.into_json());
}

#[test]
fn test_vec_index_insertion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r"
        ?[k, v, m] <- [['a', [1,2], true],
                       ['b', [2,3], false]]

        :create a {k: String => v: <F32; 2>, m: Bool}
    ",
    )
    .unwrap();
    db.run_default(
        r"
        ::hnsw create a:vec {
            dim: 2,
            m: 50,
            dtype: F32,
            fields: [v],
            distance: L2,
            ef_construction: 20,
            filter: m,
            #extend_candidates: true,
            #keep_pruned_connections: true,
        }",
    )
    .unwrap();
    let res = db
        .run_default("?[k] := *a:vec{layer: 0, fr_k, to_k}, k = fr_k or k = to_k")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    println!("update!");
    db.run_default(r#"?[k, m] <- [["a", false]] :update a {}"#)
        .unwrap();
    let res = db
        .run_default("?[k] := *a:vec{layer: 0, fr_k, to_k}, k = fr_k or k = to_k")
        .unwrap();
    assert_eq!(res.rows.len(), 0);
    println!("{}", res.into_json());
}

#[test]
fn filtered_hnsw_build_retains_only_eligible_vector_payloads() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r#"
        :create lane {
            id: Int =>
            e: <F32; 32> default rand_vec(32),
            status: String,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ?[id, status] :=
            id in int_range(4097),
            status = if(id == 0, 'active', 'archived')
        :put lane {id => status}
        "#,
    )
    .unwrap();

    db.run_default(
        r#"
        ::hnsw create lane:active_idx {
            dim: 32,
            m: 8,
            ef_construction: 32,
            fields: [e],
            distance: Cosine,
            filter: status == 'active',
        }
        "#,
    )
    .unwrap();

    let stats = take_last_hnsw_build_stats().expect("bulk build stats");
    assert_eq!(stats.rows_scanned, 4097);
    assert_eq!(stats.rows_eligible, 1);
    assert_eq!(stats.indexed_rows, 1);
    assert_eq!(stats.vectors_retained, 1);
    assert_eq!(stats.vector_bytes_retained, 32 * std::mem::size_of::<f32>());
    assert_eq!(stats.snapshot_rows, 0);

    // The accounting is not a substitute for correctness: the resulting
    // filtered index must still resolve the sole eligible source row.
    assert_eq!(
        db.run_default(
            "?[hit] := *lane{id: 0, e}, ~lane:active_idx{id: hit | query: e, k: 1, ef: 16}",
        )
        .unwrap()
        .into_json()["rows"],
        json!([[0]])
    );
}

#[test]
fn test_vec_index() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r"
        ?[k, v] <- [['a', [1,2]],
                    ['b', [2,3]],
                    ['bb', [2,3]],
                    ['c', [3,4]],
                    ['x', [0,0.1]],
                    ['a', [112,0]],
                    ['b', [1,1]]]

        :create a {k: String => v: <F32; 2>}
    ",
    )
    .unwrap();
    db.run_default(
        r"
        ::hnsw create a:vec {
            dim: 2,
            m: 50,
            dtype: F32,
            fields: [v],
            distance: L2,
            ef_construction: 20,
            filter: k != 'k1',
            #extend_candidates: true,
            #keep_pruned_connections: true,
        }",
    )
    .unwrap();
    db.run_default(
        r"
        ?[k, v] <- [
                    ['a2', [1,25]],
                    ['b2', [2,34]],
                    ['bb2', [2,33]],
                    ['c2', [2,32]],
                    ['a2', [2,31]],
                    ['b2', [1,10]]
                    ]
        :put a {k => v}
        ",
    )
    .unwrap();

    println!("all links");
    for (_, nrows) in db.export_relations(["a:vec"].iter()).unwrap() {
        let nrows = nrows.rows;
        for row in nrows {
            println!("{} {} -> {} {}", row[0], row[1], row[4], row[7]);
        }
    }

    let res = db
        .run_default(
            r"
        #::explain {
        ?[dist, k, v] := ~a:vec{k, v | query: q, k: 2, ef: 20, bind_distance: dist}, q = vec([200, 34])
        #}
        ",
        )
        .unwrap();
    println!("results");
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{} {} {}", row[0], row[1], row[2]);
    }
}

#[test]
fn test_fts_indexing() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {k: String => v: String}")
        .unwrap();
    db.run_default(
        r"?[k, v] <- [['a', 'hello world!'], ['b', 'the world is round']] :put a {k => v}",
    )
    .unwrap();
    db.run_default(
        r"::fts create a:fts {
            extractor: v,
            tokenizer: Simple,
            filters: [Lowercase, Stemmer('English'), Stopwords('en')]
        }",
    )
    .unwrap();
    db.run_default(
        r"?[k, v] <- [
            ['b', 'the world is square!'],
            ['c', 'see you at the end of the world!'],
            ['d', 'the world is the world and makes the world go around']
        ] :put a {k => v}",
    )
    .unwrap();
    let res = db
        .run_default(
            r"
        ?[word, src_k, offset_from, offset_to, position, total_length] :=
            *a:fts{word, src_k, offset_from, offset_to, position, total_length}
        ",
        )
        .unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    println!("query");
    let res = db
        .run_default(r"?[k, v, s] := ~a:fts{k, v | query: 'world', k: 2, bind_score: s}")
        .unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn test_lsh_indexing2() {
    for i in 1..10 {
        let f = i as f64 / 10.;
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(r":create a {k: String => v: String}")
            .unwrap();
        db.run_script(
            r"::lsh create a:lsh {extractor: v, tokenizer: NGram, n_gram: 3, target_threshold: $t }",
            BTreeMap::from([("t".into(), f.into())]),
            ScriptMutability::Mutable
        )
            .unwrap();
        db.run_default("?[k, v] <- [['a', 'ewiygfspeoighjsfcfxzdfncalsdf']] :put a {k => v}")
            .unwrap();
        let res = db
            .run_default("?[k] := ~a:lsh{k | query: 'ewiygfspeoighjsfcfxzdfncalsdf', k: 1}")
            .unwrap();
        assert!(!res.rows.is_empty());
    }
}

#[test]
fn test_lsh_indexing3() {
    for i in 1..10 {
        let f = i as f64 / 10.;
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(r":create text {id: String,  => text: String, url: String? default null, dt: Float default now(), dup_for: String? default null }")
            .unwrap();
        db.run_script(
            r"::lsh create text:lsh {
                    extractor: text,
                    # extract_filter: is_null(dup_for),
                    tokenizer: NGram,
                    n_perm: 200,
                    target_threshold: $t,
                    n_gram: 7,
                }",
            BTreeMap::from([("t".into(), f.into())]),
            ScriptMutability::Mutable,
        )
        .unwrap();
        db.run_default(
            "?[id, text] <- [['a', 'This function first generates 32 random bytes using the os.urandom function. It then base64 encodes these bytes using base64.urlsafe_b64encode, removes the padding, and decodes the result to a string.']] :put text {id, text}",
        )
        .unwrap();
        let res = db
            .run_default(
                r#"?[id, dup_for] :=
    ~text:lsh{id: id, dup_for: dup_for, | query: "This function first generates 32 random bytes using the os.urandom function. It then base64 encodes these bytes using base64.urlsafe_b64encode, removes the padding, and decodes the result to a string.", }"#,
            )
            .unwrap();
        assert!(!res.rows.is_empty());
        println!("{}", res.into_json());
    }
}

#[test]
fn filtering() {
    let db = DbInstance::default();
    let res = db
        .run_default(
            r"
        {
            ?[x, y] <- [[1, 2]]
            :create _rel {x => y}
            :returning
        }
        {
            ?[x, y] := x = 1, *_rel{x, y: 3}, y = 2
        }
    ",
        )
        .unwrap();
    assert_eq!(0, res.rows.len());

    let res = db
        .run_default(
            r"
        {
            ?[x, u, y] <- [[1, 0, 2]]
            :create _rel {x, u => y}
            :returning
        }
        {
            ?[x, y] := x = 1, *_rel{x, y: 3}, y = 2
        }
    ",
        )
        .unwrap();
    assert_eq!(0, res.rows.len());
}

#[test]
fn test_lsh_indexing4() {
    for i in 1..10 {
        let f = i as f64 / 10.;
        let db = DbInstance::new("mem", "", "").unwrap();
        db.run_default(r":create a {k: String => v: String}")
            .unwrap();
        db.run_script(
            r"::lsh create a:lsh {extractor: v, tokenizer: NGram, n_gram: 3, target_threshold: $t }",
            BTreeMap::from([("t".into(), f.into())]),
            ScriptMutability::Mutable
        )
            .unwrap();
        db.run_default("?[k, v] <- [['a', 'ewiygfspeoighjsfcfxzdfncalsdf']] :put a {k => v}")
            .unwrap();
        db.run_default("?[k] <- [['a']] :rm a {k}").unwrap();
        let res = db
            .run_default("?[k] := ~a:lsh{k | query: 'ewiygfspeoighjsfcfxzdfncalsdf', k: 1}")
            .unwrap();
        assert!(res.rows.is_empty());
    }
}

#[test]
fn test_lsh_indexing() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {k: String => v: String}")
        .unwrap();
    db.run_default(
        r"?[k, v] <- [['a', 'hello world!'], ['b', 'the world is round']] :put a {k => v}",
    )
    .unwrap();
    db.run_default(
        r"::lsh create a:lsh {extractor: v, tokenizer: Simple, n_gram: 3, target_threshold: 0.3 }",
    )
    .unwrap();
    db.run_default(
        r"?[k, v] <- [
            ['b', 'the world is square!'],
            ['c', 'see you at the end of the world!'],
            ['d', 'the world is the world and makes the world go around'],
            ['e', 'the world is the world and makes the world not go around']
        ] :put a {k => v}",
    )
    .unwrap();
    let res = db.run_default("::columns a:lsh").unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    let _res = db
        .run_default(
            r"
        ?[src_k, hash] :=
            *a:lsh{src_k, hash}
        ",
        )
        .unwrap();
    // for row in _res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }
    let _res = db
        .run_default(
            r"
        ?[k, minhash] :=
            *a:lsh:inv{k, minhash}
        ",
        )
        .unwrap();
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }
    let res = db
        .run_default(
            r"
            ?[k, v] := ~a:lsh{k, v |
                query: 'see him at the end of the world',
            }
            ",
        )
        .unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    let res = db.run_default("::indices a").unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    db.run_default(r"::lsh drop a:lsh").unwrap();
}

#[test]
fn test_insertions() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {k => v: <F32; 1536> default rand_vec(1536)}")
        .unwrap();
    db.run_default(r"?[k] <- [[1]] :put a {k}").unwrap();
    db.run_default(r"?[k, v] := *a{k, v}").unwrap();
    db.run_default(
        r"::hnsw create a:i {
            fields: [v], dim: 1536, ef: 16, filter: k % 3 == 0,
            m: 32
        }",
    )
    .unwrap();
    db.run_default(r"?[count(fr_k)] := *a:i{fr_k}").unwrap();
    db.run_default(r"?[k] <- [[1]] :put a {k}").unwrap();
    db.run_default(r"?[k] := k in int_range(300) :put a {k}")
        .unwrap();
    let res = db
        .run_default(
            r"?[dist, k] := ~a:i{k | query: v, bind_distance: dist, k:10, ef: 50, filter: k % 2 == 0, radius: 245}, *a{k: 96, v}",
        )
        .unwrap();
    println!("results");
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{} {}", row[0], row[1]);
    }
}

#[test]
fn tokenizers() {
    let tokenizers = TokenizerCache::default();
    let tokenizer = tokenizers
        .get(
            "simple",
            &TokenizerConfig {
                name: "Simple".into(),
                args: vec![],
            },
            &[],
        )
        .unwrap();

    // let tokenizer = TextAnalyzer::from(SimpleTokenizer)
    //     .filter(RemoveLongFilter::limit(40))
    //     .filter(LowerCaser)
    //     .filter(Stemmer::new(Language::English));
    let mut token_stream = tokenizer.token_stream("It is closer to Apache Lucene than to Elasticsearch or Apache Solr in the sense it is not an off-the-shelf search engine server, but rather a crate that can be used to build such a search engine.");
    while let Some(token) = token_stream.next() {
        println!("Token {:?}", token.text);
    }

    println!("XXXXXXXXXXXXX");

    let tokenizer = tokenizers
        .get(
            "cangjie",
            &TokenizerConfig {
                name: "Cangjie".into(),
                args: vec![],
            },
            &[],
        )
        .unwrap();

    let mut token_stream = tokenizer.token_stream("这个产品Finchat.io是一个相对比较有特色的文档问答类网站，它集成了750多家公司的经融数据。感觉是把财报等数据借助Embedding都向量化了，然后接入ChatGPT进行对话。");
    while let Some(token) = token_stream.next() {
        println!("Token {:?}", token.text);
    }
}

#[test]
fn multi_index_vec() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r#"
        :create product {
            id
            =>
            name,
            description,
            price,
            name_vec: <F32; 1>,
            description_vec: <F32; 1>
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::hnsw create product:semantic{
            fields: [name_vec, description_vec],
            dim: 1,
            ef: 16,
            m: 32,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ?[id, name, description, price, name_vec, description_vec] <- [[1, "name", "description", 100, [1], [1]]]

        :put product {id => name, description, price, name_vec, description_vec}
        "#,
    ).unwrap();
    let res = db.run_default("::indices product").unwrap();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn ensure_not() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(
        r"
    %ignore_error { :create id_alloc{id: Int => next_id: Int, last_id: Int}}
%ignore_error {
    ?[id, next_id, last_id] <- [[0, 1, 1000]];
    :ensure_not id_alloc{id => next_id, last_id}
}
    ",
    )
    .unwrap();
}

#[test]
fn insertion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {x => y}").unwrap();
    assert!(db
        .run_default(r"?[x, y] <- [[1, 2]] :insert a {x => y}",)
        .is_ok());
    assert!(db
        .run_default(r"?[x, y] <- [[1, 3]] :insert a {x => y}",)
        .is_err());
}

#[test]
fn deletion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {x => y}").unwrap();
    assert!(db.run_default(r"?[x] <- [[1]] :delete a {x}").is_err());
    assert!(db
        .run_default(r"?[x, y] <- [[1, 2]] :insert a {x => y}",)
        .is_ok());
    db.run_default(r"?[x] <- [[1]] :delete a {x}").unwrap();
}

#[test]
fn into_payload() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r":create a {x => y}").unwrap();
    db.run_default(r"?[x, y] <- [[1, 2], [3, 4]] :insert a {x => y}")
        .unwrap();

    let mut res = db.run_default(r"?[x, y] := *a[x, y]").unwrap();
    assert_eq!(res.rows.len(), 2);

    let delete = res.clone().into_payload("a", "rm");
    db.run_script(delete.0.as_str(), delete.1, ScriptMutability::Mutable)
        .unwrap();
    assert_eq!(
        db.run_default(r"?[x, y] := *a[x, y]").unwrap().rows.len(),
        0
    );

    db.run_default(r":create b {m => n}").unwrap();
    res.headers = vec!["m".into(), "n".into()];
    let put = res.into_payload("b", "put");
    db.run_script(put.0.as_str(), put.1, ScriptMutability::Mutable)
        .unwrap();
    assert_eq!(
        db.run_default(r"?[m, n] := *b[m, n]").unwrap().rows.len(),
        2
    );
}

#[test]
fn returning() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a {x => y}").unwrap();
    let res = db
        .run_default(r"?[x, y] <- [[1, 2]] :insert a {x => y} ")
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([["OK"]]));
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }

    let res = db
        .run_default(r"?[x, y] <- [[1, 3], [2, 4]] :returning :put a {x => y} ")
        .unwrap();
    assert_eq!(
        res.into_json()["rows"],
        json!([["inserted", 1, 3], ["inserted", 2, 4], ["replaced", 1, 2]])
    );
    // println!("{:?}", res.headers);
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }

    let res = db
        .run_default(r"?[x] <- [[1], [4]] :returning :rm a {x} ")
        .unwrap();
    // println!("{:?}", res.headers);
    // for row in res.into_json()["rows"].as_array().unwrap() {
    //     println!("{}", row);
    // }
    assert_eq!(
        res.into_json()["rows"],
        json!([
            ["requested", 1, null],
            ["requested", 4, null],
            ["deleted", 1, 3]
        ])
    );
    db.run_default(r":create todo{id:Uuid default rand_uuid_v1() => label: String, done: Bool}")
        .unwrap();
    let res = db
        .run_default(r"?[label,done] <- [['milk',false]] :put todo{label,done} :returning")
        .unwrap();
    assert_eq!(res.rows[0].len(), 4);
    for title in res.headers.iter() {
        print!("{} ", title);
    }
    println!();
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn parser_corner_case() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(r#"?[x] := x = 1 or x = 2"#).unwrap();
    db.run_default(r#"?[C] := C = 1  orx[C] := C = 1"#).unwrap();
    db.run_default(r#"?[C] := C = true, C  inx[C] := C = 1"#)
        .unwrap();
    db.run_default(r#"?[k] := k in int_range(300)"#).unwrap();
    db.run_default(r#"ywcc[a] <- [[1]] noto[A] := ywcc[A] ?[A] := noto[A]"#)
        .unwrap();
}

#[test]
fn as_store_in_imperative_script() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let res = db
        .run_default(
            r#"
    { ?[x, y, z] <- [[1, 2, 3], [4, 5, 6]] } as _store
    { ?[x, y, z] := *_store{x, y, z} }
    "#,
        )
        .unwrap();
    assert_eq!(res.into_json()["rows"], json!([[1, 2, 3], [4, 5, 6]]));
    let res = db
        .run_default(
            r#"
    {
        ?[y] <- [[1], [2], [3]]
        :create a {x default rand_uuid_v1() => y}
        :returning
    } as _last
    {
        ?[x] := *_last{_kind: 'inserted', x}
    }
    "#,
        )
        .unwrap();
    assert_eq!(3, res.rows.len());
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
    assert!(db
        .run_default(
            r#"
    {
        ?[x, x] := x = 1
    } as _last
    "#
        )
        .is_err());

    let res = db
        .run_default(
            r#"
    {
        x[y] <- [[1], [2], [3]]
        ?[sum(y)] := x[y]
    } as _last
    {
        ?[sum_y] := *_last{sum_y}
    }
    "#,
        )
        .unwrap();
    assert_eq!(1, res.rows.len());
    for row in res.into_json()["rows"].as_array().unwrap() {
        println!("{}", row);
    }
}

#[test]
fn update_shall_not_destroy_values() {
    let db = DbInstance::default();
    db.run_default(r"?[x, y] <- [[1, 2]] :create z {x => y default 0}")
        .unwrap();
    let r = db.run_default(r"?[x, y] := *z {x, y}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2]]));
    db.run_default(r"?[x] <- [[1]] :update z {x}").unwrap();
    let r = db.run_default(r"?[x, y] := *z {x, y}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2]]));
}

#[test]
fn update_shall_work() {
    let db = DbInstance::default();
    db.run_default(r"?[x, y, z] <- [[1, 2, 3]] :create z {x => y, z}")
        .unwrap();
    let r = db.run_default(r"?[x, y, z] := *z {x, y, z}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2, 3]]));
    db.run_default(r"?[x, y] <- [[1, 4]] :update z {x, y}")
        .unwrap();
    let r = db.run_default(r"?[x, y, z] := *z {x, y, z}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 4, 3]]));
}

#[test]
fn sysop_in_imperatives() {
    let script = r#"
    {
            :create cm_src {
                aid: String =>
                title: String,
                author: String?,
                kind: String,
                url: String,
                domain: String?,
                pub_time: Float?,
                dt: Float default now(),
                weight: Float default 1,
            }
        }
        {
            :create cm_txt {
                tid: String =>
                aid: String,
                tag: String,
                follows_tid: String?,
                dup_for: String?,
                text: String,
                info_amount: Int,
            }
        }
        {
            :create cm_seg {
                sid: String =>
                tid: String,
                tag: String,
                part: Int,
                text: String,
                vec: <F32; 1536>,
            }
        }
        {
            ::hnsw create cm_seg:vec {
                dim: 1536,
                m: 50,
                dtype: F32,
                fields: vec,
                distance: Cosine,
                ef: 100,
            }
        }
        {
            ::lsh create cm_txt:lsh {
                extractor: text,
                extract_filter: is_null(dup_for),
                tokenizer: NGram,
                n_perm: 200,
                target_threshold: 0.5,
                n_gram: 7,
            }
        }
        {::relations}
    "#;
    let db = DbInstance::default();
    db.run_default(script).unwrap();
}

#[test]
fn bad_parse() {
    let db = DbInstance::default();
    db.run_default(
        r"
        :create named_hero_history {
        name: String,
        value: Bool,
        when: Int
    }",
    )
    .unwrap();
    db.run_default(r"
        last_named_hero[first, first, max(hist)] := *named_hero_history[first, first, value, hist], hist <= 1;

        some_named_hero[first, first, value] := last_named_hero[first, first, last], *named_hero_history[first, first, value, last];

        named_hero[first, first, value] := cast[first], value = false, not some_named_hero[first, first, _];
        named_hero[first, first, value] := some_named_hero[first, first, value];
        ?[hero] :=
    ").expect_err("should fail");
}

#[test]
fn puts() {
    let db = DbInstance::default();
    db.run_default(
        r"
            :create cm_txt {
                tid: String =>
                aid: String,
                tag: String,
                follows_tid: String? default null,
                for_qs: [String] default [],
                dup_for: String? default null,
                text: String,
                seg_vecs: [<F32; 1536>],
                seg_pos: [(Int, Int)],
                format: String default 'text',
                info_amount: Int,
            }
    ",
    )
    .unwrap();
    db.run_default(
        r"
        ?[tid, aid, tag, text, info_amount, dup_for, seg_vecs, seg_pos] := dup_for = null,
                tid = 'x', aid = 'y', tag = 'z', text = 'w', info_amount = 12,
                follows_tid = null, for_qs = [], format = 'x',
                seg_vecs = [], seg_pos = [[0, 10]]
        :put cm_txt {tid, aid, tag, text, info_amount, seg_vecs, seg_pos, dup_for}
    ",
    )
    .unwrap();
}

#[test]
fn short_hand() {
    let db = DbInstance::default();
    db.run_default(r":create x {x => y, z}").unwrap();
    db.run_default(r"?[x, y, z] <- [[1, 2, 3]] :put x {}")
        .unwrap();
    let r = db.run_default(r"?[x, y, z] := *x {x, y, z}").unwrap();
    assert_eq!(r.into_json()["rows"], json!([[1, 2, 3]]));
}

#[test]
fn param_shorthand() {
    let db = DbInstance::default();
    db.run_script(
        r"
        ?[] <- [[$x, $y, $z]]
        :create x {}
    ",
        BTreeMap::from([
            ("x".to_string(), DataValue::from(1)),
            ("y".to_string(), DataValue::from(2)),
            ("z".to_string(), DataValue::from(3)),
        ]),
        ScriptMutability::Mutable,
    )
    .unwrap();
    let res = db.run_default(r"?[x, y, z] := *x {x, y, z}");
    assert_eq!(res.unwrap().into_json()["rows"], json!([[1, 2, 3]]));
}

#[test]
fn crashy_imperative() {
    let db = DbInstance::default();
    db.run_default(
        r"
        {:create _test {a}}

        %loop
            %if { len[count(x)] := *_test[x]; ?[x] := len[z], x = z >= 10 }
                %then %return _test
            %end
            { ?[a] := a = rand_uuid_v1(); :put _test {a} }
        %end
        ",
    )
    .unwrap();
}

#[test]
fn hnsw_index() {
    // NOTE (mnestic 0.12.2): the inherited schema below used
    // `last_accessed_at: Validity default [floor(now()), true]`. `floor(now())` is a float in
    // SECONDS, and a Validity timestamp is an integer in MICROSECONDS — so every row this test
    // wrote was stamped at 1970, ~1e6x too small. The test never asserted on the value, so it
    // never noticed. It was the ONLY caller of the validity float channel in the entire tree,
    // and it was in our own suite. See docs/plans/mnestic-0121-0130/design-0122.md.
    let db = DbInstance::default();
    db.run_default(
        r#"
        :create beliefs {
            belief_id: Uuid,
            character_id: Uuid,
            belief: String,
            last_accessed_at: Validity default [to_int(now() * 1000000), true],
            =>
            details: String default "",
            parent_belief_id: Uuid? default null,
            valence: Float default 0,
            aspects: [(String, Float, String, String)] default [],
            belief_embedding: <F32; 768>,
            details_embedding: <F32; 768>,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::hnsw create beliefs:embedding_space {
            dim: 768,
            m: 50,
            dtype: F32,
            fields: [belief_embedding, details_embedding],
            distance: Cosine,
            ef_construction: 20,
            extend_candidates: false,
            keep_pruned_connections: false,
        }
    "#,
    )
    .unwrap();
    db.run_default(r#"
        ?[belief_id, character_id, belief, belief_embedding, details_embedding] <- [[rand_uuid_v1(), rand_uuid_v1(), "test", rand_vec(768), rand_vec(768)]]
        :put beliefs {}
    "#).unwrap();
    let res = db.run_default(r#"
            ?[belief, valence, dist, character_id, vector] := ~beliefs:embedding_space{ belief, valence, character_id |
                query: rand_vec(768),
                k: 100,
                ef: 20,
                radius: 1.0,
                bind_distance: dist,
                bind_vector: vector
            }

            :order -valence
            :order dist
    "#).unwrap();
    println!("{}", res.into_json()["rows"][0][4]);
}

#[test]
fn fts_drop() {
    let db = DbInstance::default();
    db.run_default(
        r#"
            :create entity {name}
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::fts create entity:fts_index { extractor: name,
            tokenizer: Simple, filters: [Lowercase]
        }
    "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::fts drop entity:fts_index
    "#,
    )
    .unwrap();
}

// ==== mnestic fork: temporal-axis rule at :create (bitemporality step 3) ====

#[test]
fn txtime_create_validation() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let expect_axis_err = |script: &str, needle: &str| {
        let err = db.run_default(script).expect_err(script);
        // collapse miette's line-wrapping so needles match across breaks
        let msg = format!("{err:?}")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            msg.contains("invalid temporal-axis declaration"),
            "{script}: {msg}"
        );
        assert!(
            msg.contains(needle),
            "{script}: expected `{needle}` in: {msg}"
        );
        // the copy-pasteable corrected declaration is in the help text
        assert!(
            msg.contains(":create"),
            "{script}: no corrected form in: {msg}"
        );
    };

    expect_axis_err(
        ":create r_val {k => v: Int, tt: TxTime}",
        "key column, not a value column",
    );
    expect_axis_err(":create r_pos {tt: TxTime, k => v: Int}", "last key column");
    expect_axis_err(
        ":create r_ord {k, tt: TxTime, v: Validity => x: Int}",
        "last key column",
    );
    expect_axis_err(
        ":create r_two {k, t1: TxTime, t2: TxTime => v: Int}",
        "at most one TxTime",
    );
    expect_axis_err(
        ":create r_2vt {v1: Validity, v2: Validity, tt: TxTime}",
        "at most one Validity",
    );
    expect_axis_err(
        ":create r_null {k, tt: TxTime? => v: Int}",
        "cannot be nullable",
    );
    expect_axis_err(
        ":create r_gap {v: Validity, k, tt: TxTime}",
        "immediately precede",
    );

    // Valid shapes: tt-only (system-versioned) and bitemporal.
    db.run_default(":create audit {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default(":create belief {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    let cols = db.run_default("::columns audit").unwrap().into_json();
    let rendered = cols["rows"].to_string();
    assert!(rendered.contains("TxTime"), "{rendered}");
}

#[test]
fn txtime_create_rejected_on_temp_relations() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // inside a multi-statement script, `_`-relations are legitimate temps
    let err = db
        .run_default("{:create _tmp {k, tt: TxTime => v: Int}} {?[k] <- [[1]]}")
        .expect_err("temp TxTime must be rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("transaction-temp"), "{msg}");
}

#[test]
fn txtime_user_supplied_value_rejected() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit2 {k, tt: TxTime => v: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, tt, v] <- [[1, 123, 2]] :put audit2 {k, tt => v}")
        .expect_err("user-supplied tt must be rejected");
    let msg = format!("{err:?}");
    assert!(msg.contains("engine-assigned"), "{msg}");
}

// ==== mnestic fork: tt write path (bitemporality step 3b) ====

#[test]
fn txtime_put_stamps_at_commit() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_w {k, tt: TxTime => v: Int}")
        .unwrap();

    // Deferred read-your-writes: the same script does NOT see its own write.
    let res = db
        .run_default("{?[k, v] <- [[1, 10]] :put audit_w {k => v}} {?[k] := *audit_w[k, tt, v]}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // The next script does.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // Capture a tt point between the two versions, then correct.
    let DbInstance::Mem(inner) = &db else {
        panic!()
    };
    let between = inner.tt_clock().peek() + 1;
    db.run_default("?[k, v] <- [[1, 20]] :put audit_w {k => v}")
        .unwrap();

    // Bare read = CURRENT STATE (the correction only) — §4 migration invariant.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 20]]));

    // As-of the point between the versions: the original belief.
    let res = db
        .run_default(&format!("?[k, v] := *audit_w[k, tt, v @ (tt: {between})]"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // As-of before the first write: nothing was known.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v @ (tt: 1)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // @ (tt: 'NOW') is the explicit spelling of the current-state default.
    let res = db
        .run_default("?[k, v] := *audit_w[k, tt, v @ (tt: 'NOW')]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 20]]));
}

#[test]
fn txtime_same_tx_double_put_is_last_write_wins() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_lww {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10], [1, 20]] :put audit_lww {k => v}")
        .unwrap();
    let res = db
        .run_default("?[v] := *audit_lww[k, tt, v]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "same (key, tt) collapses: {rows:?}");
}

#[test]
fn txtime_rm_appends_retraction() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_rm {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_rm {k => v}")
        .unwrap();
    let DbInstance::Mem(inner) = &db else {
        panic!()
    };
    let before_rm = inner.tt_clock().peek() + 1;
    db.run_default("?[k] <- [[1]] :rm audit_rm {k}").unwrap();

    // Current state: the key is believed-deleted -> absent.
    let res = db
        .run_default("?[k] := *audit_rm[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // As-of before the removal: still there — nothing was physically deleted.
    let res = db
        .run_default(&format!(
            "?[k, v] := *audit_rm[k, tt, v @ (tt: {before_rm})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // rm again: already believed-deleted, a no-op.
    db.run_default("?[k] <- [[1]] :rm audit_rm {k}").unwrap();

    // rm of a missing key: no-op; :delete of a missing key: error.
    db.run_default("?[k] <- [[999]] :rm audit_rm {k}").unwrap();
    let err = db
        .run_default("?[k] <- [[999]] :delete audit_rm {k}")
        .expect_err(":delete missing must fail");
    assert!(format!("{err:?}").contains("does not exist"), "{err:?}");
}

#[test]
fn txtime_bitemporal_puts_and_conflicts() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create belief_w {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    // assert + later cessation on the vt axis — two separate transactions
    db.run_default("?[e, v, x] <- [[1, 'ASSERT', 10]] :put belief_w {e, v => x}")
        .unwrap();
    db.run_default("?[e, v, x] <- [[1, 'RETRACT', 10]] :put belief_w {e, v => x}")
        .unwrap();
    let res = db
        .run_default("?[v, x] := *belief_w[e, v, tt, x]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 2);

    // assert AND retract of one (key, vt) in ONE tx: unbreakable tie -> error
    let err = db
        .run_default(
            "?[e, v, x] <- [[2, [123, true], 1], [2, [123, false], 1]] :put belief_w {e, v => x}",
        )
        .expect_err("assert+retract same (key, vt) in one tx must fail");
    assert!(
        format!("{err:?}").contains("asserts AND retracts"),
        "{err:?}"
    );

    // :rm on bitemporal (4c): a cessation at the supplied valid time —
    // 'RETRACT' coerces to (now, retract), i.e. "ceases now"
    db.run_default("?[e, v] <- [[1, 'RETRACT']] :rm belief_w {e, v}")
        .unwrap();
    let res = db
        .run_default("?[x] := *belief_w[e, v, tt, x @ 'NOW']")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "ceased now");
}

#[test]
fn txtime_unsupported_ops_error() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_ops {k, tt: TxTime => v: Int}")
        .unwrap();
    // 4c: these now work — only :replace stays rejected
    let err = db
        .run_default("?[k, v] <- [[1, 1]] :replace audit_ops {k => v}")
        .expect_err("replace must fail");
    assert!(format!("{err:?}").contains("history"), "{err:?}");

    let err = db
        .run_default(r#"::set_triggers audit_ops on put { ?[k] <- [[1]] }"#)
        .expect_err("triggers must fail");
    assert!(format!("{err:?}").contains("not supported"), "{err:?}");

    let err = db
        .run_default("::index create audit_ops:by_v {v}")
        .expect_err("index create must fail");
    assert!(format!("{err:?}").contains("not supported"), "{err:?}");
}

#[test]
fn txtime_rows_persist_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tt_rows.db");
    let path_str = path.to_str().unwrap().to_string();
    {
        let db = DbInstance::new("sqlite", &path_str, "").unwrap();
        db.run_default(":create audit_p {k, tt: TxTime => v: Int}")
            .unwrap();
        db.run_default("?[k, v] <- [[1, 10]] :put audit_p {k => v}")
            .unwrap();
    }
    let db = DbInstance::new("sqlite", &path_str, "").unwrap();
    let res = db
        .run_default("?[k, v] := *audit_p[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));
}

#[test]
fn txtime_import_relations_one_tt_per_batch() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_imp {k, tt: TxTime => v: Int}")
        .unwrap();

    // import two rows in one batch: both get the SAME tt (one belief event)
    let payload = serde_json::json!({
        "audit_imp": {"headers": ["k", "v"], "rows": [[1, 10], [2, 20]]}
    });
    db.import_relations_str_with_err(&payload.to_string())
        .unwrap();
    let res = db
        .run_default("?[k, tt, v] := *audit_imp[k, tt, v]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][1], rows[1][1], "one tt per import batch: {rows:?}");

    // importing a tt column is rejected
    let bad = serde_json::json!({
        "audit_imp": {"headers": ["k", "tt", "v"], "rows": [[3, 1, 30]]}
    });
    let err = db
        .import_relations_str_with_err(&bad.to_string())
        .expect_err("tt header must be rejected");
    assert!(format!("{err:?}").contains("engine-assigned"), "{err:?}");

    // delete-imports are rejected
    let del = serde_json::json!({
        "-audit_imp": {"headers": ["k"], "rows": [[1]]}
    });
    let err = db
        .import_relations_str_with_err(&del.to_string())
        .expect_err("delete-import must be rejected");
    assert!(format!("{err:?}").contains("use :rm"), "{err:?}");
}

#[test]
fn txtime_restore_backup_reseeds_clock() {
    // Build a source store whose clock is far in the future, back it up,
    // restore into a fresh store: the fresh clock must jump past the mark.
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src.db");
    let backup_path = dir.path().join("backup.db");
    let dst_path = dir.path().join("dst.db");

    let far_future;
    {
        let db = DbInstance::new("sqlite", src_path.to_str().unwrap(), "").unwrap();
        db.run_default(":create audit_b {k, tt: TxTime => v: Int}")
            .unwrap();
        let DbInstance::Sqlite(inner) = &db else {
            panic!()
        };
        far_future = inner
            .tt_clock()
            .advance_with_now(crate::runtime::tt_clock::wall_clock_micros() + 3_600_000_000);
        db.run_default("?[k, v] <- [[1, 10]] :put audit_b {k => v}")
            .unwrap();
        db.backup_db(backup_path.to_str().unwrap()).unwrap();
    }

    let db = DbInstance::new("sqlite", dst_path.to_str().unwrap(), "").unwrap();
    db.restore_backup(backup_path.to_str().unwrap()).unwrap();
    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    assert!(
        inner.tt_clock().peek() > far_future,
        "restore must re-seed the clock past the restored mark"
    );
    // and the restored row is readable
    let res = db
        .run_default("?[k, v] := *audit_b[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // import_from_backup of a tt relation is rejected
    let err = db
        .import_from_backup(backup_path.to_str().unwrap(), &["audit_b".to_string()])
        .expect_err("import_from_backup of tt relation must be rejected");
    assert!(format!("{err:?}").contains("restore_backup"), "{err:?}");
}

#[test]
fn restore_backup_reseeds_relation_store_id() {
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("src-relid.db");
    let backup_path = dir.path().join("backup-relid.db");
    let dst_path = dir.path().join("dst-relid.db");

    {
        let src = DbInstance::new("sqlite", src_path.to_str().unwrap(), "").unwrap();
        src.run_default(":create alpha {k: Int => v: String}")
            .unwrap();
        src.run_default(":create beta {k: Int => v: String}")
            .unwrap();
        src.run_default("?[k, v] <- [[1, 'alpha-one']] :put alpha {k => v}")
            .unwrap();
        src.backup_db(backup_path.to_str().unwrap()).unwrap();
    }

    let dst = DbInstance::new("sqlite", dst_path.to_str().unwrap(), "").unwrap();
    dst.restore_backup(backup_path.to_str().unwrap()).unwrap();
    dst.run_default(":create gamma {k: Int => v: String}")
        .unwrap();
    dst.run_default("?[k, v] <- [[1, 'gamma-one']] :put gamma {k => v}")
        .unwrap();

    let alpha = dst
        .run_default("?[k, v] := *alpha{k, v}")
        .unwrap()
        .into_json();
    assert_eq!(alpha["rows"], json!([[1, "alpha-one"]]));
    let gamma = dst
        .run_default("?[k, v] := *gamma{k, v}")
        .unwrap()
        .into_json();
    assert_eq!(gamma["rows"], json!([[1, "gamma-one"]]));
}

#[test]
fn poisoned_relation_counter_is_repaired_on_open() {
    use crate::data::tuple::TupleT;
    use crate::runtime::relation::RelationId;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("poisoned-relid.db");
    {
        let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
        db.run_default(":create alpha {k: Int => v: String}")
            .unwrap();
        db.run_default(":create beta {k: Int => v: String}")
            .unwrap();
        db.run_default("?[k, v] <- [[1, 'beta-one']] :put beta {k => v}")
            .unwrap();

        let DbInstance::Sqlite(inner) = &db else {
            panic!()
        };
        let mut tx = inner.transact_write().unwrap();
        let counter_key = vec![DataValue::Null].encode_as_key(RelationId::SYSTEM);
        tx.store_tx
            .put(&counter_key, &RelationId::new(1).raw_encode())
            .unwrap();
        tx.commit_tx().unwrap();
    }

    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create gamma {k: Int => v: String}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 'gamma-one']] :put gamma {k => v}")
        .unwrap();
    let beta = db
        .run_default("?[k, v] := *beta{k, v}")
        .unwrap()
        .into_json();
    assert_eq!(beta["rows"], json!([[1, "beta-one"]]));
}

#[test]
fn corrupt_value_rows_error_and_can_be_repaired() {
    use crate::data::tuple::TupleT;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-value.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create damaged {k: Int => v: String}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 'intact']] :put damaged {k => v}")
        .unwrap();

    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    let mut tx = inner.transact_write().unwrap();
    let handle = tx.get_relation("damaged", false).unwrap();
    let key = vec![DataValue::from(1)].encode_as_key(handle.id);
    tx.store_tx.put(&key, &[0x91, 0x01, 0x02]).unwrap();
    tx.commit_tx().unwrap();
    drop(tx);

    let scan_error = db
        .run_default("?[k, v] := *damaged{k, v}")
        .expect_err("a corrupt row must fail a full scan without panicking");
    assert!(
        format!("{scan_error:?}").contains("eval::corrupt_value_blob"),
        "{scan_error:?}"
    );

    let lookup_error = db
        .run_default("wanted[k] <- [[1]] ?[v] := wanted[k], *damaged{k, v}")
        .expect_err("a fully-bound point lookup must not swallow decode errors");
    assert!(
        format!("{lookup_error:?}").contains("eval::corrupt_value_blob"),
        "{lookup_error:?}"
    );

    let repaired = db.run_default("::repair_corrupt damaged").unwrap();
    assert_eq!(repaired.rows, vec![vec![DataValue::from(1)]]);
    assert!(db
        .run_default("?[k, v] := *damaged{k, v}")
        .unwrap()
        .rows
        .is_empty());
}

#[cfg(feature = "storage-sqlite")]
fn corrupt_all_index_values(db: &DbInstance, relation: &str, index: &str, hnsw: bool) {
    let DbInstance::Sqlite(inner) = db else {
        panic!("corruption fixture requires sqlite")
    };
    let mut tx = inner.transact_write().unwrap();
    let relation_handle = tx.get_relation(relation, false).unwrap();
    let index_handle = if hnsw {
        &relation_handle.hnsw_indices[index].0
    } else {
        &relation_handle.fts_indices[index].0
    };
    let start = index_handle.encode_partial_key_for_store(&[]);
    let end = index_handle.encode_partial_key_for_store(&[DataValue::Bot]);
    let keys = tx
        .store_tx
        .range_scan(&start, &end)
        .map(|item| item.unwrap().0)
        .collect_vec();
    assert!(!keys.is_empty(), "fixture must create index rows");
    for key in keys {
        tx.store_tx.put(&key, &[0x91, 0x01, 0x02]).unwrap();
    }
    tx.commit_tx().unwrap();
}

#[test]
#[cfg(feature = "storage-sqlite")]
fn corrupt_fts_postings_return_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-fts.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create docs {id: Int => body: String}")
        .unwrap();
    db.run_default(
        "::fts create docs:search { extractor: body, tokenizer: Simple, filters: [Lowercase] }",
    )
    .unwrap();
    db.run_default("?[id, body] <- [[1, 'needle text']] :put docs {id => body}")
        .unwrap();
    corrupt_all_index_values(&db, "docs", "search", false);

    let error = db
        .run_default("?[id] := ~docs:search{id | query: 'needle', k: 10}")
        .expect_err("a corrupt FTS posting must return an error without panicking");
    assert!(
        format!("{error:?}").contains("eval::corrupt_value_blob"),
        "{error:?}"
    );
}

#[test]
#[cfg(feature = "storage-sqlite")]
fn corrupt_hnsw_rows_return_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt-hnsw.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create pts {id: Int => emb: <F32; 2>}")
        .unwrap();
    db.run_default(
        "::hnsw create pts:search { dim: 2, m: 8, dtype: F32, fields: [emb], \
         distance: L2, ef_construction: 32 }",
    )
    .unwrap();
    db.run_default(
        "?[id, emb] <- [[1, vec([1.0, 0.0])], [2, vec([0.0, 1.0])]] \
         :put pts {id => emb}",
    )
    .unwrap();
    corrupt_all_index_values(&db, "pts", "search", true);

    let error = db
        .run_default("?[id] := ~pts:search{id | query: vec([1.0, 0.0]), k: 10, ef: 32}")
        .expect_err("a corrupt HNSW row must return an error without panicking");
    assert!(
        format!("{error:?}").contains("eval::corrupt_value_blob"),
        "{error:?}"
    );
}

#[test]
#[cfg(feature = "storage-sqlite")]
fn short_base_tuple_hnsw_build_returns_error_instead_of_panicking() {
    use crate::data::tuple::TupleT;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("short-hnsw-base-row.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create pts {id: Int => emb: <F32; 2>}")
        .unwrap();

    // A valid key plus an empty (and therefore syntactically valid) value blob
    // decodes to a one-field tuple even though the relation schema has two.
    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    let mut tx = inner.transact_write().unwrap();
    let handle = tx.get_relation("pts", false).unwrap();
    let key = vec![DataValue::from(1)].encode_as_key(handle.id);
    tx.store_tx.put(&key, &[]).unwrap();
    tx.commit_tx().unwrap();
    drop(tx);

    let error = db
        .run_default(
            "::hnsw create pts:search { dim: 2, m: 8, dtype: F32, fields: [emb], \
             distance: L2, ef_construction: 32 }",
        )
        .expect_err("a short base tuple must return an error without panicking");
    assert!(
        format!("{error:?}").contains("corrupt base row for HNSW index 'pts:search'"),
        "{error:?}"
    );
}

#[cfg(feature = "storage-rocksdb")]
const ROCKS_HNSW_CREATE: &str =
    "::hnsw create pts:search { dim: 2, m: 8, dtype: F32, fields: [emb], \
     distance: L2, ef_construction: 32 }";

#[cfg(feature = "storage-rocksdb")]
fn rocks_hnsw_fixture(path: &str) -> DbInstance {
    let db = DbInstance::new("rocksdb", path, "").unwrap();
    db.run_default(":create pts {id: Int => emb: <F32; 2>}")
        .unwrap();
    db.run_default(
        "?[id, emb] <- [[1, vec([1.0, 0.0])], [2, vec([0.0, 1.0])]] \
         :put pts {id => emb}",
    )
    .unwrap();
    db
}

#[cfg(feature = "storage-rocksdb")]
fn set_rocks_hnsw_failpoint(db: &DbInstance, point: HnswBuildTestFailPoint) {
    let DbInstance::RocksDb(inner) = db else {
        panic!("HNSW recovery fixture requires RocksDB")
    };
    inner.set_hnsw_build_failpoint_for_tests(point);
}

#[cfg(feature = "storage-rocksdb")]
fn relation_description(db: &DbInstance, name: &str) -> Option<String> {
    db.run_default("::relations")
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row[0] == DataValue::from(name))
        .map(|row| match &row[8] {
            DataValue::Str(description) => description.to_string(),
            other => panic!("relation description is not a string: {other:?}"),
        })
}

#[cfg(feature = "storage-rocksdb")]
fn assert_rocks_hnsw_works(db: &DbInstance) {
    let rows = db
        .run_default("?[id] := ~pts:search{id | query: vec([1.0, 0.0]), k: 2, ef: 32}")
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 2, "published index should return both vectors");
    assert_eq!(relation_description(db, "pts:search").as_deref(), Some(""));
}

#[test]
#[cfg(feature = "storage-rocksdb")]
fn rocks_hnsw_reported_error_cleans_ingested_child_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hnsw-error-cleanup");
    let db = rocks_hnsw_fixture(path.to_str().unwrap());
    set_rocks_hnsw_failpoint(&db, HnswBuildTestFailPoint::ErrorAfterIngest);

    let error = db
        .run_default(ROCKS_HNSW_CREATE)
        .expect_err("injected post-ingest failure must be reported");
    assert!(
        format!("{error:?}").contains("ErrorAfterIngest"),
        "{error:?}"
    );
    assert!(
        db.run_default("::columns pts:search").is_err(),
        "eager error cleanup must remove the unattached child catalogue row"
    );
    assert!(db.run_default("::indices pts").unwrap().rows.is_empty());

    db.run_default(ROCKS_HNSW_CREATE).unwrap();
    assert_rocks_hnsw_works(&db);
}

#[test]
#[cfg(feature = "storage-rocksdb")]
fn rocks_hnsw_reported_phase_a_error_cleans_empty_child_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hnsw-phase-a-error-cleanup");
    let db = rocks_hnsw_fixture(path.to_str().unwrap());
    set_rocks_hnsw_failpoint(&db, HnswBuildTestFailPoint::ErrorAfterPhaseA);

    let error = db
        .run_default(ROCKS_HNSW_CREATE)
        .expect_err("injected Phase-A failure must be reported");
    assert!(
        format!("{error:?}").contains("ErrorAfterPhaseA"),
        "{error:?}"
    );
    assert!(
        db.run_default("::columns pts:search").is_err(),
        "eager error cleanup must remove the empty Phase-A child"
    );
    assert!(db.run_default("::indices pts").unwrap().rows.is_empty());

    db.run_default(ROCKS_HNSW_CREATE).unwrap();
    assert_rocks_hnsw_works(&db);
}

#[test]
#[cfg(feature = "storage-rocksdb")]
fn rocks_hnsw_phase_a_kill_is_recovered_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hnsw-phase-a-kill");
    let db = rocks_hnsw_fixture(path.to_str().unwrap());
    set_rocks_hnsw_failpoint(&db, HnswBuildTestFailPoint::AbandonAfterPhaseA);

    let error = db
        .run_default(ROCKS_HNSW_CREATE)
        .expect_err("kill point must leave Phase-A state behind");
    assert!(
        format!("{error:?}").contains("AbandonAfterPhaseA"),
        "{error:?}"
    );
    assert!(db.run_default("::indices pts").unwrap().rows.is_empty());
    let description = relation_description(&db, "pts:search").unwrap();
    assert!(
        description.starts_with("__mnestic_internal_hnsw_build_v1:"),
        "unattached child must carry its durable recovery marker: {description}"
    );

    drop(db);
    let db = DbInstance::new("rocksdb", path.to_str().unwrap(), "").unwrap();
    db.run_default(ROCKS_HNSW_CREATE).unwrap();
    assert_rocks_hnsw_works(&db);
}

#[test]
#[cfg(feature = "storage-rocksdb")]
fn rocks_hnsw_post_ingest_kill_reclaims_old_data_range() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hnsw-post-ingest-kill");
    let db = rocks_hnsw_fixture(path.to_str().unwrap());
    set_rocks_hnsw_failpoint(&db, HnswBuildTestFailPoint::AbandonAfterIngest);

    let error = db
        .run_default(ROCKS_HNSW_CREATE)
        .expect_err("kill point must leave ingested but unattached data behind");
    assert!(
        format!("{error:?}").contains("AbandonAfterIngest"),
        "{error:?}"
    );
    assert!(db.run_default("::indices pts").unwrap().rows.is_empty());

    let (old_lower, old_upper) = {
        let DbInstance::RocksDb(inner) = &db else {
            panic!()
        };
        let mut tx = inner.transact().unwrap();
        let child = tx.get_relation("pts:search", false).unwrap();
        let lower = child.encode_partial_key_for_store(&[]);
        let upper = child.encode_partial_key_for_store(&[DataValue::Bot]);
        assert!(
            tx.store_tx.range_scan(&lower, &upper).next().is_some(),
            "post-ingest kill fixture must leave persistent graph rows"
        );
        tx.commit_tx().unwrap();
        (lower, upper)
    };

    drop(db);
    let db = DbInstance::new("rocksdb", path.to_str().unwrap(), "").unwrap();
    db.run_default(ROCKS_HNSW_CREATE).unwrap();
    assert_rocks_hnsw_works(&db);

    let DbInstance::RocksDb(inner) = &db else {
        panic!()
    };
    let mut tx = inner.transact().unwrap();
    assert!(
        tx.store_tx
            .range_scan(&old_lower, &old_upper)
            .next()
            .is_none(),
        "retry recovery must delete the abandoned relation-id range"
    );
    tx.commit_tx().unwrap();
}

#[test]
#[cfg(feature = "storage-rocksdb")]
fn rocks_hnsw_recovery_rejects_an_unmarked_child() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hnsw-unmarked-child");
    let db = rocks_hnsw_fixture(path.to_str().unwrap());
    set_rocks_hnsw_failpoint(&db, HnswBuildTestFailPoint::AbandonAfterPhaseA);
    db.run_default(ROCKS_HNSW_CREATE)
        .expect_err("kill point must leave Phase-A state behind");

    db.run_default("::describe pts:search 'tampered'")
        .unwrap();
    let error = db
        .run_default(ROCKS_HNSW_CREATE)
        .expect_err("recovery must not guess whether an unmarked child is disposable");
    assert!(
        error
            .to_string()
            .contains("does not carry a valid durable HNSW-build marker"),
        "{error:?}"
    );
    assert_eq!(
        relation_description(&db, "pts:search").as_deref(),
        Some("tampered"),
        "fail-closed recovery must leave the ambiguous child untouched"
    );
    assert!(db.run_default("::indices pts").unwrap().rows.is_empty());
}

#[test]
fn txtime_cross_statement_conflicts_rejected() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create belief_ms {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    // assert + retract of one (key, vt) across TWO statements of one script
    let err = db
        .run_default(
            "{?[e, v, x] <- [[9, [123, true], 1]] :put belief_ms {e, v => x}} \
             {?[e, v, x] <- [[9, [123, false], 1]] :put belief_ms {e, v => x}}",
        )
        .expect_err("cross-statement assert+retract must fail");
    assert!(
        format!("{err:?}").contains("asserts AND retracts"),
        "{err:?}"
    );

    // tt-only: :rm then :put of the same PRE-EXISTING key in one script
    db.run_default(":create a_ms {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put a_ms {k => v}")
        .unwrap();
    let err = db
        .run_default("{?[k] <- [[1]] :rm a_ms {k}} {?[k, v] <- [[1, 99]] :put a_ms {k => v}}")
        .expect_err("rm-then-put same key one tx must fail");
    assert!(
        format!("{err:?}").contains("asserts AND retracts"),
        "{err:?}"
    );

    // tt-only: :put then :rm of a key NOT in the store — clearer message
    let err = db
        .run_default("{?[k, v] <- [[5, 50]] :put a_ms {k => v}} {?[k] <- [[5]] :rm a_ms {k}}")
        .expect_err("put-then-rm same key one tx must fail");
    assert!(
        format!("{err:?}").contains("written in the same transaction"),
        "{err:?}"
    );
}

#[test]
fn txtime_delete_believed_deleted_errors() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a_bd {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put a_bd {k => v}")
        .unwrap();
    db.run_default("?[k] <- [[1]] :rm a_bd {k}").unwrap();
    let err = db
        .run_default("?[k] <- [[1]] :delete a_bd {k}")
        .expect_err(":delete of believed-deleted key must fail");
    assert!(format!("{err:?}").contains("believed-deleted"), "{err:?}");
}

#[test]
fn txtime_remove_relation_with_pending_writes_rejected() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create zomb {k, tt: TxTime => v: Int}")
        .unwrap();
    let err = db
        .run_default("{?[k, v] <- [[9, 9]] :put zomb {k => v}} {::remove zomb}")
        .expect_err("::remove with pending tt writes must fail");
    assert!(
        format!("{err:?}").contains("pending transaction-time writes"),
        "{err:?}"
    );
}

#[test]
fn txtime_create_with_rows_rejects_tt_header() {
    let db = DbInstance::new("mem", "", "").unwrap();
    let err = db
        .run_default("?[k, tt, v] <- [[1, 123, 2]] :create cw2 {k, tt: TxTime => v: Int}")
        .expect_err("tt header on :create-with-rows must fail");
    assert!(format!("{err:?}").contains("engine-assigned"), "{err:?}");
}

#[test]
fn txtime_axis_selector_errors() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a_asof {k, tt: TxTime => v: Int}")
        .unwrap();
    // bare @ E means valid time, everywhere: tt-only relations reject it
    let err = db
        .run_default("?[k, v] := *a_asof[k, tt, v @ 'NOW']")
        .expect_err("bare @ on tt-only must error");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("no valid-time axis") || msg.contains("system-versioned"),
        "{msg}"
    );
    let err = db
        .run_default("?[k, v] := *a_asof[k, tt, v @ (vt: 'NOW')]")
        .expect_err("vt label on tt-only must error");
    assert!(format!("{err:?}").contains("valid-time"), "{err:?}");

    // tt label on a plain relation errors
    db.run_default(":create plain_r {k => v: Int}").unwrap();
    let err = db
        .run_default("?[k, v] := *plain_r[k, v @ (tt: 'NOW')]")
        .expect_err("tt label on plain must error");
    assert!(
        format!("{err:?}").contains("no transaction-time axis"),
        "{err:?}"
    );

    // tt label on a vt-only relation errors; vt label still works
    db.run_default(":create vt_r {k, v: Validity => x: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, x] := *vt_r[k, v, x @ (tt: 'NOW')]")
        .expect_err("tt label on vt-only must error");
    assert!(
        format!("{err:?}").contains("no transaction-time axis"),
        "{err:?}"
    );
    db.run_default("?[k, x] := *vt_r[k, v, x @ (vt: 'NOW')]")
        .unwrap();

    // bitemporal: selectors resolve via the two-level scan (step 4b)
    db.run_default(":create bi_r {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, x] := *bi_r[k, v, tt, x @ (tt: 'NOW')]")
        .unwrap();
    db.run_default("?[k, x] := *bi_r[k, v, tt, x]").unwrap();

    // duplicate axis label is a parse error
    let err = db
        .run_default("?[k, v] := *a_asof[k, tt, v @ (tt: 1, tt: 2)]")
        .expect_err("duplicate label must fail");
    assert!(
        format!("{err:?}").contains("duplicate temporal axis"),
        "{err:?}"
    );

    // labeled vt form works on vt relations, order-free pair parses
    db.run_default("?[k, x] := *vt_r[k, v, x @ (vt: 'NOW')]")
        .unwrap();
}

#[test]
fn txtime_bitemporal_double_assert_lww() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create belief_lww {e, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default(
        "?[e, v, x] <- [[1, [50, true], 10], [1, [50, true], 20]] :put belief_lww {e, v => x}",
    )
    .unwrap();
    let res = db
        .run_default("?[x] := *belief_lww[e, v, tt, x]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn txtime_trigger_on_plain_relation_writes_into_tt_relation() {
    // The one currently-working trigger/tt interaction: a put-trigger on a
    // PLAIN relation whose body writes into a tt relation — rows buffer and
    // stamp at the outer commit.
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create plain_src {k => v: Int}").unwrap();
    db.run_default(":create audit_trail {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default(
        "::set_triggers plain_src on put { ?[k, v] := _new[k, v] :put audit_trail {k => v} }",
    )
    .unwrap();
    db.run_default("?[k, v] <- [[7, 70]] :put plain_src {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *audit_trail[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[7, 70]]));
}

#[test]
fn txtime_abort_drops_rows_and_hwm_atomically() {
    // The HWM+rows same-tx atomicity obligation from step 2: a transaction
    // whose later statement fails must leave neither rows nor an advanced
    // persisted mark.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tt_atomic.db");
    let path_str = path.to_str().unwrap().to_string();
    let db = DbInstance::new("sqlite", &path_str, "").unwrap();
    db.run_default(":create a_at {k, tt: TxTime => v: Int}")
        .unwrap();

    // script: a valid buffered put, then a failing statement -> whole tx aborts
    let err = db
        .run_default("{?[k, v] <- [[1, 10]] :put a_at {k => v}} {?[k] <- [[1]] :delete a_at {k}}");
    assert!(
        err.is_err(),
        "the :delete of a not-yet-committed key must fail the tx"
    );

    // no rows visible...
    let res = db
        .run_default("?[k] := *a_at[k, tt, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);
    // ...and no persisted mark (nothing tt-stamped has ever committed here)
    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    let tx = inner.transact().unwrap();
    assert_eq!(tx.read_persisted_tt_hwm().unwrap(), None);
}

// ==== mnestic fork: custom aggregate registration (semirings R0b) ====

/// A user ⊕ keeping the numeric maximum — a legitimate absorptive meet.
struct TestMaxi;
impl crate::data::aggr::MeetAggrObj for TestMaxi {
    fn init_val(&self) -> DataValue {
        DataValue::Null
    }
    fn update(&self, left: &mut DataValue, right: &DataValue) -> miette::Result<bool> {
        if *left == DataValue::Null || right > left {
            *left = right.clone();
            return Ok(*left != DataValue::Null);
        }
        Ok(false)
    }
}

/// A non-absorptive ⊕ (numeric addition) — illegal as a meet.
struct TestAdder;
impl crate::data::aggr::MeetAggrObj for TestAdder {
    fn init_val(&self) -> DataValue {
        DataValue::from(0)
    }
    fn update(&self, left: &mut DataValue, right: &DataValue) -> miette::Result<bool> {
        let l = left.get_float().unwrap_or(0.);
        let r = right.get_float().unwrap_or(0.);
        *left = DataValue::from(l + r);
        Ok(true)
    }
}

#[test]
fn custom_aggr_meet_in_recursion_converges() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.register_custom_aggr("maxi".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    // longest-reachable-value: recursive SCC using the custom meet
    let res = db
        .run_default(
            r#"
        edges[f, t, w] <- [[1, 2, 10.0], [2, 3, 5.0], [1, 3, 2.0], [3, 4, 30.0]]
        reach[t, maxi(w)] := edges[1, t, w]
        reach[t, maxi(w2)] := reach[m, w], edges[m, t, w1], w2 = max(w, w1)
        ?[t, w] := reach[t, w]
        "#,
        )
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    // node 4 reachable with max edge weight 30 along the path
    assert!(rows.iter().any(|r| r[0] == 4 && r[1] == 30.0), "{rows:?}");
}

#[test]
fn custom_aggr_non_meet_rejected_in_recursion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.register_custom_aggr("summy".to_string(), false, || Box::new(TestAdder))
        .unwrap();
    // non-meet custom in a recursive SCC: stratifier must reject like a builtin
    let err = db
        .run_default(
            r#"
        edges[f, t] <- [[1, 2], [2, 3]]
        reach[t, summy(x)] := edges[1, t], x = 1
        reach[t, summy(x)] := reach[m, x], edges[m, t]
        ?[t, x] := reach[t, x]
        "#,
        )
        .expect_err("non-meet custom aggregate in recursion must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("stratif") || msg.contains("aggregation") || msg.contains("recursion"),
        "{msg}"
    );
}

#[test]
fn custom_aggr_non_recursive_uses_normal_adapter() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // is_meet=true in an all-meet head rides the meet path...
    db.register_custom_aggr("maxi2".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    let res = db
        .run_default("?[maxi2(x)] := x in [3, 9, 4]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]));
    // ...while is_meet=false forces AggrKind::Normal — the MeetToNormalAdapter
    db.register_custom_aggr("maxinm".to_string(), false, || Box::new(TestMaxi))
        .unwrap();
    let res = db
        .run_default("?[maxinm(x)] := x in [3, 9, 4]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]));
    // custom aggregates take no args in R0
    let err = db
        .run_default("?[maxinm(x, 5)] := x in [3, 9, 4]")
        .expect_err("args must be rejected");
    assert!(format!("{err:?}").contains("takes no arguments"), "{err:?}");
}

#[test]
fn custom_aggr_registry_policy() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // builtin names reserved
    let err = db
        .register_custom_aggr("min".to_string(), true, || Box::new(TestMaxi))
        .expect_err("builtin name must be reserved");
    assert!(format!("{err:?}").contains("reserved"), "{err:?}");
    // duplicates rejected; unregister-then-register works
    db.register_custom_aggr("dup".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    let err = db
        .register_custom_aggr("dup".to_string(), true, || Box::new(TestMaxi))
        .expect_err("duplicate must be rejected");
    assert!(format!("{err:?}").contains("already registered"), "{err:?}");
    assert!(db.unregister_custom_aggr("dup").unwrap());
    db.register_custom_aggr("dup".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    // unknown aggregate still errors cleanly
    let err = db
        .run_default("?[nosuch(x)] := x in [1]")
        .expect_err("unknown aggregate must fail");
    assert!(format!("{err:?}").contains("nosuch"), "{err:?}");
}

#[test]
fn custom_aggr_rejected_in_trigger_scripts() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.register_custom_aggr("maxi3".to_string(), true, || Box::new(TestMaxi))
        .unwrap();
    db.run_default(":create t_src {k => v: Int}").unwrap();
    db.run_default(":create t_dst {k => v}").unwrap();
    // trigger validation parses with an empty custom registry (R0 policy)
    let err = db
        .run_default(
            "::set_triggers t_src on put { ?[k, maxi3(v)] := _new[k, v] :put t_dst {k => v} }",
        )
        .expect_err("custom aggregate in trigger script must be rejected");
    assert!(format!("{err:?}").contains("maxi3"), "{err:?}");
}

#[test]
#[should_panic(expected = "non-idempotent meet aggregate")]
fn custom_aggr_debug_probe_catches_non_absorptive_meet() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // registered as meet, but ⊕ is addition: the debug probe must fire
    db.register_custom_aggr("badsum".to_string(), true, || Box::new(TestAdder))
        .unwrap();
    let _ = db.run_default(
        r#"
        edges[f, t] <- [[1, 2], [1, 3]]
        reach[f, badsum(x)] := edges[f, t], x = 1.0
        ?[f, x] := reach[f, x]
        "#,
    );
}

#[test]
fn meet_and_or_report_change_not_stability() {
    // mnestic fork fix: upstream returned the INVERTED changed-bit from the
    // and/or meet aggregates (true when stable), so a real change never
    // propagated through the semi-naive delta and stable values were kept in
    // it. Pin the corrected contract: true iff the value changed.
    use crate::data::aggr::{MeetAggrAnd, MeetAggrObj, MeetAggrOr};
    let and = MeetAggrAnd;
    let mut v = DataValue::from(true);
    assert!(and.update(&mut v, &DataValue::from(false)).unwrap());
    assert!(!and.update(&mut v, &DataValue::from(false)).unwrap());
    assert!(!and.update(&mut v, &DataValue::from(true)).unwrap());
    let or = MeetAggrOr;
    let mut v = DataValue::from(false);
    assert!(or.update(&mut v, &DataValue::from(true)).unwrap());
    assert!(!or.update(&mut v, &DataValue::from(true)).unwrap());
    assert!(!or.update(&mut v, &DataValue::from(false)).unwrap());
}

#[test]
fn txtime_negation_sees_current_state() {
    // Regression: negated atoms against tt-only relations hit NegJoin's
    // unreachable!() before StoredWithValidity gained a neg_join (the
    // current-state default made that the DEFAULT path for `not *audit{…}`).
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("neg.db");
        let db = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
        db.run_default(":create audit_n {k, tt: TxTime => v: Int}")
            .unwrap();
        db.run_default("?[k, v] <- [[1, 10], [2, 20]] :put audit_n {k => v}")
            .unwrap();
        db.run_default("?[k] <- [[2]] :rm audit_n {k}").unwrap();

        // bare negation: key 1 exists (excluded), key 2 believed-deleted and
        // key 3 never existed (both included)
        let res = db
            .run_default("r[k] <- [[1], [2], [3]] ?[k] := r[k], not *audit_n{k}")
            .unwrap()
            .into_json();
        assert_eq!(res["rows"], serde_json::json!([[2], [3]]), "{engine}");

        // explicit selector on the negated atom
        let res = db
            .run_default("r[k] <- [[1], [2]] ?[k] := r[k], not *audit_n{k @ (tt: 'NOW')}")
            .unwrap()
            .into_json();
        assert_eq!(res["rows"], serde_json::json!([[2]]), "{engine}");

        // ::explain must not panic either (join_type)
        db.run_default("::explain { r[k] <- [[1]] ?[k] := r[k], not *audit_n{k} }")
            .unwrap();
    }
}

#[test]
fn vt_negation_with_selector_no_longer_panics() {
    // Pre-existing upstream panic, fixed by the same StoredWithValidity
    // neg_join: a negated vt atom with an @ selector.
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create vt_n {k, v: Validity => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, 'ASSERT', 5]] :put vt_n {k, v => x}")
        .unwrap();
    let res = db
        .run_default("r[k] <- [[1], [2]] ?[k] := r[k], not *vt_n{k @ 'NOW'}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[2]]));
}

#[test]
fn txtime_reads_on_sqlite_and_selector_forms() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forms.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    db.run_default(":create audit_f {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_f {k => v}")
        .unwrap();
    let DbInstance::Sqlite(inner) = &db else {
        panic!()
    };
    let between = inner.tt_clock().peek() + 1;
    db.run_default("?[k, v] <- [[1, 20]] :put audit_f {k => v}")
        .unwrap();

    // named-field form with selector
    let res = db
        .run_default(&format!("?[k, w] := *audit_f{{k, v: w @ (tt: {between})}}"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]));

    // 'END' synonym = current state
    let res = db
        .run_default("?[k, w] := *audit_f{k, v: w @ (tt: 'END')}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 20]]));

    // date-only ISO string now parses (midnight UTC) — far past -> empty
    let res = db
        .run_default("?[k, w] := *audit_f{k, v: w @ (tt: '2001-01-01')}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);

    // order-free pair on a BITEMPORAL relation resolves (step 4b)
    db.run_default(":create bi_f {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, x] := *bi_f[k, v, tt, x @ (tt: 'NOW', vt: 'NOW')]")
        .unwrap();

    // nullable vt with TxTime is rejected at :create (the 8th shape)
    let err = db
        .run_default(":create bad_null_vt {k, v: Validity?, tt: TxTime => x: Int}")
        .expect_err("nullable vt with tt must be rejected");
    assert!(format!("{err:?}").contains("cannot be nullable"), "{err:?}");
}

// ==== mnestic fork: two-level bitemporal reads (step 4b) ====

/// A worked bitemporal history. Timeline (vt in abstract µs, tt per commit):
///   tt0: assert (vt=100, x=1)   — "1 from day 100"
///   tt1: assert (vt=200, x=2)   — "changed to 2 on day 200"
///   tt2: assert (vt=200, x=3)   — "correction: it was 3, not 2"
///   tt3: retract (vt=300)       — "ceased on day 300"
fn bitemporal_fixture(engine: &str, path: &str) -> (DbInstance, [i64; 4]) {
    let db = DbInstance::new(engine, path, "").unwrap();
    db.run_default(":create hist {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    fn peek(db: &DbInstance) -> i64 {
        // The wildcard is reachable when optional storage features add variants, but not in the
        // default all-targets gate where only mem + sqlite are compiled.
        #[allow(unreachable_patterns)]
        match db {
            DbInstance::Mem(i) => i.tt_clock().peek(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(i) => i.tt_clock().peek(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(i) => i.tt_clock().peek(),
            _ => panic!("unsupported engine in fixture"),
        }
    }
    let mut tts = [0i64; 4];
    db.run_default("?[k, v, x] <- [[1, [100, true], 1]] :put hist {k, v => x}")
        .unwrap();
    tts[0] = peek(&db);
    db.run_default("?[k, v, x] <- [[1, [200, true], 2]] :put hist {k, v => x}")
        .unwrap();
    tts[1] = peek(&db);
    db.run_default("?[k, v, x] <- [[1, [200, true], 3]] :put hist {k, v => x}")
        .unwrap();
    tts[2] = peek(&db);
    db.run_default("?[k, v, x] <- [[1, [300, false], 0]] :put hist {k, v => x}")
        .unwrap();
    tts[3] = peek(&db);
    (db, tts)
}

#[test]
fn bitemporal_four_quadrants() {
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quad.db");
        let (db, tts) = bitemporal_fixture(engine, path.to_str().unwrap());
        let q = |vt: i64, tt: i64| -> serde_json::Value {
            db.run_default(&format!("?[x] := *hist[k, v, t, x @ (vt: {vt}, tt: {tt})]"))
                .unwrap()
                .into_json()["rows"]
                .clone()
        };
        assert_eq!(q(250, tts[3]), serde_json::json!([[3]]), "{engine}");
        assert_eq!(q(150, tts[3]), serde_json::json!([[1]]), "{engine}");
        assert_eq!(q(250, tts[1]), serde_json::json!([[2]]), "{engine}");
        assert_eq!(q(250, tts[0]), serde_json::json!([[1]]), "{engine}");
        assert_eq!(q(350, tts[3]).as_array().unwrap().len(), 0, "{engine}");
        assert_eq!(q(350, tts[2]), serde_json::json!([[3]]), "{engine}");
        assert_eq!(q(50, tts[3]).as_array().unwrap().len(), 0, "{engine}");
    }
}

#[test]
fn bitemporal_bare_scan_is_current_belief_per_group() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db
        .run_default("?[v, x] := *hist[k, v, t, x]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0][0][0], 300);
    assert_eq!(rows[0][0][1], serde_json::json!(false));
    assert_eq!(rows[1][0][0], 200);
    assert_eq!(rows[1][1], 3, "correction wins in the bare scan: {rows:?}");
    assert_eq!(rows[2][0][0], 100);
    assert_eq!(rows[2][1], 1);

    let res = db
        .run_default(&format!("?[v, x] := *hist[k, v, t, x @ (tt: {})]", tts[1]))
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][1], 2);

    let res = db
        .run_default("?[x] := *hist[k, v, t, x @ 250]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));
}

/// The step-6 pinned-iterator seek override on RocksDB must answer exactly
/// like the generic probe path: four quadrants (resolve-key mode) + the bare
/// scan (resolve-groups mode) + raw ::history.
#[cfg(feature = "storage-rocksdb")]
#[test]
fn bitemporal_reads_on_rocksdb() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quad_rocks");
    let (db, tts) = bitemporal_fixture("rocksdb", path.to_str().unwrap());
    let q = |vt: i64, tt: i64| -> serde_json::Value {
        db.run_default(&format!("?[x] := *hist[k, v, t, x @ (vt: {vt}, tt: {tt})]"))
            .unwrap()
            .into_json()["rows"]
            .clone()
    };
    assert_eq!(q(250, tts[3]), serde_json::json!([[3]]));
    assert_eq!(q(150, tts[3]), serde_json::json!([[1]]));
    assert_eq!(q(250, tts[1]), serde_json::json!([[2]]));
    assert_eq!(q(250, tts[0]), serde_json::json!([[1]]));
    assert_eq!(q(350, tts[3]).as_array().unwrap().len(), 0);
    assert_eq!(q(350, tts[2]), serde_json::json!([[3]]));
    assert_eq!(q(50, tts[3]).as_array().unwrap().len(), 0);

    let res = db
        .run_default("?[v, x] := *hist[k, v, t, x]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0][0][0], 300);
    assert_eq!(rows[0][0][1], serde_json::json!(false));
    assert_eq!(rows[1][1], 3, "correction wins in the bare scan");
    assert_eq!(rows[2][1], 1);

    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 4);
}

#[test]
fn bitemporal_cessation_across_runs_and_ties() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create hr {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 7]] :put hr {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, false], 0]] :put hr {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *hr[k, v, t, x @ (vt: 100)]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"].as_array().unwrap().len(),
        0,
        "the later cessation must win across the is_assert-run boundary"
    );

    let db2 = DbInstance::new("mem", "", "").unwrap();
    db2.run_default(":create hr2 {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db2.run_default("?[k, v, x] <- [[1, [100, false], 0]] :put hr2 {k, v => x}")
        .unwrap();
    db2.run_default("?[k, v, x] <- [[1, [100, true], 9]] :put hr2 {k, v => x}")
        .unwrap();
    let res = db2
        .run_default("?[x] := *hr2[k, v, t, x @ (vt: 100)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]));
}

#[test]
fn bitemporal_repudiation_by_copy_and_chained_staleness() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create rep {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 100]] :put rep {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [300, true], 120]] :put rep {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [300, true], 100]] :put rep {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *rep[k, v, t, x @ (vt: 350)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[100]]));

    db.run_default("?[k, v, x] <- [[1, [100, true], 95]] :put rep {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *rep[k, v, t, x @ (vt: 350)]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[100]]),
        "copy is a snapshot"
    );
    let res = db
        .run_default("?[x] := *rep[k, v, t, x @ (vt: 150)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[95]]));
}

#[test]
fn bitemporal_negation_and_joins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("neg4b.db");
    let (db, _tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db
        .run_default("r[k] <- [[1], [2]] ?[k] := r[k], not *hist{k @ (vt: 250)}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[2]]));
    let res = db
        .run_default("r[k] <- [[1]] ?[k, x] := r[k], *hist{k, x @ (vt: 250)}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 3]]));
}

#[test]
fn as_of_pins_the_whole_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("asof.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    // also a tt-only relation in the same query
    db.run_default(":create audit_a {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_a {k => v}")
        .unwrap();
    let after_audit = match &db {
        DbInstance::Sqlite(i) => i.tt_clock().peek(),
        _ => panic!(),
    };
    db.run_default("?[k, v] <- [[1, 20]] :put audit_a {k => v}")
        .unwrap();

    // :as_of pins BOTH tt-stamped atoms; the plain relation is untouched
    db.run_default(":create plain_a {k => v: Int}").unwrap();
    db.run_default("?[k, v] <- [[1, 5]] :put plain_a {k => v}")
        .unwrap();
    let res = db
        .run_default(&format!(
            "?[x, w, p] := *hist[k, v, t, x @ (vt: 250)], *audit_a[k2, t2, w], *plain_a[k3, p] \
             :as_of {}",
            tts[1].max(after_audit)
        ))
        .unwrap()
        .into_json();
    // hist's group-200 belief at that point (post-correction): 3;
    // audit_a as of then: 10; plain untouched: 5
    assert_eq!(res["rows"], serde_json::json!([[3, 10, 5]]));

    // explicit per-atom selector wins over :as_of
    let res = db
        .run_default(&format!(
            "?[w] := *audit_a[k, t, w @ (tt: 'NOW')] :as_of {after_audit}"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[20]]));

    // :as_of with no tt-stamped relation in the query is an error
    let err = db
        .run_default("?[p] := *plain_a[k, p] :as_of 'NOW'")
        .expect_err(":as_of without tt relations must fail");
    let msg = format!("{err:?}")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(msg.contains("no transaction-time-stamped"), "{msg}");
}

#[test]
fn as_of_minimal_probe() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create audit_p2 {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10]] :put audit_p2 {k => v}")
        .unwrap();
    // no selector
    db.run_default("?[w] := *audit_p2[k, t, w] :as_of 'NOW'")
        .unwrap();
    // with explicit selector
    db.run_default("?[w] := *audit_p2[k, t, w @ (tt: 'NOW')] :as_of 'NOW'")
        .unwrap();
}

#[test]
fn temporal_join_columns_use_materialized_join() {
    // Regression: a join binding a temporal column used to clamp the prefix
    // scan to one vt-group — superseded/ceased values resurrected on sqlite,
    // BTreeMap::range panic on mem. The dispatch now falls back to a
    // materialized join over the RESOLVED scan.
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tj.db");
        let (db, _tts) = bitemporal_fixture(engine, path.to_str().unwrap());
        // join on (k, v): the belief at vt=120 (group 100) does not exist at
        // vt=250 (group 200 wins there) -> empty result
        let res = db
            .run_default(
                "l[k, v] := *hist[k, v, t, x @ (vt: 120)] \
                 ?[k, x2] := l[k, v], *hist[k, v, t2, x2 @ (vt: 250)]",
            )
            .unwrap()
            .into_json();
        assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{engine}");

        // negation twin: nothing at vt=250 carries group-100's vt value
        let res = db
            .run_default(
                "l[k, v] := *hist[k, v, t, x @ (vt: 120)] \
                 ?[k] := l[k, v], not *hist{k, v @ (vt: 250)}",
            )
            .unwrap()
            .into_json();
        assert_eq!(res["rows"], serde_json::json!([[1]]), "{engine}");

        // full-binding self-join with a value mismatch must be empty
        let res = db
            .run_default(
                "l[k, v, t, x] := *hist[k, v, t, x0 @ (vt: 250)], x = x0 - 1 \
                 ?[k, x] := l[k, v, t, x], *hist[k, v, t, x @ (vt: 250)]",
            )
            .unwrap()
            .into_json();
        assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{engine}");
    }
}

#[test]
fn vt_only_temporal_join_columns_fixed_too() {
    // The same defect class pre-existed upstream on the single-axis path.
    for engine in ["mem", "sqlite"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vtj.db");
        let db = DbInstance::new(engine, path.to_str().unwrap(), "").unwrap();
        db.run_default(":create vtx {k, v: Validity => x: Int}")
            .unwrap();
        db.run_default("?[k, v, x] <- [[1, [100, true], 7]] :put vtx {k, v => x}")
            .unwrap();
        db.run_default("?[k, v, x] <- [[1, [150, false], 0]] :put vtx {k, v => x}")
            .unwrap();
        // the belief at vt=120 (group 100) is retracted by vt=250: join empty
        let res = db
            .run_default("l[k, v] := *vtx[k, v, x @ 120] ?[k, x2] := l[k, v], *vtx[k, v, x2 @ 250]")
            .unwrap()
            .into_json();
        assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{engine}");
    }
}

#[test]
fn bitemporal_migration_invariant_comparative() {
    // §9: adding `tt: TxTime` to a vt relation changes no existing query's
    // results (up to corrections) — same puts into a vt twin, diff results.
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create m_vt {k, v: Validity => x: Int}")
        .unwrap();
    db.run_default(":create m_bt {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    for rel in ["m_vt", "m_bt"] {
        db.run_default(&format!(
            "?[k, v, x] <- [[1, [100, true], 1], [2, [100, true], 5]] :put {rel} {{k, v => x}}"
        ))
        .unwrap();
        db.run_default(&format!(
            "?[k, v, x] <- [[1, [200, true], 2]] :put {rel} {{k, v => x}}"
        ))
        .unwrap();
        db.run_default(&format!(
            "?[k, v, x] <- [[2, [300, false], 0]] :put {rel} {{k, v => x}}"
        ))
        .unwrap();
    }
    // bare scan
    let a = db
        .run_default("?[k, v, x] := *m_vt[k, v, x]")
        .unwrap()
        .into_json();
    let b = db
        .run_default("?[k, v, x] := *m_bt[k, v, t, x]")
        .unwrap()
        .into_json();
    assert_eq!(a["rows"], b["rows"], "bare scan must match the vt twin");
    // several @V points
    for vt in [50, 100, 150, 200, 250, 300, 350] {
        let a = db
            .run_default(&format!("?[k, x] := *m_vt[k, v, x @ {vt}]"))
            .unwrap()
            .into_json();
        let b = db
            .run_default(&format!("?[k, x] := *m_bt[k, v, t, x @ {vt}]"))
            .unwrap()
            .into_json();
        assert_eq!(a["rows"], b["rows"], "@{vt} must match the vt twin");
    }
}

#[test]
fn vt_equal_ts_assert_shadows_retract_pinned() {
    // §9: the previously-unpinned single-axis equal-ts behavior — an assert
    // and a retract at the SAME vt ts leave the assert visible (assert sorts
    // first; the skip-scan emits the first qualifying row).
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create eq_vt {k, v: Validity => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 7]] :put eq_vt {k, v => x}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, false], 0]] :put eq_vt {k, v => x}")
        .unwrap();
    let res = db
        .run_default("?[x] := *eq_vt[k, v, x @ 100]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[7]]));
}

// ==== mnestic fork: 4c existence-checking writes + bitemporal :rm ====

#[test]
fn tt_insert_update_ensure() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create a4c {k, tt: TxTime => v: Int, w: Int default 0}")
        .unwrap();

    // insert new key OK
    db.run_default("?[k, v] <- [[1, 10]] :insert a4c {k => v}")
        .unwrap();
    // insert existing key fails
    let err = db
        .run_default("?[k, v] <- [[1, 11]] :insert a4c {k => v}")
        .expect_err("insert existing must fail");
    assert!(format!("{err:?}").contains("exists"), "{err:?}");
    // rm, then re-insert of the believed-deleted key succeeds
    db.run_default("?[k] <- [[1]] :rm a4c {k}").unwrap();
    db.run_default("?[k, v] <- [[1, 12]] :insert a4c {k => v}")
        .unwrap();
    let res = db
        .run_default("?[v] := *a4c[k, tt, v, w]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[12]]));
    // insert + put same key in one tx rejected
    let err = db
        .run_default(
            "{?[k, v] <- [[7, 1]] :put a4c {k => v}} {?[k, v] <- [[7, 2]] :insert a4c {k => v}}",
        )
        .expect_err("insert-after-put same tx must fail");
    assert!(format!("{err:?}").contains("already written"), "{err:?}");

    // update merges provided columns over the current belief
    db.run_default("?[k, w] <- [[1, 5]] :update a4c {k => w}")
        .unwrap();
    let res = db
        .run_default("?[v, w] := *a4c[k, tt, v, w]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[12, 5]]),
        "v kept, w updated"
    );
    // update of a missing key fails
    let err = db
        .run_default("?[k, w] <- [[99, 5]] :update a4c {k => w}")
        .expect_err("update missing must fail");
    assert!(format!("{err:?}").contains("does not exist"), "{err:?}");

    // ensure passes on matching current belief, fails on mismatch
    db.run_default("?[k, v] <- [[1, 12]] :ensure a4c {k => v}")
        .unwrap();
    let err = db
        .run_default("?[k, v] <- [[1, 999]] :ensure a4c {k => v}")
        .expect_err("ensure mismatch must fail");
    assert!(format!("{err:?}").contains("mismatch"), "{err:?}");
    // ensure_not passes on missing, fails on existing
    db.run_default("?[k, v] <- [[404, 0]] :ensure_not a4c {k => v}")
        .unwrap();
    let err = db
        .run_default("?[k, v] <- [[1, 0]] :ensure_not a4c {k => v}")
        .expect_err("ensure_not existing must fail");
    assert!(format!("{err:?}").contains("exists"), "{err:?}");
}

#[test]
fn bitemporal_rm_remap_cessation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rm4c.db");
    let (db, _tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    // cease the key at vt=400 via :rm — values come from the belief at 400
    db.run_default("?[k, v] <- [[1, [400, true]]] :rm hist {k, v}")
        .unwrap();
    // hmm: the fixture already retracted at vt=300, so belief at 400 is
    // deleted — the rm is a no-op; use vt=250 instead where belief = 3
    db.run_default("?[k, v] <- [[1, [250, true]]] :rm hist {k, v}")
        .unwrap();
    let res = db
        .run_default("?[x] := *hist[k, v, t, x @ (vt: 260)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "ceased at 250");
    // belief below 250 unaffected
    let res = db
        .run_default("?[x] := *hist[k, v, t, x @ (vt: 240)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));
    // :delete at a vt with no belief errors
    let err = db
        .run_default("?[k, v] <- [[1, [50, true]]] :delete hist {k, v}")
        .expect_err("delete with no belief must fail");
    assert!(format!("{err:?}").contains("no belief"), "{err:?}");

    // bitemporal insert/update on the (vt=NOW) current belief
    db.run_default(":create bi4c {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 5]] :put bi4c {k, v => x}")
        .unwrap();
    let err = db
        .run_default("?[k, v, x] <- [[1, [200, true], 6]] :insert bi4c {k, v => x}")
        .expect_err("bitemporal insert on a key with recorded beliefs must fail");
    assert!(format!("{err:?}").contains("recorded beliefs"), "{err:?}");
    db.run_default("?[k, x] <- [[1, 9]] :update bi4c {k => x}")
        .unwrap();
    let res = db
        .run_default("?[v, x] := *bi4c[k, v, t, x @ (vt: 150)]")
        .unwrap()
        .into_json();
    // the correction lands in group 100 (the current belief's own group)
    assert_eq!(res["rows"], serde_json::json!([[[100, true], 9]]));
}

#[test]
fn imperative_return_braced_clause_no_panic() {
    // mnestic fork fix: `%return { <query> }` panicked with unreachable!()
    // (upstream bug — the match arm expected query_script_inner but the
    // grammar delivers imperative_clause).
    let db = DbInstance::new("mem", "", "").unwrap();
    let res = db
        .run_default(
            r#"
        {:create _t_ret {a}}
        %return { ?[x] <- [[1]] }
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1]]));
}

#[test]
fn tt_4c_review_pins() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create pin4c {k, tt: TxTime => v: Int}")
        .unwrap();
    // duplicate key within ONE :insert statement is rejected (was silent LWW)
    let err = db
        .run_default("?[k, v] <- [[7, 1], [7, 2]] :insert pin4c {k => v}")
        .expect_err("duplicate in-statement insert must fail");
    assert!(format!("{err:?}").contains("duplicate key"), "{err:?}");

    // ensure with a bound tt column is rejected (was silently ignored)
    db.run_default("?[k, v] <- [[1, 10]] :put pin4c {k => v}")
        .unwrap();
    let err = db
        .run_default("?[k, tt, v] <- [[1, 5, 10]] :ensure pin4c {k, tt => v}")
        .expect_err("tt-bound ensure must fail");
    assert!(format!("{err:?}").contains("engine-assigned"), "{err:?}");

    // ensure of a key rewritten in the same tx is an ambiguous assertion
    let err = db
        .run_default(
            "{?[k, v] <- [[1, 99]] :put pin4c {k => v}} {?[k, v] <- [[1, 99]] :ensure pin4c {k => v}}",
        )
        .expect_err("ensure of same-tx rewrite must fail");
    assert!(format!("{err:?}").contains("ambiguous"), "{err:?}");

    // bitemporal: ensure with a bound vt column is rejected
    db.run_default(":create pin_bi {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db.run_default("?[k, v, x] <- [[1, [100, true], 5]] :put pin_bi {k, v => x}")
        .unwrap();
    let err = db
        .run_default("?[k, v, x] <- [[1, [100, true], 5]] :ensure pin_bi {k, v => x}")
        .expect_err("vt-bound ensure must fail");
    assert!(format!("{err:?}").contains("CURRENT belief"), "{err:?}");

    // update after cessation fails; tt-past read shows pre-update value
    let db2 = DbInstance::new("mem", "", "").unwrap();
    db2.run_default(":create pin_c {k, v: Validity, tt: TxTime => x: Int}")
        .unwrap();
    db2.run_default("?[k, v, x] <- [[1, [100, true], 5]] :put pin_c {k, v => x}")
        .unwrap();
    let DbInstance::Mem(inner) = &db2 else {
        panic!()
    };
    let before = inner.tt_clock().peek() + 1;
    db2.run_default("?[k, x] <- [[1, 9]] :update pin_c {k => x}")
        .unwrap();
    let res = db2
        .run_default(&format!(
            "?[x] := *pin_c[k, v, t, x @ (vt: 150, tt: {before})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[5]]),
        "tt-past shows pre-update"
    );
    db2.run_default("?[k, v] <- [[1, [200, true]]] :rm pin_c {k, v}")
        .unwrap();
    let err = db2
        .run_default("?[k, x] <- [[1, 11]] :update pin_c {k => x}")
        .expect_err("update after cessation must fail");
    assert!(format!("{err:?}").contains("does not exist"), "{err:?}");
}

// ==== mnestic fork: step 5 sys ops ====

#[test]
fn history_sysop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h5.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    let rows = res["rows"].as_array().unwrap();
    // 4 physical records: retract@300, correction+original@200, assert@100
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(
        res["headers"],
        serde_json::json!(["k", "vt_ts", "op", "tt", "x"])
    );
    assert_eq!(rows[0][1], 300);
    assert_eq!(rows[0][2], "retract");
    assert_eq!(rows[1][1], 200);
    assert_eq!(rows[1][2], "assert");
    assert!(rows[1][3].as_i64().unwrap() <= tts[2] && rows[1][3].as_i64().unwrap() > tts[1]);
    // limit/offset
    let res = db
        .run_default("::history hist [[1]] 2 1")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 2);
    // tt-only shape has no vt_ts column
    db.run_default(":create a5 {k, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[9, 1]] :put a5 {k => v}")
        .unwrap();
    db.run_default("?[k] <- [[9]] :rm a5 {k}").unwrap();
    let res = db.run_default("::history a5 [[9]]").unwrap().into_json();
    assert_eq!(res["headers"], serde_json::json!(["k", "op", "tt", "v"]));
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], "retract");
    // non-tt relation errors
    db.run_default(":create plain5 {k => v: Int}").unwrap();
    let err = db
        .run_default("::history plain5 [[1]]")
        .expect_err("must fail");
    assert!(
        format!("{err:?}").contains("requires a TxTime relation"),
        "{err:?}"
    );
}

#[test]
fn history_gc_sysop_and_floor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gc5.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    // cutoff between the correction (tts[2]) and the retraction (tts[3]):
    // group 200 keeps only the correction (its belief at cutoff); the
    // superseded original@200 is dropped; groups 100/300 untouched.
    let cutoff = tts[2] + 1;
    let res = db
        .run_default(&format!("::history_gc hist {cutoff}"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], 1, "exactly the superseded row dropped");
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 3);
    // as-of at/above the cutoff still answers correctly
    let res = db
        .run_default(&format!(
            "?[x] := *hist[k, v, t, x @ (vt: 250, tt: {cutoff})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));
    // below the floor errors
    let err = db
        .run_default(&format!(
            "?[x] := *hist[k, v, t, x @ (vt: 250, tt: {})]",
            tts[0]
        ))
        .expect_err("below-floor read must fail");
    assert!(format!("{err:?}").contains("gc floor"), "{err:?}");
    // read-only guard
    let err = db
        .run_script(
            &format!("::history_gc hist {cutoff}"),
            Default::default(),
            ScriptMutability::Immutable,
        )
        .expect_err("gc in read-only must fail");
    assert!(format!("{err:?}").contains("read-only"), "{err:?}");
}

#[test]
fn evict_sysop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ev5.db");
    let (db, _tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());
    let res = db.run_default("::evict hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"][0][2], 4, "all four records hard-deleted");
    // gone from history and reads
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0);
    // audit row exists with a hash marker (not the key), and an eviction tt
    let res = db
        .run_default("?[r, key, n] := *mnestic_evict_audit[r, key, tt, n]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], "hist");
    assert_eq!(rows[0][2], 4);
    let marker = rows[0][1].as_str().unwrap();
    assert!(
        !marker.contains('1') || marker.len() == 36,
        "salted uuid marker: {marker}"
    );
    // unredacted opts out
    db.run_default("?[k, v, x] <- [[2, [100, true], 5]] :put hist {k, v => x}")
        .unwrap();
    db.run_default("::evict hist [[2]] unredacted").unwrap();
    let res = db
        .run_default("?[key] := *mnestic_evict_audit[r, key, tt, n], n == 1")
        .unwrap()
        .into_json();
    assert!(res["rows"][0][0].as_str().unwrap().contains('2'), "{res:?}");
    // read-only guard
    let err = db
        .run_script(
            "::evict hist [[2]]",
            Default::default(),
            ScriptMutability::Immutable,
        )
        .expect_err("evict in read-only must fail");
    assert!(format!("{err:?}").contains("read-only"), "{err:?}");
}

#[test]
fn step5_review_pins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pins5.db");
    let (db, tts) = bitemporal_fixture("sqlite", path.to_str().unwrap());

    // same-tx pending tt writes would be stamped AFTER the eviction's
    // deletes and resurrect the key: the whole script must fail
    let err = db
        .run_default(
            "{?[k, v, x] <- [[1, [500, true], 9]] :put hist {k, v => x}} {::evict hist [[1]]}",
        )
        .expect_err("evict with pending tt writes must fail");
    assert!(
        format!("{err:?}").contains("pending transaction-time writes"),
        "{err:?}"
    );
    let res = db.run_default("::history hist [[1]]").unwrap().into_json();
    assert_eq!(
        res["rows"].as_array().unwrap().len(),
        4,
        "nothing committed"
    );

    // a no-op gc deletes nothing, so it must not raise the (irreversible)
    // floor — past reads stay exact
    let res = db.run_default("::history_gc hist 1").unwrap().into_json();
    assert_eq!(res["rows"][0][0], 0);
    assert_eq!(res["rows"][0][1], serde_json::Value::Null);
    let res = db
        .run_default(&format!(
            "?[x] := *hist[k, v, t, x @ (vt: 250, tt: {})]",
            tts[0]
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1]]));

    // a future cutoff can only be a typo
    let err = db
        .run_default("::history_gc hist 99999999999999999")
        .expect_err("future cutoff must fail");
    assert!(format!("{err:?}").contains("future"), "{err:?}");

    // the report carries the EFFECTIVE floor: an older-cutoff re-run does
    // not lower (or echo below) it
    let cutoff = tts[2] + 1;
    let res = db
        .run_default(&format!("::history_gc hist {cutoff}"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], 1);
    assert_eq!(res["rows"][0][1].as_i64().unwrap(), cutoff);
    let res = db.run_default("::history_gc hist 5").unwrap().into_json();
    assert_eq!(res["rows"][0][0], 0);
    assert_eq!(
        res["rows"][0][1].as_i64().unwrap(),
        cutoff,
        "floor not lowered"
    );

    // an imperative program containing only a destructive sysop must get a
    // write transaction (on RocksDB the read-tx bridge rejects writes)
    db.run_default(&format!("{{::history_gc hist {cutoff}}}"))
        .unwrap();

    // access levels guard the destructive ops and history reads
    db.run_default("::access_level read_only hist").unwrap();
    let err = db
        .run_default("::evict hist [[1]]")
        .expect_err("read_only evict");
    assert!(
        format!("{err:?}").contains("Insufficient access level"),
        "{err:?}"
    );
    let err = db
        .run_default(&format!("::history_gc hist {cutoff}"))
        .expect_err("read_only gc");
    assert!(
        format!("{err:?}").contains("Insufficient access level"),
        "{err:?}"
    );
    db.run_default("::access_level hidden hist").unwrap();
    let err = db
        .run_default("::history hist [[1]]")
        .expect_err("hidden history");
    assert!(
        format!("{err:?}").contains("Insufficient access level"),
        "{err:?}"
    );
    db.run_default("::access_level normal hist").unwrap();

    // duplicate keys in one ::evict: one audit row with the true count (a
    // repeat would overwrite it with rows_deleted = 0)
    let res = db
        .run_default("::evict hist [[1], [1]]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 1);
    assert_eq!(res["rows"][0][2], 3);
    let res = db
        .run_default("?[n] := *mnestic_evict_audit[r, key, tt, n]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[3]]));

    // keys coerce through the column types: a mistyped ::evict key errors
    // loudly instead of silently evicting nothing
    db.run_default(":create typed5 {k: Int, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[7, 1], [8, 2]] :put typed5 {k => v}")
        .unwrap();
    assert!(
        db.run_default("::evict typed5 [['7']]").is_err(),
        "mistyped key must be loud"
    );

    // ::history output is key-ascending; limit/offset are strict pos_ints
    // (`2 -1` must not silently parse as the single limit `2 - 1`)
    let res = db
        .run_default("::history typed5 [[8], [7]]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"][0][0], 7, "key-asc order");
    assert!(
        db.run_default("::history typed5 [[7]] 2 -1").is_err(),
        "negative offset must not parse"
    );

    // synthesized history headers must not shadow user columns
    db.run_default(":create clash5 {k, tt: TxTime => op: String}")
        .unwrap();
    let err = db
        .run_default("::history clash5 [[1]]")
        .expect_err("header collision");
    assert!(format!("{err:?}").contains("collides"), "{err:?}");

    // the audit relation name is reserved: a pre-existing relation with a
    // divergent schema would be corrupted by the raw audit puts
    let dir2 = tempfile::tempdir().unwrap();
    let path2 = dir2.path().join("pins5b.db");
    let (db2, _) = bitemporal_fixture("sqlite", path2.to_str().unwrap());
    db2.run_default(":create mnestic_evict_audit {a => b: Float}")
        .unwrap();
    let err = db2
        .run_default("::evict hist [[1]]")
        .expect_err("reserved name");
    assert!(format!("{err:?}").contains("reserved"), "{err:?}");
}

// ==== mnestic fork: provenance semirings R1 — bounded-meet (top-k proofs) ====

#[test]
fn bounded_meet_top_k_basics() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // non-recursive: the k lowest-cost packs per group, one row each,
    // cost-ordered; a group with fewer than k keeps them all
    let res = db
        .run_default(
            r#"
        data[g, pack] <- [[1, ['a', 3.0]], [1, ['b', 1.0]], [1, ['c', 2.0]],
                          [1, ['d', 4.0]], [2, ['e', 5.0]]]
        ?[g, best] := data[g, pack], best = pack
        "#,
        )
        .unwrap();
    assert_eq!(res.rows.len(), 5, "sanity: data visible");
    let res = db
        .run_default(
            r#"
        data[g, pack] <- [[1, ['a', 3.0]], [1, ['b', 1.0]], [1, ['c', 2.0]],
                          [1, ['d', 4.0]], [2, ['e', 5.0]]]
        ?[g, min_cost_k(pack, 3)] := data[g, pack]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([
            [1, ["b", 1.0]],
            [1, ["c", 2.0]],
            [1, ["a", 3.0]],
            [2, ["e", 5.0]]
        ]),
        "{res:?}"
    );
    // no grouping columns: one global k-set
    let res = db
        .run_default(
            r#"
        data[g, pack] <- [[1, ['a', 3.0]], [1, ['b', 1.0]], [2, ['e', 5.0]]]
        ?[min_cost_k(pack, 2)] := data[g, pack]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[["b", 1.0]], [["a", 3.0]]]),
        "{res:?}"
    );
}

#[test]
fn bounded_meet_k_shortest_paths() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // 1→2→3 (2.0) beats 1→3 (3.0); the 3→1 back-edge creates cycles whose
    // paths all cost ≥ 4 — the top-2 must converge despite them
    let res = db
        .run_default(
            r#"
        edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0], [3, 1, 1.0]]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[pack] := sp[3, pack]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]], [[[1, 3], 3.0]]]),
        "{res:?}"
    );
}

#[test]
fn bounded_meet_divergence_capped() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // a negative-cost cycle improves the k-set forever: the changed-bit
    // never settles, the epoch cap must convert that into a loud error
    let err = db
        .run_default(
            r#"
        edge[f, t, w] <- [[1, 2, -1.0], [2, 1, -1.0]]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[pack] := sp[2, pack]
        "#,
        )
        .expect_err("negative cycle must hit the epoch cap");
    assert!(format!("{err:?}").contains("did not converge"), "{err:?}");
}

#[test]
fn bounded_meet_relay_recursion_unstratifiable() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // pins the in-SCC half of the divergence guard's structural
    // precondition: cyclic recursion into a bounded-meet rule through a
    // relay is rejected outright (the cross-SCC half — poisoned edges into
    // an aggregated rule forced across a stratum boundary — is what keeps
    // acyclic feeders out; together they leave a bounded rule's own delta
    // as its only in-stratum input, so its changed epochs form a contiguous
    // prefix). If either half is ever relaxed, a displacement cycle could
    // improve the k-set only every other epoch — the epoch cap counts TOTAL
    // changed epochs (not a streak resetting on quiet epochs) so it stays
    // sound in that world too
    let err = db
        .run_default(
            r#"
        edge[f, t, w] <- [[1, 2, -1.0], [2, 1, -1.0]]
        relay[m, p] := sp[m, p]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := relay[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[pack] := sp[2, pack]
        "#,
        )
        .expect_err("relay recursion through a bounded-meet head must not stratify");
    assert!(format!("{err:?}").contains("unstratifiable"), "{err:?}");
}

#[test]
fn meet_bit_and_or_report_changes_accurately() {
    use crate::data::aggr::{MeetAggrBitAnd, MeetAggrBitOr, MeetAggrObj};
    // mnestic fork fix: a non-changing AND/OR must report false, or stable
    // values re-enter the semi-naive delta every epoch (the bool variants
    // were fixed earlier; the byte variants had the same defect)
    let and = MeetAggrBitAnd;
    let mut v = DataValue::Bytes(vec![0xf0]);
    assert!(!and.update(&mut v, &DataValue::Bytes(vec![0xff])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xf0]));
    assert!(and.update(&mut v, &DataValue::Bytes(vec![0x0f])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0x00]));

    let or = MeetAggrBitOr;
    let mut v = DataValue::Bytes(vec![0xff]);
    assert!(!or.update(&mut v, &DataValue::Bytes(vec![0x0f])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xff]));
    let mut v = DataValue::Bytes(vec![0x0f]);
    assert!(or.update(&mut v, &DataValue::Bytes(vec![0xf0])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xff]));

    // first contact with the empty-bytes init_val sentinel seeds the lazy
    // identity from the operand and MUST report changed — this is the
    // branch the eval-side empty-rule seeding relies on for a later real
    // row to enter the semi-naive delta
    let mut v = DataValue::Bytes(vec![]);
    assert!(and.update(&mut v, &DataValue::Bytes(vec![0xff])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0xff]));
    let mut v = DataValue::Bytes(vec![]);
    assert!(or.update(&mut v, &DataValue::Bytes(vec![0x0f])).unwrap());
    assert_eq!(v, DataValue::Bytes(vec![0x0f]));
}

#[test]
fn bounded_meet_validation() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default("?[g, pack] <- [[1, ['a', 1.0]]] :create bm {g, pack}")
        .unwrap();
    // missing k
    let err = db
        .run_default("?[g, min_cost_k(pack)] := *bm[g, pack]")
        .expect_err("missing k");
    assert!(
        format!("{err:?}").contains("exactly one argument"),
        "{err:?}"
    );
    // non-positive k
    let err = db
        .run_default("?[g, min_cost_k(pack, 0)] := *bm[g, pack]")
        .expect_err("k = 0");
    assert!(format!("{err:?}").contains("positive integer"), "{err:?}");
    // mixed with another aggregate
    let err = db
        .run_default("?[min_cost_k(pack, 2), count(g)] := *bm[g, pack]")
        .expect_err("mixed head");
    assert!(
        format!("{err:?}").contains("bounded-meet aggregate"),
        "{err:?}"
    );
    // not in the last position
    let err = db
        .run_default("?[min_cost_k(pack, 2), g] := *bm[g, pack]")
        .expect_err("not last");
    assert!(
        format!("{err:?}").contains("bounded-meet aggregate"),
        "{err:?}"
    );
    // malformed pack
    let err = db
        .run_default("?[g, min_cost_k(g, 2)] := *bm[g, pack]")
        .expect_err("bad pack");
    assert!(
        format!("{err:?}").contains("cannot compute 'min_cost_k'"),
        "{err:?}"
    );
}

#[test]
fn bounded_meet_does_not_cap_costratified_recursion() {
    let db = DbInstance::new("mem", "", "").unwrap();
    // a CONVERGED (here: non-recursive) bounded rule sharing a stratum with
    // an unrelated recursion needing more epochs than the cap must not kill
    // it — the guard counts only epochs in which some k-set actually changed
    let res = db
        .run_default(
            r#"
        base[pack] <- [[['a', 1.0]], [['b', 2.0]]]
        best[min_cost_k(pack, 2)] := base[pack]
        w[a] := a = 0
        w[a] := w[b], a = b + 1, a < 4300
        ?[count(a)] := w[a], best[p]
        "#,
        )
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[4300 * 2]]), "{res:?}");
}

// ==== mnestic fork: provenance semirings R2 — annotations persist in rows ====

/// R2's acceptance criterion — "an annotated derivation is materialized and
/// queryable without recompute" — is met by the tags-as-columns architecture
/// with NO row-format change: annotation values are ordinary DataValues, so
/// `:put` of an annotated query output persists them in the existing
/// memcomparable row format. This test pins the four contracts (including
/// the composition with the bitemporal tt axis) across a real reopen.
#[test]
fn semiring_tags_persist_in_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r2.db");
    let p = path.to_str().unwrap();
    {
        let db = DbInstance::new("sqlite", p, "").unwrap();
        // (a) meet-annotated derivation → stored relation
        db.run_default(":create sp_out {dst: Int => pack}").unwrap();
        let sp_script = |put: &str| {
            format!(
                r#"
            edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0]]
            sp[t, min_cost(pack)] := t = 1, pack = [[1], 0.0]
            sp[t, min_cost(pack)] := sp[m, p], edge[m, t, w],
                                     pack = [concat(first(p), [t]), last(p) + w]
            ?[dst, pack] := sp[dst, pack]
            {put}
            "#
            )
        };
        db.run_default(&sp_script(":put sp_out {dst => pack}"))
            .unwrap();
        // (b) bounded-meet k rows per group → stored relation (pack in key)
        db.run_default(":create topk_out {dst: Int, pack => }")
            .unwrap();
        db.run_default(
            r#"
            edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0]]
            sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
            sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                          pack = [concat(first(p), [t]), last(p) + w]
            ?[dst, pack] := sp[dst, pack]
            :put topk_out {dst, pack}
            "#,
        )
        .unwrap();
        // (c) annotated + tt: materialized beliefs carry engine-stamped
        // transaction time — annotated belief HISTORY
        db.run_default(":create belief {dst: Int, tt: TxTime => pack}")
            .unwrap();
        db.run_default(&sp_script(":put belief {dst => pack}"))
            .unwrap();
        // a later, cheaper route to 3 changes the belief
        db.run_default(
            r#"
            edge[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 0.5]]
            sp[t, min_cost(pack)] := t = 1, pack = [[1], 0.0]
            sp[t, min_cost(pack)] := sp[m, p], edge[m, t, w],
                                     pack = [concat(first(p), [t]), last(p) + w]
            ?[dst, pack] := sp[dst, pack]
            :put belief {dst => pack}
            "#,
        )
        .unwrap();
        // (d) custom-aggregate annotations materialize; the operator itself
        // is registration-scoped
        db.register_custom_aggr("fuse2".to_string(), true, || Box::new(TestMaxi))
            .unwrap();
        db.run_default(":create fused {k: Int => v}").unwrap();
        db.run_default(
            r#"
            data[k, v] <- [[1, 0.9], [1, 0.5]]
            agg[k, fuse2(v)] := data[k, v]
            ?[k, v] := agg[k, v]
            :put fused {k => v}
            "#,
        )
        .unwrap();
    }
    // reopen: no re-registration, everything readable without recompute
    let db = DbInstance::new("sqlite", p, "").unwrap();
    let res = db
        .run_default("?[dst, pack] := *sp_out[dst, pack]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[1, [[1], 0.0]], [2, [[1, 2], 1.0]], [3, [[1, 2, 3], 2.0]]]),
        "{res:?}"
    );
    let res = db
        .run_default("?[dst, pack] := *topk_out[dst, pack]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 4, "{res:?}");
    // current belief = the cheaper corrected route
    let res = db
        .run_default("?[pack] := *belief[3, t, pack]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[[[1, 3], 0.5]]]), "{res:?}");
    // annotated belief HISTORY: both materializations recorded with their tts
    let res = db
        .run_default("::history belief [[3]]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    // as-of the FIRST materialization's tt, the old belief answers
    let first_tt = rows[1][2].as_i64().unwrap();
    let res = db
        .run_default(&format!(
            "?[pack] := *belief[3, t, pack @ (tt: {first_tt})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]]]),
        "{res:?}"
    );
    // (d) materialized custom-aggregate output readable with NO registry…
    let res = db
        .run_default("?[k, v] := *fused[k, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 0.9]]), "{res:?}");
    // …while re-computing loudly requires the registration
    let err = db
        .run_default("data[k, v] <- [[1, 0.9]] ?[k, fuse2(v)] := data[k, v]")
        .expect_err("unregistered aggregate must not resolve");
    assert!(format!("{err:?}").contains("not found"), "{err:?}");
}

// ==== mnestic fork: provenance semirings R3 — :reconcile (belief revision) ====

#[test]
fn reconcile_tt_only_belief_revision() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create facts {k: Int, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default("?[k, v] <- [[1, 10], [2, 20], [3, 30]] :reconcile facts {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *facts[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[1, 10], [2, 20], [3, 30]]),
        "{res:?}"
    );
    // revision: 1 unchanged, 2 changed, 3 gone, 4 new
    db.run_default("?[k, v] <- [[1, 10], [2, 25], [4, 40]] :reconcile facts {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *facts[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[1, 10], [2, 25], [4, 40]]),
        "{res:?}"
    );
    // unchanged key 1: exactly ONE record (no history bloat)
    let res = db.run_default("::history facts [[1]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 1, "{res:?}");
    // retracted key 3: assert + retract, and the as-of read still answers
    let res = db.run_default("::history facts [[3]]").unwrap().into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0][1], "retract");
    let first_tt = rows[1][2].as_i64().unwrap();
    let res = db
        .run_default(&format!("?[v] := *facts[3, t, v @ (tt: {first_tt})]"))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[30]]), "{res:?}");
    // idempotence: an identical reconcile buffers nothing
    db.run_default("?[k, v] <- [[1, 10], [2, 25], [4, 40]] :reconcile facts {k => v}")
        .unwrap();
    let res = db.run_default("::history facts [[2]]").unwrap().into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 2, "{res:?}");
}

#[test]
fn reconcile_bitemporal_cessations() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create bt {k: Int, vld: Validity, tt: TxTime => v: Int}")
        .unwrap();
    db.run_default(
        "?[k, vld, v] <- [[1, [100, true], 7], [1, [200, true], 8]] :reconcile bt {k, vld => v}",
    )
    .unwrap();
    // revision: group 100 corrected, group 200 dropped (cessation)
    db.run_default("?[k, vld, v] <- [[1, [100, true], 9]] :reconcile bt {k, vld => v}")
        .unwrap();
    let res = db
        .run_default("?[x] := *bt[k, v, t, x @ (vt: 150)]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[9]]), "corrected: {res:?}");
    let res = db
        .run_default("?[x] := *bt[k, v, t, x @ (vt: 250)]")
        .unwrap()
        .into_json();
    // group 200 ceased: from vt 200 onward the fact is believed-deleted
    // (spec §3 — a deciding group does NOT fall through to older groups)
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{res:?}");
    // the cessation is recorded, not erased: as-of the FIRST event's tt the
    // old belief at vt 250 was 8
    let res = db.run_default("::history bt [[1]]").unwrap().into_json();
    let rows = res["rows"].as_array().unwrap();
    let first_tt = rows.iter().map(|r| r[3].as_i64().unwrap()).min().unwrap();
    let res = db
        .run_default(&format!(
            "?[x] := *bt[k, v, t, x @ (vt: 250, tt: {first_tt})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[8]]), "{res:?}");
}

/// The R3 acceptance scenario: retract a base fact, re-derive, reconcile —
/// derived annotations stay consistent, and "what did we believe, and why,
/// as of T" answers across the revision.
#[test]
fn reconcile_tms_retraction_end_to_end() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create base {f: Int, t: Int, tt: TxTime => w: Float}")
        .unwrap();
    db.run_default("?[f, t, w] <- [[1, 2, 1.0], [2, 3, 1.0], [1, 3, 3.0]] :put base {f, t => w}")
        .unwrap();
    // derived, annotated: top-2 cheapest paths per destination (proofs in key)
    db.run_default(":create paths {dst: Int, pack, tt: TxTime => }")
        .unwrap();
    let derive = r#"
        edge[f, t, w] := *base[f, t, ttx, w]
        sp[t, min_cost_k(pack, 2)] := t = 1, pack = [[1], 0.0]
        sp[t, min_cost_k(pack, 2)] := sp[m, p], edge[m, t, w],
                                      pack = [concat(first(p), [t]), last(p) + w]
        ?[dst, pack] := sp[dst, pack]
        :reconcile paths {dst, pack}
    "#;
    db.run_default(derive).unwrap();
    let res = db
        .run_default("?[pack] := *paths[3, pack, ttx]")
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]], [[[1, 3], 3.0]]]),
        "{res:?}"
    );
    // retract a base fact (the cheap 2→3 edge), re-derive, reconcile
    db.run_default("?[f, t] <- [[2, 3]] :rm base {f, t}")
        .unwrap();
    db.run_default(derive).unwrap();
    // derived annotations consistent with the post-retraction base: the
    // proof through the retracted edge is GONE, not orphaned
    let res = db
        .run_default("?[pack] := *paths[3, pack, ttx]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[[[1, 3], 3.0]]]), "{res:?}");
    // …and the old belief plus its justification still answers as-of T
    let res = db
        .run_default("::history paths [[3, [[1, 2, 3], 2.0]]]")
        .unwrap()
        .into_json();
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "assert then retract: {rows:?}");
    let first_tt = rows[1][3].as_i64().unwrap();
    let res = db
        .run_default(&format!(
            "?[pack] := *paths[3, pack, ttx @ (tt: {first_tt})]"
        ))
        .unwrap()
        .into_json();
    assert_eq!(
        res["rows"],
        serde_json::json!([[[[1, 2, 3], 2.0]], [[[1, 3], 3.0]]]),
        "the pre-retraction annotated belief, with proofs: {res:?}"
    );
}

#[test]
fn reconcile_validation() {
    let db = DbInstance::new("mem", "", "").unwrap();
    db.run_default(":create plain_r {k: Int => v: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, v] <- [[1, 1]] :reconcile plain_r {k => v}")
        .expect_err("plain relation");
    assert!(
        format!("{err:?}").contains("requires a TxTime relation"),
        "{err:?}"
    );
    db.run_default(":create rc {k: Int, tt: TxTime => v: Int}")
        .unwrap();
    // conflicting duplicate keys in one output
    let err = db
        .run_default("?[k, v] <- [[1, 1], [1, 2]] :reconcile rc {k => v}")
        .expect_err("conflicting rows");
    assert!(format!("{err:?}").contains("conflicting rows"), "{err:?}");
    // an earlier pending write in the same transaction
    let err = db
        .run_default(
            "{?[k, v] <- [[9, 9]] :put rc {k => v}} {?[k, v] <- [[1, 1]] :reconcile rc {k => v}}",
        )
        .expect_err("pending write");
    assert!(format!("{err:?}").contains("only"), "{err:?}");
    // bitemporal rows must declare beliefs (assert flag)
    db.run_default(":create rcb {k: Int, vld: Validity, tt: TxTime => v: Int}")
        .unwrap();
    let err = db
        .run_default("?[k, vld, v] <- [[1, [100, false], 1]] :reconcile rcb {k, vld => v}")
        .expect_err("retract-flag row");
    assert!(format!("{err:?}").contains("declare beliefs"), "{err:?}");
    // empty output retracts every current belief
    db.run_default("?[k, v] <- [[1, 1], [2, 2]] :reconcile rc {k => v}")
        .unwrap();
    db.run_default("?[k, v] <- [] :reconcile rc {k => v}")
        .unwrap();
    let res = db
        .run_default("?[k, v] := *rc[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"].as_array().unwrap().len(), 0, "{res:?}");

    // the declaration is complete: NO other write to the relation in the
    // same transaction, before or after — including writes an IDEMPOTENT
    // reconcile leaves no pending trace of (review must-fix)
    db.run_default("?[k, v] <- [[1, 10]] :reconcile rc {k => v}")
        .unwrap();
    for later in [
        "{?[k, v] <- [[1, 10]] :reconcile rc {k => v}} {?[k, v] <- [[3, 30]] :put rc {k => v}}",
        "{?[k, v] <- [[1, 10]] :reconcile rc {k => v}} {?[k] <- [[1]] :rm rc {k}}",
        // idempotent first reconcile buffers nothing; the second must still bail
        "{?[k, v] <- [[1, 10]] :reconcile rc {k => v}} {?[k, v] <- [[2, 20]] :reconcile rc {k => v}}",
    ] {
        let err = db.run_default(later).expect_err("write after reconcile");
        assert!(format!("{err:?}").contains("reconcile"), "{later}: {err:?}");
    }
    // §5: the revision is invisible to later reads in the same script
    let res = db
        .run_default("{?[k, v] <- [[1, 99]] :reconcile rc {k => v}} {?[k, v] := *rc[k, t, v]}")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 10]]), "{res:?}");
    let res = db
        .run_default("?[k, v] := *rc[k, t, v]")
        .unwrap()
        .into_json();
    assert_eq!(res["rows"], serde_json::json!([[1, 99]]), "{res:?}");
}

fn catalog_canonicalization_fixture() -> DbInstance {
    let db = DbInstance::new("mem", "", "").unwrap();
    populate_catalog_canonicalization_fixture(&db);
    db
}

#[cfg(feature = "storage-sqlite")]
fn sqlite_catalog_canonicalization_fixture() -> (tempfile::TempDir, DbInstance) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog-canonicalization.db");
    let db = DbInstance::new("sqlite", path.to_str().unwrap(), "").unwrap();
    populate_catalog_canonicalization_fixture(&db);
    (directory, db)
}

fn populate_catalog_canonicalization_fixture(db: &DbInstance) {
    db.run_default(
        r#"
        :create canonical_base {
            id: Int =>
            text: String,
            embedding: <F32; 2>,
        }
        "#,
    )
    .unwrap();
    db.run_default("::index create canonical_base:by_text {text}")
        .unwrap();
    db.run_default(
        r#"
        ::hnsw create canonical_base:semantic {
            dim: 2,
            fields: embedding,
            distance: Cosine,
            m: 4,
            ef: 16,
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::fts create canonical_base:search {
            extractor: text,
            tokenizer: Simple,
            filters: [Lowercase],
        }
        "#,
    )
    .unwrap();
    db.run_default(
        r#"
        ::lsh create canonical_base:similar {
            extractor: text,
            tokenizer: NGram,
            n_gram: 3,
            n_perm: 32,
            target_threshold: 0.3,
        }
        "#,
    )
    .unwrap();
    db.run_default(":create canonical_rollback_probe {id: Int}")
        .unwrap();
}

fn raw_id_zero_catalog(db: &DbInstance) -> BTreeMap<Vec<u8>, Vec<u8>> {
    use crate::runtime::relation::RelationId;

    #[allow(unreachable_patterns)]
    match db {
        DbInstance::Mem(inner) => {
            let tx = inner.transact().unwrap();
            tx.store_tx
                .range_scan(
                    &RelationId::SYSTEM.raw_encode(),
                    &RelationId::new(1).raw_encode(),
                )
                .map(|row| row.unwrap())
                .collect()
        }
        #[cfg(feature = "storage-sqlite")]
        DbInstance::Sqlite(inner) => {
            let tx = inner.transact().unwrap();
            tx.store_tx
                .range_scan(
                    &RelationId::SYSTEM.raw_encode(),
                    &RelationId::new(1).raw_encode(),
                )
                .map(|row| row.unwrap())
                .collect()
        }
        _ => panic!("unsupported catalog canonicalization test storage"),
    }
}

fn raw_relation_catalog(db: &DbInstance) -> BTreeMap<String, Vec<u8>> {
    use crate::data::tuple::try_decode_tuple_from_key;

    raw_id_zero_catalog(db)
        .into_iter()
        .filter_map(|(key, value)| {
            let tuple = try_decode_tuple_from_key(&key, 2).unwrap();
            match tuple.as_slice() {
                [DataValue::Str(name)] => Some((name.to_string(), value)),
                _ => None,
            }
        })
        .collect()
}

fn rewrite_relation_catalog_codecs(
    db: &DbInstance,
    mut codec_at: impl FnMut(usize) -> crate::runtime::catalog_codec::CatalogCodec,
) {
    use crate::data::tuple::try_decode_tuple_from_key;
    use crate::runtime::catalog_codec::encode_test_catalog_record;
    use crate::runtime::relation::{RelationHandle, RelationId};

    macro_rules! rewrite {
        ($inner:expr) => {{
            let mut tx = $inner.transact_write().unwrap();
            let rows = tx
                .store_tx
                .range_scan(
                    &RelationId::SYSTEM.raw_encode(),
                    &RelationId::new(1).raw_encode(),
                )
                .map(|row| row.unwrap())
                .filter(|(key, _)| {
                    matches!(
                        try_decode_tuple_from_key(key, 2).unwrap().as_slice(),
                        [DataValue::Str(_)]
                    )
                })
                .collect_vec();
            for (position, (key, value)) in rows.into_iter().enumerate() {
                let handle = RelationHandle::decode(&value).unwrap();
                let encoded = encode_test_catalog_record(&handle, codec_at(position));
                tx.store_tx.put(&key, &encoded).unwrap();
            }
            tx.commit_tx().unwrap();
        }};
    }
    #[allow(unreachable_patterns)]
    match db {
        DbInstance::Mem(inner) => rewrite!(inner),
        #[cfg(feature = "storage-sqlite")]
        DbInstance::Sqlite(inner) => rewrite!(inner),
        _ => panic!("unsupported catalog canonicalization test storage"),
    }
}

fn canonicalize_relation_catalog(db: &DbInstance, names: Vec<String>) {
    let cap = names.len();
    let tx = db.multi_transaction(true);
    tx.canonicalize_relation_catalog_v1(names, cap).unwrap();
    tx.commit().unwrap();
}

#[test]
fn exact_name_catalog_canonicalizer_normalizes_all_codecs_with_native_parity() {
    use crate::runtime::catalog_codec::{decode_unattested_catalog, CatalogCodec};
    use crate::runtime::relation::RelationHandle;

    for generation in [
        CatalogCodec::PositionalV0,
        CatalogCodec::PositionalV1,
        CatalogCodec::StructMapV1,
    ] {
        let db = catalog_canonicalization_fixture();
        let native = raw_relation_catalog(&db);
        let semantic_preimage = native
            .iter()
            .map(|(name, value)| (name.clone(), RelationHandle::decode(value).unwrap()))
            .collect::<BTreeMap<_, _>>();
        rewrite_relation_catalog_codecs(&db, |position| {
            if generation == CatalogCodec::StructMapV1 {
                match position % 3 {
                    0 => CatalogCodec::PositionalV0,
                    1 => CatalogCodec::PositionalV1,
                    _ => CatalogCodec::StructMapV1,
                }
            } else {
                generation
            }
        });

        let names = native.keys().cloned().collect_vec();
        canonicalize_relation_catalog(&db, names.clone());
        let canonical = raw_relation_catalog(&db);
        assert_eq!(
            canonical, native,
            "canonical bytes diverged from native maps"
        );
        assert!(canonical.values().all(|value| {
            decode_unattested_catalog(value).unwrap().codec() == CatalogCodec::StructMapV1
        }));
        let semantic_postimage = canonical
            .iter()
            .map(|(name, value)| (name.clone(), RelationHandle::decode(value).unwrap()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            semantic_postimage, semantic_preimage,
            "nested catalog descriptors changed semantics"
        );

        canonicalize_relation_catalog(&db, names);
        assert_eq!(
            raw_relation_catalog(&db),
            canonical,
            "map-to-map canonicalization was not idempotent"
        );
    }
}

#[test]
fn exact_name_catalog_canonicalizer_refuses_name_and_cap_mismatches_before_writes() {
    let db = catalog_canonicalization_fixture();
    rewrite_relation_catalog_codecs(&db, |_| {
        crate::runtime::catalog_codec::CatalogCodec::PositionalV1
    });
    let names = raw_relation_catalog(&db).into_keys().collect_vec();
    let before = raw_id_zero_catalog(&db);

    let mut missing = names.clone();
    missing.push("not_in_the_catalog".to_owned());
    let mut extra = names.clone();
    extra.pop().unwrap();
    let mut duplicate = names.clone();
    duplicate.push(names[0].clone());
    let cases = [
        (missing.clone(), missing.len()),
        (extra.clone(), names.len()),
        (duplicate.clone(), duplicate.len()),
        (names.clone(), names.len() - 1),
        (names.clone(), 257),
    ];

    for (requested, cap) in cases {
        let tx = db.multi_transaction(true);
        tx.canonicalize_relation_catalog_v1(requested, cap)
            .expect_err("invalid exact-name request must refuse");
        assert_eq!(
            raw_id_zero_catalog(&db),
            before,
            "a preflight refusal changed the catalog"
        );
    }
}

#[test]
fn exact_name_catalog_canonicalizer_rejects_malformed_values_and_foreign_system_keys() {
    use crate::data::tuple::TupleT;
    use crate::runtime::relation::RelationId;

    let malformed = catalog_canonicalization_fixture();
    let names = raw_relation_catalog(&malformed).into_keys().collect_vec();
    let damaged_name = names.last().unwrap();
    let damaged_key =
        vec![DataValue::from(damaged_name.as_str())].encode_as_key(RelationId::SYSTEM);
    let DbInstance::Mem(inner) = &malformed else {
        panic!()
    };
    let mut tx = inner.transact_write().unwrap();
    tx.store_tx.put(&damaged_key, &[0x90]).unwrap();
    tx.commit_tx().unwrap();
    drop(tx);
    let before = raw_id_zero_catalog(&malformed);
    let tx = malformed.multi_transaction(true);
    tx.canonicalize_relation_catalog_v1(names.clone(), names.len())
        .expect_err("malformed exact codec must refuse");
    assert_eq!(raw_id_zero_catalog(&malformed), before);

    let foreign = catalog_canonicalization_fixture();
    let names = raw_relation_catalog(&foreign).into_keys().collect_vec();
    let foreign_key = vec![DataValue::Null, DataValue::from("FOREIGN_SYSTEM_KEY")]
        .encode_as_key(RelationId::SYSTEM);
    let DbInstance::Mem(inner) = &foreign else {
        panic!()
    };
    let mut tx = inner.transact_write().unwrap();
    tx.store_tx.put(&foreign_key, &[0]).unwrap();
    tx.commit_tx().unwrap();
    drop(tx);
    let before = raw_id_zero_catalog(&foreign);
    let tx = foreign.multi_transaction(true);
    tx.canonicalize_relation_catalog_v1(names.clone(), names.len())
        .expect_err("foreign id-zero key shape must refuse");
    assert_eq!(raw_id_zero_catalog(&foreign), before);
}

#[test]
fn exact_name_catalog_canonicalizer_rolls_back_after_every_put_failure() {
    let db = catalog_canonicalization_fixture();
    assert_catalog_canonicalizer_rolls_back_after_every_put(&db);
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn exact_name_catalog_canonicalizer_rolls_back_every_sqlite_put_failure() {
    let (_directory, db) = sqlite_catalog_canonicalization_fixture();
    assert_catalog_canonicalizer_rolls_back_after_every_put(&db);
}

fn assert_catalog_canonicalizer_rolls_back_after_every_put(db: &DbInstance) {
    rewrite_relation_catalog_codecs(db, |_| {
        crate::runtime::catalog_codec::CatalogCodec::PositionalV0
    });
    let names = raw_relation_catalog(db).into_keys().collect_vec();
    let before = raw_id_zero_catalog(db);

    for failed_put in 1..=names.len() {
        let tx = db.multi_transaction(true);
        tx.run_script(
            "?[id] <- [[1]] :put canonical_rollback_probe {id}",
            Default::default(),
        )
        .unwrap();
        tx.sender
            .send(
                TransactionPayload::CanonicalizeRelationCatalogV1FailAfterPut(
                    names.clone(),
                    names.len(),
                    failed_put,
                ),
            )
            .unwrap();
        let error = tx
            .receiver
            .recv()
            .unwrap()
            .expect_err("injected catalog put failure must abort");
        assert!(
            format!("{error:?}").contains("injected relation catalog canonicalization failure"),
            "{error:?}"
        );
        assert_eq!(
            raw_id_zero_catalog(db),
            before,
            "put {failed_put} escaped outer rollback"
        );
        assert!(
            db.run_default("?[id] := *canonical_rollback_probe{id}")
                .unwrap()
                .rows
                .is_empty(),
            "put {failed_put} committed work staged before canonicalization"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum HostileCatalogRelationCase {
    ZeroId,
    OutOfRangeId,
    DuplicateId,
    Temporary,
    MissingStandaloneChild,
    NestedStandaloneMismatch,
}

fn install_hostile_catalog_relation(db: &DbInstance, case: HostileCatalogRelationCase) {
    use crate::data::tuple::TupleT;
    use crate::runtime::catalog_codec::{encode_test_catalog_record, CatalogCodec};
    use crate::runtime::relation::{RelationHandle, RelationId};

    let catalog = raw_relation_catalog(db);
    let mut target = RelationHandle::decode(&catalog["canonical_base"]).unwrap();
    match case {
        HostileCatalogRelationCase::ZeroId => target.id = RelationId::SYSTEM,
        HostileCatalogRelationCase::OutOfRangeId => target.id = RelationId(1_u64 << 48),
        HostileCatalogRelationCase::DuplicateId => {
            target.id = RelationHandle::decode(&catalog["canonical_rollback_probe"])
                .unwrap()
                .id;
        }
        HostileCatalogRelationCase::Temporary => target.is_temp = true,
        HostileCatalogRelationCase::MissingStandaloneChild => {
            target
                .indices
                .values_mut()
                .next()
                .expect("canonical fixture must have a normal-index child")
                .0
                .name = "canonical_missing_standalone_child".into();
        }
        HostileCatalogRelationCase::NestedStandaloneMismatch => {
            target
                .indices
                .values_mut()
                .next()
                .expect("canonical fixture must have a normal-index child")
                .0
                .description = "hostile nested descriptor mismatch".into();
        }
    }

    let key = vec![DataValue::from("canonical_base")].encode_as_key(RelationId::SYSTEM);
    let encoded = encode_test_catalog_record(&target, CatalogCodec::StructMapV1);
    write_raw_id_zero_value(db, &key, Some(&encoded));
}

#[derive(Clone, Copy, Debug)]
enum HostileCatalogMetadataCase {
    CounterBelowLiveId,
    MalformedCounter,
    OutOfRangeCounter,
    MissingCounter,
    MissingStorageVersion,
    WrongStorageVersion,
    KeyNameMismatch,
}

impl HostileCatalogMetadataCase {
    const fn expected_error(self) -> &'static str {
        match self {
            Self::CounterBelowLiveId => "relation counter was below a live id",
            Self::MalformedCounter | Self::OutOfRangeCounter => "rejected the relation counter",
            Self::MissingCounter | Self::MissingStorageVersion => {
                "did not find the exact catalog domain"
            }
            Self::WrongStorageVersion => "rejected the storage version",
            Self::KeyNameMismatch => "found a key/name mismatch",
        }
    }
}

fn install_hostile_catalog_metadata(db: &DbInstance, case: HostileCatalogMetadataCase) {
    use crate::data::tuple::TupleT;
    use crate::runtime::catalog_codec::{encode_test_catalog_record, CatalogCodec};
    use crate::runtime::relation::{RelationHandle, RelationId};

    let counter_key = vec![DataValue::Null].encode_as_key(RelationId::SYSTEM);
    let storage_version_key =
        vec![DataValue::Null, DataValue::from("STORAGE_VERSION")].encode_as_key(RelationId::SYSTEM);
    match case {
        HostileCatalogMetadataCase::CounterBelowLiveId => {
            let maximum = raw_relation_catalog(db)
                .values()
                .map(|value| RelationHandle::decode(value).unwrap().id.0)
                .max()
                .filter(|maximum| *maximum > 0)
                .expect("fixture must contain a positive relation id");
            write_raw_id_zero_value(db, &counter_key, Some(&(maximum - 1).to_be_bytes()));
        }
        HostileCatalogMetadataCase::MalformedCounter => {
            write_raw_id_zero_value(db, &counter_key, Some(&[0; 7]));
        }
        HostileCatalogMetadataCase::OutOfRangeCounter => {
            write_raw_id_zero_value(db, &counter_key, Some(&(1_u64 << 48).to_be_bytes()));
        }
        HostileCatalogMetadataCase::MissingCounter => {
            write_raw_id_zero_value(db, &counter_key, None);
        }
        HostileCatalogMetadataCase::MissingStorageVersion => {
            write_raw_id_zero_value(db, &storage_version_key, None);
        }
        HostileCatalogMetadataCase::WrongStorageVersion => {
            write_raw_id_zero_value(db, &storage_version_key, Some(&[1]));
        }
        HostileCatalogMetadataCase::KeyNameMismatch => {
            let catalog = raw_relation_catalog(db);
            let mut target = RelationHandle::decode(&catalog["canonical_base"]).unwrap();
            target.name = "canonical_value_name_mismatch".into();
            let encoded = encode_test_catalog_record(&target, CatalogCodec::StructMapV1);
            let key = vec![DataValue::from("canonical_base")].encode_as_key(RelationId::SYSTEM);
            write_raw_id_zero_value(db, &key, Some(&encoded));
        }
    }
}

fn write_raw_id_zero_value(db: &DbInstance, key: &[u8], value: Option<&[u8]>) {
    macro_rules! write {
        ($inner:expr) => {{
            let mut tx = $inner.transact_write().unwrap();
            match value {
                Some(value) => tx.store_tx.put(key, value).unwrap(),
                None => tx.store_tx.del(key).unwrap(),
            }
            tx.commit_tx().unwrap();
        }};
    }
    #[allow(unreachable_patterns)]
    match db {
        DbInstance::Mem(inner) => write!(inner),
        #[cfg(feature = "storage-sqlite")]
        DbInstance::Sqlite(inner) => write!(inner),
        _ => panic!("unsupported catalog canonicalization test storage"),
    }
}

fn assert_catalog_canonicalization_refused_without_change(db: &DbInstance, expected_error: &str) {
    let names = raw_relation_catalog(db).into_keys().collect_vec();
    let before = raw_id_zero_catalog(db);
    let tx = db.multi_transaction(true);
    let error = tx
        .canonicalize_relation_catalog_v1(names.clone(), names.len())
        .expect_err("hostile catalog must be refused");
    assert!(
        format!("{error:?}").contains(expected_error),
        "unexpected hostile-catalog error: {error:?}"
    );
    assert_eq!(
        raw_id_zero_catalog(db),
        before,
        "hostile catalog refusal changed durable id-zero bytes"
    );
}

#[test]
fn exact_name_catalog_canonicalizer_refuses_every_hostile_identity_and_linkage_case() {
    for case in [
        HostileCatalogRelationCase::ZeroId,
        HostileCatalogRelationCase::OutOfRangeId,
        HostileCatalogRelationCase::DuplicateId,
        HostileCatalogRelationCase::Temporary,
        HostileCatalogRelationCase::MissingStandaloneChild,
        HostileCatalogRelationCase::NestedStandaloneMismatch,
    ] {
        let db = catalog_canonicalization_fixture();
        install_hostile_catalog_relation(&db, case);
        assert_catalog_canonicalization_refused_without_change(
            &db,
            "rejected catalog identity or linkage",
        );
    }
}

#[test]
fn exact_name_catalog_canonicalizer_refuses_every_hostile_metadata_case() {
    for case in [
        HostileCatalogMetadataCase::CounterBelowLiveId,
        HostileCatalogMetadataCase::MalformedCounter,
        HostileCatalogMetadataCase::OutOfRangeCounter,
        HostileCatalogMetadataCase::MissingCounter,
        HostileCatalogMetadataCase::MissingStorageVersion,
        HostileCatalogMetadataCase::WrongStorageVersion,
        HostileCatalogMetadataCase::KeyNameMismatch,
    ] {
        let db = catalog_canonicalization_fixture();
        install_hostile_catalog_metadata(&db, case);
        assert_catalog_canonicalization_refused_without_change(&db, case.expected_error());
    }
}

fn assert_catalog_preflight_refusal_is_terminal(db: &DbInstance) {
    install_hostile_catalog_relation(db, HostileCatalogRelationCase::Temporary);
    let names = raw_relation_catalog(db).into_keys().collect_vec();
    let before = raw_id_zero_catalog(db);
    let tx = db.multi_transaction(true);
    tx.run_script(
        "?[id] <- [[91]] :put canonical_rollback_probe {id}",
        Default::default(),
    )
    .unwrap();

    tx.canonicalize_relation_catalog_v1(names.clone(), names.len())
        .expect_err("preflight refusal must fail the operation");
    tx.commit()
        .expect_err("a refused canonicalizer transaction must be terminal");

    assert_eq!(
        raw_id_zero_catalog(db),
        before,
        "terminal refusal changed the hostile catalog"
    );
    assert!(
        db.run_default("?[id] := *canonical_rollback_probe{id}")
            .unwrap()
            .rows
            .is_empty(),
        "terminal refusal committed work staged before preflight"
    );
}

#[test]
fn exact_name_catalog_canonicalizer_preflight_refusal_is_terminal_in_memory() {
    let db = catalog_canonicalization_fixture();
    assert_catalog_preflight_refusal_is_terminal(&db);
}

#[cfg(feature = "storage-sqlite")]
#[test]
fn exact_name_catalog_canonicalizer_preflight_refusal_is_terminal_in_sqlite() {
    let (_directory, db) = sqlite_catalog_canonicalization_fixture();
    assert_catalog_preflight_refusal_is_terminal(&db);
}
