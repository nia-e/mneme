/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
*/

//! This crate provides the core functionalities of [CozoDB](https://cozodb.org).
//! It may be used to embed CozoDB in your application.
//!
//! This doc describes the Rust API. To learn how to use CozoDB to query (CozoScript), see:
//!
//! * [The CozoDB documentation](https://docs.cozodb.org)
//!
//! Rust API usage:
//! ```
//! use cozo::*;
//!
//! let db = DbInstance::new("mem", "", Default::default()).unwrap();
//! let script = "?[a] := a in [1, 2, 3]";
//! let result = db.run_script(script, Default::default(), ScriptMutability::Immutable).unwrap();
//! println!("{:?}", result);
//! ```
//! We created an in-memory database above. There are other persistent options:
//! see [DbInstance::new]. It is perfectly fine to run multiple storage engines in the same process.
//!
#![doc = document_features::document_features!()]
#![warn(rust_2018_idioms, future_incompatible)]
// mnestic fork: the CI clippy gate runs `-D warnings`. The allows below are
// inherited from upstream CozoDB across the whole tree (or intrinsic to its
// design) and are tracked as a cleanup backlog rather than gated on, so the gate
// stays meaningful for *new* issues. Revisit behind a dedicated docs/cleanup pass.
#![allow(missing_docs)] // upstream public items are largely undocumented
#![allow(dead_code)] // upstream scaffolding kept for parity
#![allow(unused_assignments)] // inherited
#![allow(private_interfaces)] // internal IR structs are `pub` but expose pub(crate) types
#![allow(clippy::type_complexity)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::mutable_key_type)] // DataValue (interior-mutable) used as map keys throughout
#![allow(clippy::unnecessary_unwrap)] // inherited query-compile patterns
#![allow(clippy::missing_transmute_annotations)] // documented intentional lifetime casts (e.g. sqlite stmt)
#![allow(clippy::manual_is_multiple_of)] // `is_multiple_of` needs Rust 1.87; our MSRV is 1.85

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
#[allow(unused_imports)]
use std::time::{Duration, Instant};

use crossbeam::channel::{bounded, Receiver, RecvTimeoutError, Sender};
use data::functions::current_validity;
use lazy_static::lazy_static;
pub use miette::Error;
use miette::Report;
#[allow(unused_imports)]
use miette::{
    bail, ensure, miette, GraphicalReportHandler, GraphicalTheme, IntoDiagnostic, JSONReportHandler,
    Result, ThemeCharacters, ThemeStyles,
};
use parse::parse_script;
use parse::CozoScript;
use serde_json::json;

#[cfg(feature = "cypher")]
pub use cypher::{CypherGraphSchema, EdgeMap, NodeMap};
pub use data::aggr::{
    CustomAggrRegistries, MeetAggrObj, NormalAggrObj, RegisteredAggr, RegisteredBoundedMeet,
};
pub use data::tuple::try_decode_tuple_from_key;
pub use data::value::{DataValue, Num, RegexWrapper, UuidWrapper, Validity, ValidityTs};
pub use fixed_rule::{FixedRule, FixedRuleInputRelation, FixedRulePayload};
/// The adjacency types a graph projection hands out (mnestic fork).
///
/// These come from the [`graph`](https://docs.rs/graph) crate, so it is a
/// **public dependency** of mnestic: these are its types, not ours, and a
/// consumer that also depends on `graph` directly must resolve to the same
/// semver-compatible version or the types will not unify.
///
/// The `graph = "0.3"` requirement therefore constrains mnestic's own API.
/// Moving it to `0.4` is a **semver-major event for mnestic** and must be
/// released as one; within `0.3.x`, cargo may pick a newer patch, which semver
/// obliges to stay API-compatible (verified: the `0.3.1` and `0.3.2` preludes
/// are identical). Pinning `=0.3.1` is deliberately *not* done — it would make
/// mnestic unresolvable alongside any crate wanting a later `0.3.x`.
#[cfg(feature = "graph-algo")]
pub use graph::prelude::{DirectedCsrGraph, DirectedNeighbors, DirectedNeighborsWithValues, Graph};
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub use runtime::catalog_codec::ManagedCatalogFixtureV1;
pub use runtime::catalog_codec::{
    ManagedAccessLevelV1, ManagedCatalogCensusV1, ManagedCatalogEncodingV1, ManagedCatalogEntryV1,
    ManagedColumnTypeKindV1, ManagedColumnTypeV1, ManagedColumnV1, ManagedFtsIndexV1,
    ManagedFtsManifestV1, ManagedHnswDistanceV1, ManagedHnswIndexV1, ManagedHnswManifestV1,
    ManagedNormalIndexV1, ManagedRelationV1, ManagedTokenizerV1, ManagedVectorElementTypeV1,
};
pub use runtime::db::Db;
pub use runtime::db::NamedRows;
#[cfg(feature = "graph-algo")]
pub use runtime::graph_projection::{
    GraphSource, GraphVariant, ProjectionVariant, VariantKey, VariantSpec,
};
pub use runtime::hybrid::{build_hybrid_query, GraphLeg, HybridList, HybridSearch, MmrParams};
pub use runtime::relation::try_decode_tuple_from_kv;
pub use runtime::temp_store::RegularTempStore;
pub use storage::mem::{new_cozo_mem, MemStorage};
#[cfg(feature = "storage-new-rocksdb")]
pub use storage::newrocks::{new_cozo_newrocksdb, NewRocksDbStorage};
#[cfg(feature = "storage-rocksdb")]
pub use storage::rocks::{new_cozo_rocksdb, RocksDbStorage};
#[cfg(feature = "storage-sled")]
pub use storage::sled::{new_cozo_sled, SledStorage};
#[cfg(feature = "storage-sqlite")]
pub use storage::sqlite::{
    new_cozo_sqlite, new_cozo_sqlite_existing, ExistingSqliteSnapshotSource,
    ManagedCatalogFencePolicy, ManagedExistingSqliteOpenPermitV1, ManagedSnapshotPolicy,
    ManagedSqliteAssertionTranscriptV1, ManagedSqliteCatalogAssertionPlannerV1,
    ManagedSqliteCatalogFenceAssertionEvidenceV1, ManagedSqliteCatalogFenceAssertionOutcomeV1,
    ManagedSqliteCatalogFenceAssertionTranscriptV1, ManagedSqliteClosedAuditV1,
    ManagedSqliteClosedCatalogFenceV1, ManagedSqliteClosedMainFileIdentityV1,
    ManagedSqliteClosedRecordVisitV1, ManagedSqliteClosedResidualMetadataV1,
    ManagedSqliteClosedResidualV1, ManagedSqliteClosedSourceEvidenceV1,
    ManagedSqlitePhysicalCensusV1, ManagedSqlitePrimaryIndexCatalogV1,
    ManagedSqliteRecordVisitEventV1, ManagedSqliteRecordVisitEvidenceV1,
    ManagedSqliteRelationAssertionEvidenceV1, ManagedSqliteRelationAssertionOutcomeV1,
    ManagedSqliteRelationAssertionPlannerV1, ManagedSqliteRelationRowCountV1,
    ManagedSqliteSnapshotCensusV1, ManagedSqliteSnapshotReader, SqliteStorage,
    SQLITE_BUSY_TIMEOUT_MS,
};
#[cfg(feature = "storage-tikv")]
pub use storage::tikv::{new_cozo_tikv, TiKvStorage};
pub use storage::{Storage, StoreTx};

pub use crate::data::expr::Expr;
use crate::data::json::JsonValue;
pub use crate::data::symb::Symbol;
pub use crate::data::value::{JsonData, Vector};
pub use crate::fixed_rule::SimpleFixedRule;
pub use crate::parse::SourceSpan;
pub use crate::runtime::callback::CallbackOp;
pub use crate::runtime::db::evaluate_expressions;
pub use crate::runtime::db::get_variables;
pub use crate::runtime::db::Payload;
pub use crate::runtime::db::Poison;
pub use crate::runtime::db::ScriptMutability;
pub use crate::runtime::db::ScriptRunOptions;
pub use crate::runtime::db::TransactionPayload;

#[cfg(feature = "cypher")]
pub mod cypher;
pub mod data;
pub(crate) mod fixed_rule;
pub(crate) mod fts;
pub mod parse;
pub(crate) mod query;
pub(crate) mod runtime;
pub(crate) mod storage;
pub(crate) mod utils;

/// Hard ceiling for one direct primary-key range page. This API exists for
/// bounded systems work that must not route through CozoScript's global sorter.
pub const MAX_PRIMARY_KEY_SCAN_ROWS: usize = 4_096;

/// One complete suffix bound for a [`PrimaryKeyScan`]. Bounds never expose the
/// storage encoding's private sentinel values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrimaryKeyScanBound {
    Unbounded,
    Included(Vec<DataValue>),
    Excluded(Vec<DataValue>),
}

/// Physical iteration direction for a [`PrimaryKeyScan`]. Descending scans are
/// supported only when the selected storage engine has a native lazy reverse
/// range iterator; the API never collects and reverses an unbounded range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryKeyScanDirection {
    Ascending,
    Descending,
}

/// A bounded direct scan over one stored relation's physical primary key.
///
/// `prefix` fixes the leading key columns. Each bounded suffix must supply all
/// remaining key columns, which makes cursor ordering unambiguous. Temporal
/// relations are rejected: this primitive deliberately has no as-of semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrimaryKeyScan {
    pub prefix: Vec<DataValue>,
    pub lower: PrimaryKeyScanBound,
    pub upper: PrimaryKeyScanBound,
    pub direction: PrimaryKeyScanDirection,
    pub limit: usize,
}

/// Result of one [`PrimaryKeyScan`]. `scanned` counts decoded storage rows
/// yielded by the bounded range operation; it is not an estimate of internal
/// storage-page visits and is always at most the hard limit.
#[derive(Debug)]
pub struct PrimaryKeyScanPage {
    pub rows: NamedRows,
    pub scanned: usize,
}

/// A dispatcher for concrete storage implementations, wrapping [Db]. This is done so that
/// client code does not have to deal with generic code constantly. You may prefer to use
/// [Db] directly, especially if you provide a custom storage engine.
///
/// Many methods are dispatching methods for the corresponding methods on [Db].
///
/// Other methods are wrappers simplifying signatures to deal with only strings.
/// These methods made code for interop with other languages much easier,
/// but are not desirable if you are using Rust.
#[derive(Clone)]
pub enum DbInstance {
    /// In memory storage (not persistent)
    Mem(Db<MemStorage>),
    #[cfg(feature = "storage-sqlite")]
    /// Sqlite storage
    Sqlite(Db<SqliteStorage>),
    #[cfg(feature = "storage-rocksdb")]
    /// RocksDB storage
    RocksDb(Db<RocksDbStorage>),
    #[cfg(feature = "storage-new-rocksdb")]
    /// New RocksDB storage
    NewRocksDb(Db<NewRocksDbStorage>),
    #[cfg(feature = "storage-sled")]
    /// Sled storage (experimental)
    Sled(Db<SledStorage>),
    #[cfg(feature = "storage-tikv")]
    /// TiKV storage (experimental)
    TiKv(Db<TiKvStorage>),
}

impl Default for DbInstance {
    fn default() -> Self {
        Self::new("mem", "", Default::default()).unwrap()
    }
}

impl DbInstance {
    /// Open an initialized SQLite database without creating a missing file or
    /// initializing an empty one.
    #[cfg(feature = "storage-sqlite")]
    pub fn open_existing_sqlite(permit: ManagedExistingSqliteOpenPermitV1) -> Result<Self> {
        Ok(Self::Sqlite(new_cozo_sqlite_existing(permit)?))
    }

    /// Create a DbInstance, which is a dispatcher for various concrete implementations.
    /// The valid engines are:
    ///
    /// * `mem`
    /// * `sqlite`
    /// * `rocksdb`
    /// * `newrocksdb`
    /// * `sled`
    /// * `tikv`
    ///
    /// assuming all features are enabled during compilation. Otherwise only
    /// some of the engines are available. The `mem` engine is always available.
    ///
    /// `path` is ignored for `mem` and `tikv` engines.
    /// `options` is ignored for every engine except `tikv`.
    #[allow(unused_variables)]
    pub fn new(engine: &str, path: impl AsRef<Path>, options: &str) -> Result<Self> {
        let options = if options.is_empty() { "{}" } else { options };
        Ok(match engine {
            "mem" => Self::Mem(new_cozo_mem()?),
            #[cfg(feature = "storage-sqlite")]
            "sqlite" => Self::Sqlite(new_cozo_sqlite(path)?),
            #[cfg(feature = "storage-rocksdb")]
            "rocksdb" => Self::RocksDb(new_cozo_rocksdb(path)?),
            #[cfg(feature = "storage-new-rocksdb")]
            "newrocksdb" => Self::NewRocksDb(new_cozo_newrocksdb(path)?),
            #[cfg(feature = "storage-sled")]
            "sled" => Self::Sled(new_cozo_sled(path)?),
            #[cfg(feature = "storage-tikv")]
            "tikv" => {
                #[derive(serde_derive::Deserialize)]
                struct TiKvOpts {
                    end_points: Vec<String>,
                    optimistic: bool,
                }
                let opts: TiKvOpts = serde_json::from_str(options).into_diagnostic()?;
                Self::TiKv(new_cozo_tikv(opts.end_points.clone(), opts.optimistic)?)
            }
            k => bail!(
                "database engine '{}' not supported (maybe not compiled in)",
                k
            ),
        })
    }
    /// Same as [Self::new], but inputs and error messages are all in strings
    pub fn new_with_str(
        engine: &str,
        path: &str,
        options: &str,
    ) -> std::result::Result<Self, String> {
        Self::new(engine, path, options).map_err(|err| err.to_string())
    }

    /// Dispatcher method.  See [crate::Db::get_fixed_rules].
    pub fn get_fixed_rules(&self) -> BTreeMap<String, Arc<Box<dyn FixedRule>>> {
        match self {
            DbInstance::Mem(db) => db.get_fixed_rules(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.get_fixed_rules(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.get_fixed_rules(),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.get_fixed_rules(),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.get_fixed_rules(),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.get_fixed_rules(),
        }
    }
    /// Snapshot of the registered custom aggregates (mnestic fork, R0b).
    pub fn get_custom_aggrs(&self) -> BTreeMap<String, RegisteredAggr> {
        match self {
            DbInstance::Mem(db) => db.get_custom_aggrs(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.get_custom_aggrs(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.get_custom_aggrs(),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.get_custom_aggrs(),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.get_custom_aggrs(),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.get_custom_aggrs(),
        }
    }

    /// Dispatcher method. See [crate::Db::get_custom_bounded_meets].
    pub fn get_custom_bounded_meets(&self) -> BTreeMap<String, RegisteredBoundedMeet> {
        match self {
            DbInstance::Mem(db) => db.get_custom_bounded_meets(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.get_custom_bounded_meets(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.get_custom_bounded_meets(),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.get_custom_bounded_meets(),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.get_custom_bounded_meets(),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.get_custom_bounded_meets(),
        }
    }
    /// Dispatcher method. See [crate::Db::run_script].
    pub fn run_script(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        mutability: ScriptMutability,
    ) -> Result<NamedRows> {
        let cur_vld = current_validity();
        self.run_script_ast(
            parse_script(
                payload,
                &params,
                &self.get_fixed_rules(),
                CustomAggrRegistries {
                    meet: &self.get_custom_aggrs(),
                    bounded: &self.get_custom_bounded_meets(),
                },
                cur_vld,
            )?,
            cur_vld,
            mutability,
        )
    }
    /// `run_script` with mutable script and no parameters
    pub fn run_default(&self, payload: &str) -> Result<NamedRows> {
        self.run_script(payload, BTreeMap::new(), ScriptMutability::Mutable)
    }
    /// Run the CozoScript with per-call [`ScriptRunOptions`] (mnestic fork,
    /// query budget) — currently a per-call wall-clock `timeout` in seconds.
    /// See [`crate::Db::run_script_with_options`] for the budget precedence.
    pub fn run_script_with_options(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        mutability: ScriptMutability,
        options: ScriptRunOptions,
    ) -> Result<NamedRows> {
        match self {
            DbInstance::Mem(db) => db.run_script_with_options(payload, params, mutability, options),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => {
                db.run_script_with_options(payload, params, mutability, options)
            }
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => {
                db.run_script_with_options(payload, params, mutability, options)
            }
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => {
                db.run_script_with_options(payload, params, mutability, options)
            }
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => {
                db.run_script_with_options(payload, params, mutability, options)
            }
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => {
                db.run_script_with_options(payload, params, mutability, options)
            }
        }
    }
    /// Enable or disable the automatic factorized-count rewrite (mnestic fork,
    /// query factorization). Db-wide kill switch, default OFF. See
    /// [`crate::Db::set_query_factorization`].
    pub fn set_query_factorization(&self, enabled: bool) {
        match self {
            DbInstance::Mem(db) => db.set_query_factorization(enabled),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.set_query_factorization(enabled),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.set_query_factorization(enabled),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.set_query_factorization(enabled),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.set_query_factorization(enabled),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.set_query_factorization(enabled),
        }
    }

    /// Whether the automatic factorized-count rewrite is enabled (mnestic fork,
    /// query factorization). See [`crate::Db::query_factorization`].
    pub fn query_factorization(&self) -> bool {
        match self {
            DbInstance::Mem(db) => db.query_factorization(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.query_factorization(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.query_factorization(),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.query_factorization(),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.query_factorization(),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.query_factorization(),
        }
    }

    /// Set a Db-wide default per-query wall-clock budget in seconds (mnestic
    /// fork, query budget). `None` disables it. See
    /// [`crate::Db::set_default_query_timeout`].
    pub fn set_default_query_timeout(&self, secs: Option<f64>) {
        match self {
            DbInstance::Mem(db) => db.set_default_query_timeout(secs),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.set_default_query_timeout(secs),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.set_default_query_timeout(secs),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.set_default_query_timeout(secs),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.set_default_query_timeout(secs),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.set_default_query_timeout(secs),
        }
    }
    /// Test-only dispatcher. See [`crate::Db::fail_next_commit_for_tests`].
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn fail_next_commit_for_tests(&self) {
        match self {
            DbInstance::Mem(db) => db.fail_next_commit_for_tests(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.fail_next_commit_for_tests(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.fail_next_commit_for_tests(),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.fail_next_commit_for_tests(),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.fail_next_commit_for_tests(),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.fail_next_commit_for_tests(),
        }
    }

    /// Test-only dispatcher. See
    /// [`crate::Db::abort_after_imperative_statement_for_tests`].
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn abort_after_imperative_statement_for_tests(&self, statements: u32) {
        match self {
            DbInstance::Mem(db) => {
                db.abort_after_imperative_statement_for_tests(statements)
            }
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => {
                db.abort_after_imperative_statement_for_tests(statements)
            }
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => {
                db.abort_after_imperative_statement_for_tests(statements)
            }
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => {
                db.abort_after_imperative_statement_for_tests(statements)
            }
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => {
                db.abort_after_imperative_statement_for_tests(statements)
            }
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => {
                db.abort_after_imperative_statement_for_tests(statements)
            }
        }
    }

    /// The current Db-wide default per-query budget in seconds, or `None`
    /// (mnestic fork, query budget). See [`crate::Db::default_query_timeout`].
    pub fn default_query_timeout(&self) -> Option<f64> {
        match self {
            DbInstance::Mem(db) => db.default_query_timeout(),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.default_query_timeout(),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.default_query_timeout(),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.default_query_timeout(),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.default_query_timeout(),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.default_query_timeout(),
        }
    }
    /// One-call hybrid retrieval (mnestic fork addition): runs an HNSW + FTS
    /// (+ optional traversal) recall, fuses with Reciprocal Rank Fusion, and
    /// optionally diversifies with Maximal Marginal Relevance. Read-only. See
    /// [`HybridSearch`].
    pub fn hybrid_search(&self, q: &HybridSearch) -> Result<NamedRows> {
        let (script, params) = build_hybrid_query(q)?;
        self.run_script(&script, params, ScriptMutability::Immutable)
    }
    /// Build the CozoScript that [`DbInstance::hybrid_search`] would run, without
    /// executing it — for inspection or hand-tuning. See [`HybridSearch`].
    pub fn hybrid_search_script(&self, q: &HybridSearch) -> Result<String> {
        Ok(build_hybrid_query(q)?.0)
    }
    /// Run a read-only Cypher query (mnestic fork; feature `cypher`). Translates an
    /// openCypher subset to CozoScript against the property-graph `schema`, runs it
    /// read-only, and returns rows whose columns are the `RETURN` items. Cypher
    /// literals are passed as params; user `$name` params are supplied via `params`.
    /// See [`CypherGraphSchema`] and `docs/specs/cypher-read.md`.
    #[cfg(feature = "cypher")]
    pub fn run_cypher(
        &self,
        query: &str,
        schema: &CypherGraphSchema,
        mut params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        if params.keys().any(|k| k.starts_with("cphr_")) {
            return Err(miette::miette!(
                "parameter names starting with `cphr_` are reserved by the Cypher translator"
            ));
        }
        let cs = crate::cypher::build_cypher_script(query, schema)?;
        for (k, v) in cs.params {
            params.insert(k, v);
        }
        let mut out = self.run_script(&cs.script, params, ScriptMutability::Immutable)?;
        // Bag mode appends hidden binding-key columns; keep only the RETURN columns.
        let n = cs.out_columns.len();
        for row in &mut out.rows {
            row.truncate(n);
        }
        out.headers = cs.out_columns;
        Ok(out)
    }
    /// Build the CozoScript that [`DbInstance::run_cypher`] would run, without
    /// executing it — for inspection or hand-tuning. Returns `(script, literal params)`.
    #[cfg(feature = "cypher")]
    pub fn cypher_to_script(
        &self,
        query: &str,
        schema: &CypherGraphSchema,
    ) -> Result<(String, BTreeMap<String, DataValue>)> {
        let cs = crate::cypher::build_cypher_script(query, schema)?;
        Ok((cs.script, cs.params))
    }
    /// Run a parsed (AST) program. If you have a string script, use `run_script` or `run_default`.
    pub fn run_script_ast(
        &self,
        payload: CozoScript,
        cur_vld: ValidityTs,
        mutability: ScriptMutability,
    ) -> Result<NamedRows> {
        match self {
            DbInstance::Mem(db) => db.run_script_ast(payload, cur_vld, mutability),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.run_script_ast(payload, cur_vld, mutability),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.run_script_ast(payload, cur_vld, mutability),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.run_script_ast(payload, cur_vld, mutability),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.run_script_ast(payload, cur_vld, mutability),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.run_script_ast(payload, cur_vld, mutability),
        }
    }
    /// Run the CozoScript passed in. The `params` argument is a map of parameters.
    /// Fold any error into the return JSON itself.
    /// See [crate::Db::run_script].
    pub fn run_script_fold_err(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        mutability: ScriptMutability,
    ) -> JsonValue {
        #[cfg(not(target_arch = "wasm32"))]
        let start = Instant::now();

        match self.run_script(payload, params, mutability) {
            Ok(named_rows) => {
                let mut j_val = named_rows.into_json();
                #[cfg(not(target_arch = "wasm32"))]
                let took = start.elapsed().as_secs_f64();
                let map = j_val.as_object_mut().unwrap();
                map.insert("ok".to_string(), json!(true));
                #[cfg(not(target_arch = "wasm32"))]
                map.insert("took".to_string(), json!(took));

                j_val
            }
            Err(err) => format_error_as_json(err, Some(payload)),
        }
    }
    /// [`DbInstance::run_script_fold_err`] with per-call [`ScriptRunOptions`]
    /// (mnestic fork, query budget). A budget expiry folds into the JSON with
    /// `code == "eval::timeout"`, distinct from a `::kill`'s `eval::killed`.
    pub fn run_script_fold_err_with_options(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
        mutability: ScriptMutability,
        options: ScriptRunOptions,
    ) -> JsonValue {
        #[cfg(not(target_arch = "wasm32"))]
        let start = Instant::now();

        match self.run_script_with_options(payload, params, mutability, options) {
            Ok(named_rows) => {
                let mut j_val = named_rows.into_json();
                #[cfg(not(target_arch = "wasm32"))]
                let took = start.elapsed().as_secs_f64();
                let map = j_val.as_object_mut().unwrap();
                map.insert("ok".to_string(), json!(true));
                #[cfg(not(target_arch = "wasm32"))]
                map.insert("took".to_string(), json!(took));

                j_val
            }
            Err(err) => format_error_as_json(err, Some(payload)),
        }
    }
    /// Run the CozoScript passed in. The `params` argument is a map of parameters formatted as JSON.
    /// See [crate::Db::run_script].
    pub fn run_script_str(&self, payload: &str, params: &str, immutable: bool) -> String {
        let params_json = if params.is_empty() {
            BTreeMap::default()
        } else {
            match serde_json::from_str::<BTreeMap<String, JsonValue>>(params) {
                Ok(map) => map
                    .into_iter()
                    .map(|(k, v)| (k, DataValue::from(v)))
                    .collect(),
                Err(_) => {
                    return json!({"ok": false, "message": "params argument is not a JSON map"})
                        .to_string();
                }
            }
        };
        self.run_script_fold_err(
            payload,
            params_json,
            if immutable {
                ScriptMutability::Immutable
            } else {
                ScriptMutability::Mutable
            },
        )
        .to_string()
    }
    /// Dispatcher method. See [crate::Db::export_relations].
    pub fn export_relations<I, T>(&self, relations: I) -> Result<BTreeMap<String, NamedRows>>
    where
        T: AsRef<str>,
        I: Iterator<Item = T>,
    {
        match self {
            DbInstance::Mem(db) => db.export_relations(relations),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.export_relations(relations),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.export_relations(relations),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.export_relations(relations),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.export_relations(relations),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.export_relations(relations),
        }
    }
    /// Directly scan one stored relation's primary-key range without compiling
    /// a query or materializing a sort. See [`PrimaryKeyScan`].
    pub fn scan_relation_by_primary_key(
        &self,
        relation: &str,
        scan: &PrimaryKeyScan,
    ) -> Result<PrimaryKeyScanPage> {
        match self {
            DbInstance::Mem(db) => db.scan_relation_by_primary_key(relation, scan),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.scan_relation_by_primary_key(relation, scan),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.scan_relation_by_primary_key(relation, scan),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.scan_relation_by_primary_key(relation, scan),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.scan_relation_by_primary_key(relation, scan),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.scan_relation_by_primary_key(relation, scan),
        }
    }
    /// Make a SQLite database file self-contained before an offline move.
    ///
    /// The caller must have exclusive ownership of this `DbInstance` and must
    /// not issue more database work after this succeeds.
    #[cfg(feature = "storage-sqlite")]
    pub fn prepare_sqlite_for_file_move(&self) -> Result<()> {
        match self {
            DbInstance::Sqlite(db) => db.db.prepare_for_file_move(),
            _ => bail!("database is not backed by sqlite"),
        }
    }
    /// Export relations to JSON-encoded string.
    /// See [crate::Db::export_relations]
    pub fn export_relations_str(&self, data: &str) -> String {
        match self.export_relations_str_inner(data) {
            Ok(s) => {
                let ret = json!({"ok": true, "data": s});
                format!("{ret}")
            }
            Err(err) => {
                let ret = json!({"ok": false, "message": err.to_string()});
                format!("{ret}")
            }
        }
    }
    fn export_relations_str_inner(&self, data: &str) -> Result<JsonValue> {
        #[derive(serde_derive::Deserialize)]
        struct Payload {
            relations: Vec<String>,
        }
        let j_val: Payload = serde_json::from_str(data).into_diagnostic()?;
        let results = self.export_relations(j_val.relations.iter().map(|s| s as &str))?;
        Ok(results
            .into_iter()
            .map(|(k, v)| (k, v.into_json()))
            .collect())
    }
    /// Dispatcher method. See [crate::Db::import_relations].
    pub fn import_relations(&self, data: BTreeMap<String, NamedRows>) -> Result<()> {
        match self {
            DbInstance::Mem(db) => db.import_relations(data),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.import_relations(data),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.import_relations(data),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.import_relations(data),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.import_relations(data),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.import_relations(data),
        }
    }
    /// Import a relation, the data is given as a JSON string, and the returned result is converted into a string.
    /// See [crate::Db::import_relations].
    pub fn import_relations_str(&self, data: &str) -> String {
        match self.import_relations_str_with_err(data) {
            Ok(()) => {
                format!("{}", json!({"ok": true}))
            }
            Err(err) => {
                format!("{}", json!({"ok": false, "message": err.to_string()}))
            }
        }
    }
    /// Import a relation, the data is given as a JSON string.
    /// See [crate::Db::import_relations].
    pub fn import_relations_str_with_err(&self, data: &str) -> Result<()> {
        let json_data: JsonValue = serde_json::from_str(data).into_diagnostic()?;
        let json_object = json_data
            .as_object()
            .ok_or_else(|| miette!("A JSON object is requried"))?;
        let mapping = json_object
            .iter()
            .map(|(k, v)| -> Result<(String, NamedRows)> {
                Ok((k.to_string(), NamedRows::from_json(v)?))
            })
            .collect::<Result<_>>()?;
        self.import_relations(mapping)
    }
    /// Dispatcher method. See [crate::Db::backup_db].
    pub fn backup_db(&self, out_file: impl AsRef<Path>) -> Result<()> {
        match self {
            DbInstance::Mem(db) => db.backup_db(out_file),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.backup_db(out_file),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.backup_db(out_file),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.backup_db(out_file),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.backup_db(out_file),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.backup_db(out_file),
        }
    }
    /// Backup the running database into an Sqlite file, with JSON string return value.
    /// See [crate::Db::backup_db].
    pub fn backup_db_str(&self, out_file: impl AsRef<Path>) -> String {
        match self.backup_db(out_file) {
            Ok(_) => json!({"ok": true}).to_string(),
            Err(err) => json!({"ok": false, "message": err.to_string()}).to_string(),
        }
    }
    /// Dispatcher method. See [crate::Db::restore_backup].
    pub fn restore_backup(&self, in_file: impl AsRef<Path>) -> Result<()> {
        match self {
            DbInstance::Mem(db) => db.restore_backup(in_file),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.restore_backup(in_file),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.restore_backup(in_file),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.restore_backup(in_file),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.restore_backup(in_file),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.restore_backup(in_file),
        }
    }
    /// Restore from an Sqlite backup, with JSON string return value.
    /// See [crate::Db::restore_backup].
    pub fn restore_backup_str(&self, in_file: impl AsRef<Path>) -> String {
        match self.restore_backup(in_file) {
            Ok(_) => json!({"ok": true}).to_string(),
            Err(err) => json!({"ok": false, "message": err.to_string()}).to_string(),
        }
    }
    /// Dispatcher method. See [crate::Db::import_from_backup].
    pub fn import_from_backup(
        &self,
        in_file: impl AsRef<Path>,
        relations: &[String],
    ) -> Result<()> {
        match self {
            DbInstance::Mem(db) => db.import_from_backup(in_file, relations),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.import_from_backup(in_file, relations),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.import_from_backup(in_file, relations),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.import_from_backup(in_file, relations),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.import_from_backup(in_file, relations),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.import_from_backup(in_file, relations),
        }
    }
    /// Import relations from an Sqlite backup, with JSON string return value.
    /// See [crate::Db::import_from_backup].
    pub fn import_from_backup_str(&self, payload: &str) -> String {
        match self.import_from_backup_str_inner(payload) {
            Ok(_) => json!({"ok": true}).to_string(),
            Err(err) => json!({"ok": false, "message": err.to_string()}).to_string(),
        }
    }
    fn import_from_backup_str_inner(&self, payload: &str) -> Result<()> {
        #[derive(serde_derive::Deserialize)]
        struct Payload {
            path: String,
            relations: Vec<String>,
        }
        let json_payload: Payload = serde_json::from_str(payload).into_diagnostic()?;

        self.import_from_backup(&json_payload.path, &json_payload.relations)
    }

    /// Dispatcher method. See [crate::Db::register_callback].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn register_callback(
        &self,
        relation: &str,
        capacity: Option<usize>,
    ) -> (u32, Receiver<(CallbackOp, NamedRows, NamedRows)>) {
        match self {
            DbInstance::Mem(db) => db.register_callback(relation, capacity),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.register_callback(relation, capacity),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.register_callback(relation, capacity),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.register_callback(relation, capacity),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.register_callback(relation, capacity),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.register_callback(relation, capacity),
        }
    }

    /// Dispatcher method. See [crate::Db::unregister_callback].
    #[cfg(not(target_arch = "wasm32"))]
    pub fn unregister_callback(&self, id: u32) -> bool {
        match self {
            DbInstance::Mem(db) => db.unregister_callback(id),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.unregister_callback(id),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.unregister_callback(id),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.unregister_callback(id),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.unregister_callback(id),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.unregister_callback(id),
        }
    }
    /// Dispatcher method. See [crate::Db::register_fixed_rule].
    pub fn register_fixed_rule<R>(&self, name: String, rule_impl: R) -> Result<()>
    where
        R: FixedRule + 'static,
    {
        match self {
            DbInstance::Mem(db) => db.register_fixed_rule(name, rule_impl),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.register_fixed_rule(name, rule_impl),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.register_fixed_rule(name, rule_impl),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.register_fixed_rule(name, rule_impl),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.register_fixed_rule(name, rule_impl),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.register_fixed_rule(name, rule_impl),
        }
    }
    /// Dispatcher method. See [crate::Db::register_custom_aggr].
    pub fn register_custom_aggr<F>(&self, name: String, is_meet: bool, factory: F) -> Result<()>
    where
        F: Fn() -> Box<dyn MeetAggrObj> + Send + Sync + 'static,
    {
        match self {
            DbInstance::Mem(db) => db.register_custom_aggr(name, is_meet, factory),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.register_custom_aggr(name, is_meet, factory),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.register_custom_aggr(name, is_meet, factory),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.register_custom_aggr(name, is_meet, factory),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.register_custom_aggr(name, is_meet, factory),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.register_custom_aggr(name, is_meet, factory),
        }
    }
    /// Dispatcher method. See [crate::Db::register_bounded_meet_aggr].
    pub fn register_bounded_meet_aggr<F>(
        &self,
        name: String,
        dominates: F,
        max_survivors: usize,
    ) -> Result<()>
    where
        F: Fn(&DataValue, &DataValue) -> bool + Send + Sync + 'static,
    {
        match self {
            DbInstance::Mem(db) => db.register_bounded_meet_aggr(name, dominates, max_survivors),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.register_bounded_meet_aggr(name, dominates, max_survivors),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => {
                db.register_bounded_meet_aggr(name, dominates, max_survivors)
            }
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => {
                db.register_bounded_meet_aggr(name, dominates, max_survivors)
            }
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.register_bounded_meet_aggr(name, dominates, max_survivors),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.register_bounded_meet_aggr(name, dominates, max_survivors),
        }
    }
    /// Dispatcher method. See [crate::Db::unregister_bounded_meet_aggr].
    pub fn unregister_bounded_meet_aggr(&self, name: &str) -> Result<bool> {
        match self {
            DbInstance::Mem(db) => db.unregister_bounded_meet_aggr(name),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.unregister_bounded_meet_aggr(name),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.unregister_bounded_meet_aggr(name),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.unregister_bounded_meet_aggr(name),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.unregister_bounded_meet_aggr(name),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.unregister_bounded_meet_aggr(name),
        }
    }
    /// Dispatcher method. See [crate::Db::unregister_custom_aggr].
    pub fn unregister_custom_aggr(&self, name: &str) -> Result<bool> {
        match self {
            DbInstance::Mem(db) => db.unregister_custom_aggr(name),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.unregister_custom_aggr(name),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.unregister_custom_aggr(name),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.unregister_custom_aggr(name),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.unregister_custom_aggr(name),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.unregister_custom_aggr(name),
        }
    }
    /// Dispatcher method. See [crate::Db::unregister_fixed_rule]
    pub fn unregister_fixed_rule(&self, name: &str) -> Result<bool> {
        match self {
            DbInstance::Mem(db) => db.unregister_fixed_rule(name),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.unregister_fixed_rule(name),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.unregister_fixed_rule(name),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.unregister_fixed_rule(name),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.unregister_fixed_rule(name),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.unregister_fixed_rule(name),
        }
    }

    /// Dispatcher method. See [crate::Db::run_multi_transaction]
    pub fn run_multi_transaction(
        &self,
        write: bool,
        payloads: Receiver<TransactionPayload>,
        results: Sender<Result<NamedRows>>,
    ) {
        match self {
            DbInstance::Mem(db) => db.run_multi_transaction(write, payloads, results),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.run_multi_transaction(write, payloads, results),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.run_multi_transaction(write, payloads, results),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.run_multi_transaction(write, payloads, results),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.run_multi_transaction(write, payloads, results),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.run_multi_transaction(write, payloads, results),
        }
    }

    fn run_read_multi_transaction_with_deadline(
        &self,
        payloads: Receiver<TransactionPayload>,
        results: Sender<Result<NamedRows>>,
        startup: Sender<Result<()>>,
        deadline: Instant,
    ) {
        match self {
            DbInstance::Mem(db) => {
                db.run_read_multi_transaction_with_deadline(payloads, results, startup, deadline)
            }
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => {
                db.run_read_multi_transaction_with_deadline(payloads, results, startup, deadline)
            }
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => {
                db.run_read_multi_transaction_with_deadline(payloads, results, startup, deadline)
            }
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => {
                db.run_read_multi_transaction_with_deadline(payloads, results, startup, deadline)
            }
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => {
                db.run_read_multi_transaction_with_deadline(payloads, results, startup, deadline)
            }
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => {
                db.run_read_multi_transaction_with_deadline(payloads, results, startup, deadline)
            }
        }
    }

    /// A higher-level, blocking wrapper for [crate::Db::run_multi_transaction]. Runs the transaction on a dedicated thread.
    /// Write transactions _may_ block other reads, but we guarantee that this does not happen for the RocksDB backend.
    pub fn multi_transaction(&self, write: bool) -> MultiTransaction {
        let (app2db_send, app2db_recv) = bounded(1);
        let (db2app_send, db2app_recv) = bounded(1);
        let db = self.clone();
        // A dedicated thread, as documented — NOT `rayon::spawn` (which this
        // used until 0.11.0): the transaction loop blocks in `recv` between
        // scripts for the transaction's whole life, and parking a global-pool
        // worker that long starves the pool. With `available_parallelism` open
        // transactions — or one open transaction racing any parallel query on
        // a single-core host — every rayon-using query in the process
        // deadlocks. (mnestic fork; found by the graph-projection interleaving
        // suite, where one open reader plus one par-sorted CSR build wedged a
        // one-worker run.)
        std::thread::spawn(move || db.run_multi_transaction(write, app2db_recv, db2app_send));
        MultiTransaction {
            sender: app2db_send,
            receiver: db2app_recv,
        }
    }

    /// Open a read-only multi-transaction with one whole-transaction
    /// cooperative wall-clock budget.
    ///
    /// The monotonic deadline is anchored before the transaction worker is
    /// spawned, so scheduling delay, storage snapshot setup, every CozoScript
    /// query, and every direct primary-key scan consume the same budget. A
    /// script's own `:timeout` may tighten this deadline but never re-arm or
    /// extend it. [`Self::multi_transaction`] is unchanged and remains
    /// unbudgeted; write transactions deliberately have no deadline variant.
    ///
    /// A zero duration normally expires during startup and returns the typed
    /// timeout instead of a usable handle. Deadline overflow, a missing
    /// monotonic clock, or worker-spawn failure is reported synchronously
    /// rather than silently disabling the guard.
    ///
    /// This is intended for audited read scripts, not as a hostile-query
    /// sandbox: backend calls, one output sort, one parse/compile phase, and
    /// extension code that does not poll the query poison remain individually
    /// non-preemptible. Bounded queries deliberately do not appear in
    /// `::running` and cannot be targeted by `::kill`; their immutable deadline
    /// is the cancellation mechanism, avoiding that global registry's
    /// unbounded mutex. Unsupported storage backends fail during startup.
    pub fn read_multi_transaction_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<BoundedReadTransaction> {
        #[cfg(target_arch = "wasm32")]
        let deadline: Instant =
            bail!("read multi-transaction deadlines require a monotonic clock");

        #[cfg(not(target_arch = "wasm32"))]
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| miette!("read multi-transaction deadline overflow"))?;

        let (app2db_send, app2db_recv) = bounded(1);
        let (db2app_send, db2app_recv) = bounded(1);
        let (startup_send, startup_recv) = bounded(1);
        let db = self.clone();
        let worker = std::thread::Builder::new()
            .name("mnestic-read-transaction".to_string())
            .spawn(move || {
                db.run_read_multi_transaction_with_deadline(
                    app2db_recv,
                    db2app_send,
                    startup_send,
                    deadline,
                )
            })
            .into_diagnostic()?;
        let mut transaction = BoundedReadTransaction {
            sender: Some(app2db_send),
            receiver: db2app_recv,
            worker: Some(worker),
            deadline,
            request_timed_out: false,
            #[cfg(test)]
            teardown_started: None,
        };
        match startup_recv.recv() {
            Ok(Ok(())) => Ok(transaction),
            Ok(Err(error)) => match transaction.join_worker() {
                Ok(()) => Err(error),
                Err(panic) => Err(panic),
            },
            Err(error) => {
                let channel = miette!("bounded read transaction startup channel failed: {error}");
                match transaction.join_worker() {
                    Ok(()) => Err(channel),
                    Err(panic) => Err(panic),
                }
            }
        }
    }
    /// Dispatcher method. See [crate::Db::set_graph_projection_capacity].
    /// (mnestic fork; without this the graph-projection memory ceiling was
    /// unreachable from every language binding and from `cozo-bin`, while the
    /// engine's oversize-variant warning named it as the remedy.)
    #[cfg(feature = "graph-algo")]
    pub fn set_graph_projection_capacity(&self, bytes: usize) {
        match self {
            DbInstance::Mem(db) => db.set_graph_projection_capacity(bytes),
            #[cfg(feature = "storage-sqlite")]
            DbInstance::Sqlite(db) => db.set_graph_projection_capacity(bytes),
            #[cfg(feature = "storage-rocksdb")]
            DbInstance::RocksDb(db) => db.set_graph_projection_capacity(bytes),
            #[cfg(feature = "storage-new-rocksdb")]
            DbInstance::NewRocksDb(db) => db.set_graph_projection_capacity(bytes),
            #[cfg(feature = "storage-sled")]
            DbInstance::Sled(db) => db.set_graph_projection_capacity(bytes),
            #[cfg(feature = "storage-tikv")]
            DbInstance::TiKv(db) => db.set_graph_projection_capacity(bytes),
        }
    }
}

/// A multi-transaction handle.
/// You should use either the fields directly, or the associated functions.
pub struct MultiTransaction {
    /// Commands can be sent into the transaction through this channel
    pub sender: Sender<TransactionPayload>,
    /// Results can be retrieved from the transaction from this channel
    pub receiver: Receiver<Result<NamedRows>>,
}

impl MultiTransaction {
    /// Runs a single script in the transaction.
    pub fn run_script(
        &self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        if let Err(err) = self
            .sender
            .send(TransactionPayload::Query((payload.to_string(), params)))
        {
            bail!(err);
        }
        match self.receiver.recv() {
            Ok(r) => r,
            Err(err) => bail!(err),
        }
    }
    /// Run one allowlisted relation-catalog system operation in this SQLite
    /// write multi-transaction.
    ///
    /// This deliberately accepts only `::index create`, `::fts create`,
    /// `::hnsw create`, or `::access_level`. It is not a general imperative or
    /// system-command escape hatch: process-local and independently committing
    /// operations cannot be composed into an outer storage transaction safely.
    /// The payload must parse as exactly one unbraced system operation and
    /// takes no parameters.
    ///
    /// This capability is intentionally SQLite-only. SQLite holds one storage-level
    /// writer for the outer transaction, so relation locks cannot be released
    /// between index construction and commit while another writer slips rows into
    /// the base relation. RocksDB's live SST-ingest path is also therefore
    /// unreachable from this API.
    ///
    /// Backend, parse, allowlist, read-only, and execution failures are terminal.
    /// The transaction worker drops the outer storage transaction before returning
    /// the error, because a catalog operation may have staged writes before it
    /// discovers a later refusal.
    pub fn run_sqlite_catalog_system_operation(&self, payload: &str) -> Result<NamedRows> {
        if let Err(err) = self.sender.send(
            TransactionPayload::SqliteCatalogSystemOperation(payload.to_owned()),
        ) {
            bail!(err);
        }
        match self.receiver.recv() {
            Ok(result) => result,
            Err(err) => bail!(err),
        }
    }
    /// Runs a bounded direct physical-primary-key scan in this transaction.
    ///
    /// This is supported only by read-only multi-transactions. Repeated calls
    /// and ordinary queries on the same handle observe one storage snapshot.
    pub fn scan_relation_by_primary_key(
        &self,
        relation: &str,
        scan: PrimaryKeyScan,
    ) -> Result<NamedRows> {
        if let Err(err) = self.sender.send(TransactionPayload::PrimaryKeyScan(
            relation.to_string(),
            scan,
        )) {
            bail!(err);
        }
        match self.receiver.recv() {
            Ok(result) => result,
            Err(err) => bail!(err),
        }
    }
    /// Canonicalize the complete persistent relation catalog named by
    /// `relation_names` through Mnestic's frozen struct-map v1 wire codec.
    ///
    /// This capability is supported only on a write multi-transaction. The
    /// caller must supply the exact complete, duplicate-free name set and its
    /// operation-specific relation cap (at most 256). Mnestic preflights the
    /// full id-zero catalog before its first write. Any refusal or storage
    /// failure aborts this outer transaction, including earlier staged work.
    pub fn canonicalize_relation_catalog_v1(
        &self,
        relation_names: Vec<String>,
        relation_count_cap: usize,
    ) -> Result<()> {
        if let Err(err) = self
            .sender
            .send(TransactionPayload::CanonicalizeRelationCatalogV1(
                relation_names,
                relation_count_cap,
            ))
        {
            bail!(err);
        }
        match self.receiver.recv() {
            Ok(result) => result.map(|_| ()),
            Err(err) => bail!(err),
        }
    }
    /// Commits the multi-transaction.
    ///
    /// Returns the commit's own error if it failed — the caller must not treat a
    /// failed commit as durable. (mnestic fork, 0.12.1: this used to match
    /// `Ok(_) => Ok(())`, which discarded the `Result<NamedRows>` the
    /// transaction thread sends back and so reported **success for a failed
    /// commit**. `run_script` above has always propagated it correctly.)
    pub fn commit(&self) -> Result<()> {
        if let Err(err) = self.sender.send(TransactionPayload::Commit) {
            bail!(err);
        }
        match self.receiver.recv() {
            Ok(r) => r.map(|_| ()),
            Err(err) => bail!(err),
        }
    }
    /// Aborts the multi-transaction
    pub fn abort(&self) -> Result<()> {
        if let Err(err) = self.sender.send(TransactionPayload::Abort) {
            bail!(err);
        }
        match self.receiver.recv() {
            Ok(r) => r.map(|_| ()),
            Err(err) => bail!(err),
        }
    }
}

/// Opaque, read-only multi-transaction carrying one immutable monotonic
/// deadline. Unlike [`MultiTransaction`], its channels cannot be cloned or
/// driven out of request/response order. Consuming [`Self::close_and_join`] or
/// dropping the handle synchronously closes and joins its worker. If either
/// operation returns, the storage snapshot has actually gone away.
///
/// Drop cannot report worker panic provenance, so callers that need that
/// diagnostic should use [`Self::close_and_join`]. Both paths wait without a
/// second deadline for an already-running non-preemptible backend, sort,
/// compiler phase, or extension region. They may therefore never return if an
/// uncooperative backend or extension never returns; own and destroy this
/// handle on a blocking-capable thread.
pub struct BoundedReadTransaction {
    sender: Option<Sender<TransactionPayload>>,
    receiver: Receiver<Result<NamedRows>>,
    worker: Option<std::thread::JoinHandle<()>>,
    deadline: Instant,
    request_timed_out: bool,
    #[cfg(test)]
    teardown_started: Option<Sender<()>>,
}

impl BoundedReadTransaction {
    /// Run one read-only CozoScript statement inside the shared snapshot.
    pub fn run_script(
        &mut self,
        payload: &str,
        params: BTreeMap<String, DataValue>,
    ) -> Result<NamedRows> {
        self.request(TransactionPayload::Query((payload.to_string(), params)))
    }

    /// Run one bounded physical-primary-key page inside the shared snapshot.
    pub fn scan_relation_by_primary_key(
        &mut self,
        relation: &str,
        scan: PrimaryKeyScan,
    ) -> Result<NamedRows> {
        self.request(TransactionPayload::PrimaryKeyScan(
            relation.to_string(),
            scan,
        ))
    }

    fn request(&mut self, payload: TransactionPayload) -> Result<NamedRows> {
        ensure!(
            !self.request_timed_out,
            "a bounded read request timed out locally; close the transaction before continuing"
        );
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| miette!("bounded read transaction is already closed"))?;
        if let Err(error) = sender.send(payload) {
            let fallback = miette!("bounded read transaction request channel failed: {error}");
            return self.finish_disconnected(fallback);
        }
        match self.receiver.recv_deadline(self.deadline) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.request_timed_out = true;
                Err(runtime::db::timeout_report())
            }
            Err(RecvTimeoutError::Disconnected) => self.finish_disconnected(miette!(
                "bounded read transaction result channel disconnected"
            )),
        }
    }

    fn finish_disconnected(&mut self, fallback: Report) -> Result<NamedRows> {
        let terminal = self.receiver.try_recv().ok();
        self.join_worker()?;
        terminal.unwrap_or(Err(fallback))
    }

    /// Close the read transaction and wait until its worker, including storage
    /// snapshot destruction, has returned. Natural deadline expiry counts as a
    /// successful close; a worker panic is retained and returned. This is a
    /// completion join, not a second cancellation deadline, so it can wait for
    /// a non-preemptible backend or extension call already in progress. If
    /// that call never returns, neither does this method; if this method does
    /// return, the worker has joined and the snapshot has been destroyed.
    pub fn close_and_join(mut self) -> Result<()> {
        self.teardown()
    }

    fn teardown(&mut self) -> Result<()> {
        #[cfg(test)]
        if let Some(started) = self.teardown_started.take() {
            let _ = started.send(());
        }

        if let Some(sender) = self.sender.take() {
            close_bounded_request_channel(sender);
        }

        // Join cannot come first: the worker may be blocked publishing into
        // the capacity-one result channel. Drain through disconnect so this is
        // safe even when a timed-out result is already buffered and another
        // command occupies the capacity-one request channel.
        while self.receiver.recv().is_ok() {}
        self.join_worker()
    }

    fn join_worker(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        match worker.join() {
            Ok(()) => Ok(()),
            Err(payload) => {
                let detail = if let Some(message) = payload.downcast_ref::<&str>() {
                    (*message).to_string()
                } else if let Some(message) = payload.downcast_ref::<String>() {
                    message.clone()
                } else {
                    "non-string panic payload".to_string()
                };
                bail!("bounded read transaction worker panicked: {detail}")
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn panic_worker_for_tests(&mut self) -> Result<()> {
        self.request(TransactionPayload::PanicWorkerForTests)
            .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn block_worker_for_tests(
        &mut self,
        release: Receiver<()>,
        ready: Sender<()>,
    ) -> Result<()> {
        self.request(TransactionPayload::BlockWorkerForTests(
            runtime::db::WorkerTestGate::new(release, ready),
        ))
        .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn pause_before_next_receive_for_tests(
        &mut self,
        release: Receiver<()>,
        ready: Sender<()>,
        buffer_result: bool,
    ) -> Result<()> {
        self.request(TransactionPayload::PauseBeforeNextReceiveForTests(
            runtime::db::WorkerTestGate::new(release, ready),
            buffer_result,
        ))
        .map(|_| ())
    }

    #[cfg(test)]
    pub(crate) fn queue_worker_panic_for_tests(&self) -> Result<()> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| miette!("bounded read transaction is already closed"))?;
        sender
            .send(TransactionPayload::PanicWorkerForTests)
            .map_err(|error| miette!("bounded read transaction request channel failed: {error}"))
    }

    #[cfg(test)]
    pub(crate) fn notify_teardown_started_for_tests(&mut self, started: Sender<()>) {
        self.teardown_started = Some(started);
    }
}

impl Drop for BoundedReadTransaction {
    fn drop(&mut self) {
        // Destructors must not introduce a second panic while unwinding. The
        // explicit close path remains available when the caller needs errors.
        // Teardown may wait forever for an uncooperative in-flight region, but
        // if it returns then the worker has joined and its snapshot is gone.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = self.teardown();
        }));
    }
}

/// Try to enqueue the normal Abort terminator without ever blocking on a
/// request that still occupies the capacity-one command channel, then drop the
/// final sender so the worker exits after any already-queued work.
fn close_bounded_request_channel(sender: Sender<TransactionPayload>) {
    let _ = sender.try_send(TransactionPayload::Abort);
    drop(sender);
}

/// Convert error raised by the database into friendly JSON format
pub fn format_error_as_json(mut err: Report, source: Option<&str>) -> JsonValue {
    if err.source_code().is_none() {
        if let Some(src) = source {
            err = err.with_source_code(format!("{src} "));
        }
    }
    let mut text_err = String::new();
    let mut json_err = String::new();
    TEXT_ERR_HANDLER
        .render_report(&mut text_err, err.as_ref())
        .expect("render text error failed");
    JSON_ERR_HANDLER
        .render_report(&mut json_err, err.as_ref())
        .expect("render json error failed");
    let mut json: serde_json::Value =
        serde_json::from_str(&json_err).expect("parse rendered json error failed");
    let map = json.as_object_mut().unwrap();
    map.insert("ok".to_string(), json!(false));
    map.insert("display".to_string(), json!(text_err));
    json
}

lazy_static! {
    static ref TEXT_ERR_HANDLER: GraphicalReportHandler = miette::GraphicalReportHandler::new()
        .with_theme(GraphicalTheme {
            characters: ThemeCharacters::unicode(),
            styles: ThemeStyles::ansi()
        });
    static ref JSON_ERR_HANDLER: JSONReportHandler = miette::JSONReportHandler::new();
}
