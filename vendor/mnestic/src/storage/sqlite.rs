/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString, c_void};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Instant, SystemTime};
#[cfg(test)]
use std::{cell::Cell, sync::MutexGuard};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{FileExt, OpenOptionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

use ::sqlite::Connection;
use crossbeam::sync::{ShardedLock, ShardedLockReadGuard, ShardedLockWriteGuard};
use either::{Either, Left, Right};
use miette::{IntoDiagnostic, Result, bail, miette};
use sha2::{Digest, Sha256};
use sqlite::{ConnectionThreadSafe, OpenFlags, State, Statement};
use sqlite3_sys as ffi;

use crate::data::memcmp::{MAX_ENCODED_KEY_BYTES, STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1};
use crate::data::msgpack::{
    STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1,
    STORED_MSGPACK_RELATION_CATALOG_POLICY_FINGERPRINT_V1,
    STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1, StoredMsgpackProfile,
};
use crate::data::tuple::{Tuple, TupleT, check_key_for_validity, try_decode_tuple_from_key};
use crate::data::value::{DataValue, STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1, ValidityTs};
#[cfg(any(test, feature = "test-hooks"))]
use crate::runtime::catalog_codec::{
    CatalogFixtureRewriteV1, ManagedCatalogFixtureV1, rewrite_catalog_fixture_records_v1,
};
use crate::runtime::catalog_codec::{ManagedCatalogCensusV1, decode_unattested_catalog};
use crate::runtime::relation::{
    RelationId, STORED_RELATION_ID_POLICY_FINGERPRINT_V1, try_decode_tuple_from_kv,
    try_decode_val_only, try_extend_tuple_from_v,
};
use crate::storage::{Storage, StoreTx, park_before_deadline, read_sharded_before_deadline};
use crate::utils::swap_option_result;

pub const SQLITE_BUSY_TIMEOUT_MS: u64 = 250;

const SQLITE_MAIN_SCHEMA: &[u8] = b"main\0";
const SQLITE_SCHEMA_PROBE: &[u8] = b"SELECT count(*) >= 0 FROM main.sqlite_master;\0";
const SQLITE_SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];
const MIN_SNAPSHOT_SQLITE_VERSION: i32 = 3_031_000;
const MAX_SNAPSHOT_SHM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SNAPSHOT_SCHEMA_CELLS: u16 = 64;
const MANAGED_SQLITE_SCHEMA_LENGTH_LIMIT: i32 = 64 * 1024;
const MANAGED_SQLITE_SQL_LENGTH_LIMIT: i32 = 4 * 1024;
const MANAGED_SQLITE_COLUMN_LIMIT: i32 = 16;
const MANAGED_SQLITE_EXPR_DEPTH_LIMIT: i32 = 32;
const MANAGED_SQLITE_COMPOUND_SELECT_LIMIT: i32 = 1;
const MANAGED_SQLITE_VDBE_OP_LIMIT: i32 = 4 * 1024;
const MANAGED_SQLITE_FUNCTION_ARG_LIMIT: i32 = 16;
const MANAGED_SQLITE_LIKE_PATTERN_LIMIT: i32 = 128;
const MANAGED_SQLITE_VARIABLE_LIMIT: i32 = 4;
const MANAGED_SQLITE_ATTACHED_LIMIT: i32 = 0;
const MANAGED_SQLITE_TRIGGER_DEPTH_LIMIT: i32 = 0;
const MANAGED_SQLITE_WORKER_THREADS_LIMIT: i32 = 0;
const MANAGED_SQLITE_DEFENSIVE_VALUE: i32 = 1;
const MANAGED_SQLITE_TRUSTED_SCHEMA_VALUE: i32 = 0;
const MANAGED_SQLITE_ENABLE_TRIGGER_VALUE: i32 = 0;
const MANAGED_SQLITE_QUERY_ONLY_VALUE: i32 = 1;
const MANAGED_CATALOG_RELATION_COUNT: usize = 26;
const MANAGED_CATALOG_ROW_COUNT: usize = MANAGED_CATALOG_RELATION_COUNT + 2;
const MANAGED_CATALOG_QUERY_ROW_LIMIT: usize = MANAGED_CATALOG_ROW_COUNT + 1;
const MANAGED_CATALOG_VALUE_LIMIT: usize = StoredMsgpackProfile::RelationCatalog.byte_limit();
const MANAGED_CATALOG_CUMULATIVE_RAW_LIMIT: usize = 8 * 1024 * 1024;
const MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT: usize = 8 * 1024 * 1024;
const MANAGED_ROW_VALUE_LIMIT: usize = StoredMsgpackProfile::Row.byte_limit();
const MANAGED_PHYSICAL_MAX_FILE_BYTES: u64 = 1_u64 << 40;
const MANAGED_PHYSICAL_MAX_PAGE_COUNT: u64 = 1_u64 << 24;
const MANAGED_PHYSICAL_CACHE_SIZE_KIB: i32 = 65_536;
const MANAGED_PHYSICAL_PROGRESS_INTERVAL: i32 = 1_024;
const MANAGED_PHYSICAL_PROGRESS_CALLBACK_LIMIT: u64 = 1_u64 << 27;
const MANAGED_ASSERTION_PROGRESS_INTERVAL: i32 = 1_024;
const MANAGED_ASSERTION_PROGRESS_CALLBACK_LIMIT: u64 = 4_096;
const MANAGED_ASSERTION_LIMIT: usize = 8;
const MANAGED_ASSERTION_POINT_LIMIT: usize = 8;
const MANAGED_ASSERTION_STRING_BYTE_LIMIT: usize = 256;
const MANAGED_ASSERTION_POINT_VALUE_BYTE_LIMIT: usize = 1_024;
const MANAGED_ASSERTION_TRANSCRIPT_BYTE_LIMIT: u64 = 8_801;
const MANAGED_RECORD_VISIT_RELATION_COUNT_V1: usize = 16;
const MANAGED_RECORD_VISIT_TOTAL_ROW_LIMIT_V1: u64 = 1_u64 << 24;
#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_CATALOG_FIXTURE_WRITE_CALLBACK_LIMIT: u64 = 4_096;
const MANAGED_SQLITE_RUNTIME_VERSION_LIMIT: usize = 32;
const MANAGED_SQLITE_RUNTIME_SOURCE_ID_LIMIT: usize = 128;
// SQLite applies SQLITE_LIMIT_LENGTH to both individual values and complete
// encoded rows. The fixed query returns one bounded key and one bounded
// catalog value plus two integer lengths. This explicit headroom covers the
// SQLite record header and varints without relaxing the 4 MiB value fuse.
const MANAGED_SQLITE_CATALOG_LENGTH_LIMIT: i32 =
    (MANAGED_CATALOG_VALUE_LIMIT + MAX_ENCODED_KEY_BYTES + 1024) as i32;

const MANAGED_BEGIN_READ_SQL: &[u8] = b"BEGIN DEFERRED TRANSACTION;\0";
const MANAGED_ROLLBACK_READ_SQL: &[u8] = b"ROLLBACK;\0";
const MANAGED_CATALOG_QUERY: &[u8] = b"SELECT length(k), length(v), k, v \
    FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 \
    WHERE k >= x'0000000000000000' AND k < x'0000000000000001' \
    ORDER BY k LIMIT 29;\0";
const MANAGED_MMAP_DISABLE_SQL: &[u8] = b"PRAGMA main.mmap_size=0;\0";
const MANAGED_CACHE_SIZE_SQL: &[u8] = b"PRAGMA main.cache_size=-65536;\0";
const MANAGED_CELL_SIZE_CHECK_SQL: &[u8] = b"PRAGMA cell_size_check=ON;\0";
const MANAGED_MMAP_CHECK: &[u8] = b"PRAGMA main.mmap_size;\0";
const MANAGED_CACHE_SIZE_CHECK: &[u8] = b"PRAGMA main.cache_size;\0";
const MANAGED_CELL_SIZE_CHECK: &[u8] = b"PRAGMA cell_size_check;\0";
const MANAGED_PAGE_COUNT_QUERY: &[u8] = b"PRAGMA main.page_count;\0";
const MANAGED_PAGE_SIZE_QUERY: &[u8] = b"PRAGMA main.page_size;\0";
const MANAGED_INTEGRITY_CHECK_QUERY: &[u8] = b"PRAGMA main.integrity_check(1);\0";
const MANAGED_TABLE_CENSUS_QUERY: &[u8] = b"SELECT rowid, length(k), length(v), k, v \
    FROM main.cozo NOT INDEXED ORDER BY rowid;\0";
const MANAGED_POINT_QUERY: &[u8] = b"SELECT rowid, length(k), length(v), k, v \
    FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k = ?1;\0";
const MANAGED_CATALOG_FENCE_RANGE_QUERY_V1: &[u8] = b"SELECT 1 \
    FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 \
    WHERE k >= ?1 AND k < ?2 ORDER BY k LIMIT ?3;\0";
const MANAGED_RECORD_VISIT_RANGE_QUERY_V1: &[u8] = b"SELECT length(k), length(v), k, v \
    FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 \
    WHERE k >= ?1 AND k < ?2 ORDER BY k LIMIT ?3;\0";
const MANAGED_CATALOG_FENCE_PROGRESS_INTERVAL_V1: i32 = 256;
const MANAGED_CATALOG_FENCE_PROGRESS_CALLBACK_LIMIT_V1: u64 = 1_024;
const MANAGED_CATALOG_FENCE_ASSERTION_LIMIT_V1: usize = 8;
const MANAGED_CATALOG_FENCE_RANGE_LIMIT_V1: usize = 8;
const MANAGED_CATALOG_FENCE_POINT_LIMIT_V1: usize = 8;
const MANAGED_CATALOG_FENCE_EXPECTED_MAX_V1: u64 = 8;
const MANAGED_CATALOG_FENCE_STRING_LIMIT_V1: usize = 256;
const MANAGED_CATALOG_FENCE_POINT_VALUE_LIMIT_V1: usize = 1_024;
const MANAGED_CATALOG_FENCE_TRANSCRIPT_BYTE_LIMIT_V1: u64 = 8_801;
const MANAGED_COVERING_INDEX_QUERY: &[u8] = b"SELECT length(k), k, rowid \
    FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 ORDER BY k;\0";
const MANAGED_COVERING_INDEX_PLAN_QUERY: &[u8] = b"EXPLAIN QUERY PLAN \
    SELECT length(k), k, rowid FROM main.cozo \
    INDEXED BY sqlite_autoindex_cozo_1 ORDER BY k;\0";
const MANAGED_COVERING_INDEX_PLAN_DETAIL_V1: &[u8] =
    b"SCAN main.cozo USING COVERING INDEX sqlite_autoindex_cozo_1";

const MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-relation-assertion.transcript.v1\0";
#[cfg(test)]
const MANAGED_ASSERTION_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-relation-assertion.policy-fingerprint-transcript.v1\0";
const MANAGED_ASSERTION_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-relation-assertion-policy.v1\n",
    "authority=conditional typed assertions inside one already-pinned managed SQLite READ transaction; excludes source binding, F7 semantics, mutation, and authentication\n",
    "inputs=closed methods only: exact row count, exact string pair, ordered two-alternative string pair with unique outcome tag\n",
    "ids=positive persisted relation id below 2^48 and present in the same cached physical census\n",
    "strings=nonempty static UTF-8 key,value,alternative,tag; each <=256 bytes; alternatives distinct\n",
    "counts=at most 8 assertions and 8 point reads; count ids, point id+key pairs, and outcome tags are independently unique\n",
    "query=SELECT rowid,length(k),length(v),k,v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k=?1; exact fixed SQL bytes include whitespace and NUL terminator\n",
    "point=one reused statement; exact reset,clear,bind; bounded encoded key copied by SQLite with SQLITE_TRANSIENT; INTEGER rowid/lengths and BLOB cells; canonical requested key byte match; value<=1024 before pointer; exactly one ROW then DONE; reset+clear every ordinary path and finalize on unwind\n",
    "decode=exact canonical memcmp [String(key)] plus exact bounded MessagePack row [String(value)] with no extra or trailing data\n",
    "progress=separate assertion-phase handler at interval 1024 and callback limit 4096; never merged with the catalog+physical census budget\n",
    "transcript=domain-separated ordered entries; marker 01; each field u64be-length framed; entry carries u64be field-count; terminator ff plus u64be entry-count; maximum 8801 framed bytes\n",
    "one-of=tag,id,key,both alternatives in caller order,selected index; selected database value is not exposed by the planner\n",
    "failure=first method or callback error permanently poisons the planner and reader runner; caught errors cannot yield evidence\n",
    "completion=statement sqlite3_finalize must return SQLITE_OK; progress handler is unregistered; same READ transaction is rechecked before private evidence\n",
);
// SHA-256 of the domain-separated, length-framed generic assertion-policy
// transcript. The literal is repinned only through the independent test oracle.
const MANAGED_ASSERTION_POLICY_FINGERPRINT_V1: [u8; 32] = [
    193, 250, 7, 93, 127, 176, 92, 94, 83, 47, 230, 253, 32, 228, 226, 40, 72, 174, 33, 87, 12,
    192, 203, 184, 160, 78, 87, 229, 147, 102, 231, 58,
];

const MANAGED_RECORD_VISIT_TRANSCRIPT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-record-visit.transcript.v1\0";
#[cfg(test)]
const MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-record-visit.policy-fingerprint.v1\0";
const MANAGED_RECORD_VISIT_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-record-visit-policy.v1\n",
    "authority=conditional one-pass decoded record delivery from sixteen caller-selected admitted positive relations inside the same pinned managed SQLite READ transaction; excludes relation-name or semantic-role claims, authentication, mutation, and downstream sink verification\n",
    "selection=exact [u64;16] caller order becomes ordinals 1..=16; ids are valid, nonzero, unique, and present in the completed physical census; exact physical counts sum to at most 16777216 before statement preparation\n",
    "query=one private reused SELECT length(k),length(v),k,v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k>=?1 AND k<?2 ORDER BY k LIMIT ?3; exact fixed SQL bytes include whitespace and NUL; lower and checked-next upper relation prefixes use SQLITE_TRANSIENT; limit is expected-count+1\n",
    "row=INTEGER lengths and BLOB cells; key<=65536 and value<=1048576 before sqlite3_column_blob or decode; exact selected relation prefix; complete bounded memcmp key decode plus canonical whole-key re-encode; complete bounded MessagePack value decode including exact relation prefix and EOF; raw keys strictly increase\n",
    "events=BeginRelation(ordinal,expected_rows), then zero or more Record(ordinal,key-values,value-values), then EndRelation(ordinal,rows); no relation names, physical ids, SQL, raw bytes, rowid, statement, or cursor are exposed\n",
    "completion=each range reaches exact DONE with exact physical count; SQLITE_STMTSTATUS_SORT and FULLSCAN_STEP are zero; statement reset+clear occurs before and after every relation and sqlite3_finalize must return SQLITE_OK; READ state is rechecked\n",
    "progress=record scan uses only the remaining cumulative census budget at interval 1024 and total callback limit 134217728 after subtracting catalog+physical callbacks; assertion work retains its separate fixed budget\n",
    "transcript=SHA256 domain then selection marker 00, u16be relation-count, repeated ordinal-u16be,id-u64be,count-u64be,declared-total-u64be; relation marker 01 plus ordinal,id,expected; row marker 02 plus ordinal,u64be key length,exact raw key,u64be value length,exact raw value; end marker 03 plus ordinal,rows; terminator ff plus u16be relation-count,total-rows,cumulative-progress-callbacks\n",
    "failure=selection, preparation, bind, step, length, decode, callback, ordering, count, plan-status, reset, clear, finalize, progress, or READ-state failure poisons the reader; consuming cleanup still reconciles and rolls back, strictly closes, and validates the source; the tentative sink is returned only with closed audit and visit evidence after every close/source postcondition\n",
);
// SHA-256 of the independently framed fixed visitor policy. The literal is
// repinned only through the test oracle below.
const MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1: [u8; 32] = [
    239, 198, 251, 90, 175, 145, 95, 155, 127, 112, 219, 59, 247, 119, 16, 150, 121, 86, 44, 137,
    198, 227, 3, 65, 195, 148, 65, 189, 136, 135, 190, 218,
];

// These deliberately describe the bounded routine-open fence rather than the
// G2 physical audit.  Keep the identities independent: changing G2 must not
// silently repin routine admission, and vice versa.
const MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-catalog-fence-assertion.transcript.v1\0";
#[cfg(test)]
const MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-catalog-fence-assertion.policy-fingerprint.v1\0";
const MANAGED_CATALOG_FENCE_ASSERTION_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-catalog-fence-assertion-policy.v1\n",
    "authority=conditional forced-primary-index point/range observations under one fresh managed SQLite READ transaction; excludes physical integrity, completeness, mutation, and authentication\n",
    "vocabulary=exact row count expected<=8; exact string pair; ordered distinct two-alternative string pair\n",
    "range=SELECT 1 FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k>=?1 AND k<?2 ORDER BY k LIMIT ?3; lower is relation id prefix, upper is checked next id prefix, both SQLITE_TRANSIENT; exact DONE; SORT=0; FULLSCAN_STEP=0; never COUNT(*)\n",
    "point=reused forced primary-index point query with reset/clear/transient bind/exact one row then DONE; value<=1024\n",
    "admission=under the installed handler and before BEGIN/catalog scan, main-file metadata length<=1099511627776\n",
    "caps=assertions,ranges,points<=8; strings<=256; point value<=1024; transcript<=8801; handler interval=256 callbacks<=1024 spans catalog scan and all assertions\n",
    "uniqueness=count ids, point id+key pairs, and one-of tags; all checked before more SQL\n",
    "transcript=separate domain; ordered fields with u64be lengths, field count, ff terminator and entry count; one-of binds both alternatives and selected index\n",
    "completion=both statements explicitly finalized SQLITE_OK, handler removed, READ rechecked; callback/method error poisons runner\n",
);
const MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1: [u8; 32] = [
    188, 112, 86, 144, 47, 34, 215, 65, 89, 20, 208, 245, 196, 214, 22, 253, 119, 190, 177, 57, 88,
    55, 153, 148, 108, 88, 168, 152, 146, 189, 250, 50,
];
#[cfg(test)]
const MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-closed-catalog-fence.policy-fingerprint.v1\0";
const MANAGED_CLOSED_CATALOG_FENCE_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-closed-catalog-fence-policy.v1\n",
    "selector=ManagedCatalogFencePolicy::V1 only; public construction consumes ExistingSqliteSnapshotSource into a private initial Ready reader with no catalog cache, transaction, or physical census\n",
    "evidence=catalog plus separate bounded light assertions plus supplied snapshot policy plus strict close/source evidence; no physical census, integrity, page/table/index census, or cleanup authority\n",
    "lifecycle=one progress handler spans catalog and assertions; reconcile tracked/live transaction, rollback when either active, require autocommit, strict sqlite3_close, then full main/residual/main source bracket before evidence\n",
    "composition=managed snapshot selector, catalog policy, closed source policy, and light assertion policy remain separately exposed\n",
);
const MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    162, 40, 41, 176, 139, 255, 193, 176, 196, 193, 88, 125, 168, 149, 230, 47, 104, 81, 247, 121,
    184, 236, 178, 28, 34, 240, 12, 124, 90, 177, 87, 235,
];

const MANAGED_CLOSED_SOURCE_IDENTITY_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-closed-source-identity.transcript.v1\0";
#[cfg(test)]
const MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-closed-source-policy-fingerprint.transcript.v1\0";
const MANAGED_SNAPSHOT_POLICY_V1_IDENTITY_BYTES: &[u8] = b"mnestic.managed-snapshot-policy.v1";
const MANAGED_CLOSED_SOURCE_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-closed-source-policy.v1\n",
    "authority=owned historical evidence of one managed SQLite observation and pathname/descriptor/residual bracketing checks in one consuming operation; not an attestation or main-file seal\n",
    "identity=SHA256 of the exact v1 domain, entry marker 01, u64be frame-count 6, six u64be name/value length-framed fields in the fixed order source-policy-fingerprint,managed-snapshot-policy,supplied-absolute-unix-path,main-identity,wal-residual,shm-residual, then ff and u64be transcript-count 1\n",
    "snapshot-policy=the explicit bytes mnestic.managed-snapshot-policy.v1; never an enum discriminant or native representation\n",
    "path=the caller-supplied absolute Unix OsStr bytes exactly; no URI, canonicalization, lossy UTF-8, or native-endian encoding\n",
    "construction=Unix only; non-Unix callers retain the API but construction fails closed instead of fabricating nullable identity\n",
    "freeze=the held O_NOFOLLOW read-only main descriptor and supplied named path repeatedly matched one frozen regular-file identity with link-count 1; WAL/SHM residual capture matched named/opened/final metadata and exact bounded bytes, while rollback-journal absence was required\n",
    "verification=main identity means all frozen descriptor+named fields matched; residual equality includes every DurableFileMetadata projection including readonly and modified SystemTime even though those redundant opaque projections are omitted from public evidence and fingerprint bytes\n",
    "completion=tracked transaction state is compared with live sqlite3_get_autocommit; either possible-active view triggers rollback; live autocommit is required afterward; sqlite3_close must succeed strictly and a close_v2 fallback is failure\n",
    "bracketing=main-before,residual,and-main-after source checks are all independently attempted and accumulated after close; operation, lifecycle, close, or source failure withholds the audit\n",
    "main=device+inode u64be; mode+uid+gid u32be; link-count+length u64be; mtime-seconds+mtime-nanoseconds+ctime-seconds+ctime-nanoseconds i64be; no descriptor, whole-main digest, readonly projection, or opaque SystemTime\n",
    "residual=absent 00; present 01 then length+device+inode u64be, mode u32be, link-count u64be, uid+gid u32be, rdev+blocks+block-size u64be, signed mtime/ctime seconds+nanos i64be, and exact SHA256; WAL and SHM remain separately named frames\n",
    "nonclaims=no authentication, freshness, anti-rollback, approval, mutation authority, managed ownership, same-UID rename-ABA protection, atomic SQLite-handle-to-held-fd binding, or main-file content digest; the path may change immediately after return\n",
);
// SHA-256 of the independently framed source-policy transcript. The literal is
// repinned only through the independent test oracle below.
const MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    196, 62, 40, 190, 206, 23, 138, 178, 241, 29, 93, 139, 70, 3, 194, 169, 218, 247, 88, 214, 192,
    121, 7, 229, 80, 220, 204, 131, 121, 109, 100, 154,
];

const MANAGED_RAW_COMMITMENT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-catalog.raw-commitment.v1\0";
const MANAGED_CANONICAL_COMMITMENT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-catalog.canonical-commitment.v1\0";
const MANAGED_COMMITMENT_ROW_MARKER_V1: [u8; 1] = [0x01];
const MANAGED_COMMITMENT_TERMINATOR_V1: [u8; 1] = [0xff];
#[cfg(test)]
const MANAGED_CATALOG_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-catalog.policy-fingerprint-transcript.v1\0";
const MANAGED_CATALOG_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-catalog-policy.v1\n",
    "readiness=ManagedSnapshotPolicy::V1 exact cozo table+primary-index schema\n",
    "snapshot=one BEGIN DEFERRED transaction held through consuming close\n",
    "query=forced sqlite_autoindex_cozo_1 id-zero literal range ORDER BY k LIMIT 29\n",
    "primary-index-visible-domain=26 canonical [String(name)] plus [Null] counter plus [Null,STORAGE_VERSION]\n",
    "counter=exact 8-byte big-endian u64; storage-version=exact 00\n",
    "catalog-codecs=positional-v0,positional-v1,struct-map-v1; projection=ManagedCatalogCensusV1\n",
    "projection-entry=source-encoding + recursive relation DTO\n",
    "projection-relation=name,id,columns(type+nullable+default-present),triggers,access,temp,normal/hnsw/fts children,lsh-count,description,tt-floor\n",
    "projection-hnsw=names,dimension,dtype,fields,distance,construction params,float bits,filter,flags\n",
    "projection-fts=names,extractor,tokenizer/filter names+argument-counts; expression bodies and tokenizer arguments discarded\n",
    "per-key=65536; per-relation=4194304; cumulative-raw=8388608; cumulative-canonical=8388608\n",
    "commitment=row-marker 01 || u64be(key-len) || key || u64be(value-len) || value; terminator ff || u64be(row-count)\n",
    "raw-domain=mnestic.managed-sqlite-catalog.raw-commitment.v1\\0\n",
    "canonical-domain=mnestic.managed-sqlite-catalog.canonical-commitment.v1\\0\n",
    "authority=conditional primary-index-visible syntax only; excludes completeness, F7 semantics, physical integrity, source binding, mutation\n",
);
// SHA-256 of the domain-separated, length-framed V1 policy transcript tested
// below. This is intentionally a literal oracle rather than a value derived by
// production code.
const MANAGED_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    14, 35, 102, 76, 164, 142, 235, 8, 177, 215, 104, 39, 220, 252, 129, 206, 10, 204, 252, 141,
    55, 67, 81, 189, 36, 231, 33, 188, 51, 138, 59, 21,
];
const MANAGED_PHYSICAL_TABLE_COMMITMENT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-physical.table-rowid-commitment.v1\0";
const MANAGED_PHYSICAL_INDEX_COMMITMENT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-physical.pk-index-commitment.v1\0";
const MANAGED_SQLITE_RUNTIME_IDENTITY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite.linked-runtime-identity.v1\0";
#[cfg(test)]
const MANAGED_PHYSICAL_POLICY_FINGERPRINT_DOMAIN_V1: &[u8] =
    b"mnestic.managed-sqlite-physical.policy-fingerprint-transcript.v1\0";
const MANAGED_PHYSICAL_POLICY_DESCRIPTION_V1: &str = concat!(
    "mnestic.managed-sqlite-physical-policy.v1\n",
    "authority=conditional linked-SQLite/VFS physical census only; excludes F7 semantics, source-bound attestation, mutation, and cross-architecture semantic portability\n",
    "runtime=evidence carries sqlite3_libversion_number, exact libversion/sourceid bytes, and their framed identity fingerprint; version>=3031000\n",
    "target-endian=little required because persisted row vectors use host-native payload bytes\n",
    "schema=ordinary rowid table stores rowid,k,v; sqlite_autoindex_cozo_1 stores k,rowid and never an independent v\n",
    "snapshot=BEGIN DEFERRED then immediate sqlite_master read; SQLITE_TXN_READ after every phase\n",
    "admission=file<=1099511627776; pages<=16777216; exact file=page-count*page-size\n",
    "memory=mmap-size=0; cache-size=-65536 KiB target; cell-size-check=1; cached catalog retains <=8388608 raw bytes plus bounded DTOs; census keeps one fixed 27-id admission map plus two 27-entry counter maps and one bounded key/value decode; SQLite/VFS allocations remain input-bounded, not allocator-hard\n",
    "work=one cumulative VDBE progress budget at interval 1024 and callback limit 134217728; one handler spans a fresh catalog+physical composite, while a pre-observed catalog is charged before installing the remaining-budget physical handler\n",
    "integrity=PRAGMA main.integrity_check(1) returns exactly one TEXT ok row then DONE\n",
    "table=main.cozo NOT INDEXED rowid order; key<=65536; positive-value<=1048576; id-zero-value<=4194304\n",
    "key-codec=exact memcmp tuple EOF plus canonical whole-key re-encoding; bound by stored-memcmp-key-policy-v1 sub-fingerprint\n",
    "relation-id=8-byte big-endian persisted domain 0..2^48; bound by stored-relation-id-policy-v1 sub-fingerprint\n",
    "positive-value=empty blob means zero value components; only a nonempty blob requires relation-prefix match plus exact bounded MessagePack array EOF and DataValue typed decode; no canonical reserialization, row arity/type, or F7 claim\n",
    "row-datavalue=serde wire ABI including native-endian vector payload; bound by stored-row-datavalue-codec-policy-v1 sub-fingerprint\n",
    "messagepack=exact-codec,row-profile,relation-catalog-profile sub-fingerprints are carried by evidence and composed here\n",
    "attribution=id zero plus exactly 26 unique nonzero top-level cached catalog ids; nested descriptors never whitelist storage\n",
    "point=reused forced sqlite_autoindex_cozo_1 lookup; bounded encoded key copied by SQLite with SQLITE_TRANSIENT; exact rowid,key,value; exactly one row; reset+clear every ordinary path and finalize on unwind\n",
    "covering=production same-connection EXPLAIN returns exactly one row with INTEGER id,parent,auxiliary and TEXT detail; id is nonnegative, parent is root zero, auxiliary is opaque optimizer telemetry, and detail is exactly SCAN main.cozo USING COVERING INDEX sqlite_autoindex_cozo_1; exact select only length(k),k,rowid in k order; SQLITE_STMTSTATUS_SORT=0\n",
    "equality=table-to-index inclusion plus equal finite total and per-id cardinality\n",
    "table-commitment=row-marker 01 || i64be(rowid) || u64be(key-len) || key || u64be(value-len) || value; terminator ff || u64be(row-count)\n",
    "index-commitment=row-marker 01 || u64be(key-len) || key || i64be(rowid); terminator ff || u64be(row-count)\n",
    "commitments=ordered telemetry only; never the exact equality decision\n",
);
// SHA-256 of the domain-separated, length-framed physical-policy transcript.
const MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    25, 34, 104, 183, 235, 148, 9, 17, 203, 100, 254, 116, 178, 137, 0, 67, 7, 124, 107, 45, 33,
    219, 179, 184, 162, 58, 21, 206, 31, 41, 44, 253,
];
#[cfg(test)]
thread_local! {
    static MANAGED_CATALOG_BLOB_POINTER_REQUESTS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_PHYSICAL_TEST_PANIC_PHASE: Cell<u8> = const { Cell::new(0) };
    static MANAGED_ASSERTION_PROGRESS_INSTALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_ASSERTION_TEST_FINALIZE_CODE: Cell<Option<i32>> = const { Cell::new(None) };
    static MANAGED_POINT_TEST_PANIC_AFTER_BIND: Cell<bool> = const { Cell::new(false) };
    static MANAGED_SOURCE_MAIN_VERIFY_CALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_SOURCE_RESIDUAL_VERIFY_CALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_SOURCE_RESIDUAL_TEST_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_BACKUP_HANDOFF_CLOSE_TEST_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_EXISTING_RUNTIME_INIT_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_EXISTING_RUNTIME_PREINIT_VERIFY_CALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_EXISTING_RUNTIME_FINAL_VERIFY_CALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_PROGRESS_INSTALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_PROGRESS_UNREGISTRATIONS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_RANGE_EXECUTIONS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_RANGE_ROWS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_RANGE_PANIC_AFTER_BIND: Cell<bool> = const { Cell::new(false) };
    static MANAGED_CATALOG_FENCE_RANGE_RESET_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_CATALOG_FENCE_RANGE_CLEAR_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CODE: Cell<Option<i32>> = const { Cell::new(None) };
    static MANAGED_CATALOG_FENCE_POINT_FINALIZE_CODE: Cell<Option<i32>> = const { Cell::new(None) };
    static MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_POINT_FINALIZE_CALLS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_POINT_EXECUTIONS: Cell<usize> = const { Cell::new(0) };
    static MANAGED_CATALOG_FENCE_POINT_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_RECORD_VISIT_PREPARE_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_RECORD_VISIT_RESET_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_RECORD_VISIT_CLEAR_FAILURE: Cell<bool> = const { Cell::new(false) };
    static MANAGED_RECORD_VISIT_FINALIZE_CODE: Cell<Option<i32>> = const { Cell::new(None) };
    static MANAGED_RECORD_VISIT_READ_FAILURE: Cell<bool> = const { Cell::new(false) };
}

const MANAGED_QUERY_ONLY_SQL: &[u8] = b"PRAGMA query_only=ON;\0";
#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_TEST_QUERY_ONLY_OFF_SQL: &[u8] = b"PRAGMA query_only=OFF;\0";
#[cfg(test)]
const MANAGED_TEST_ATTACH_SQL: &[u8] = b"ATTACH DATABASE ':memory:' AS extra;\0";
const MANAGED_QUERY_ONLY_CHECK: &[u8] = b"SELECT query_only = 1 FROM pragma_query_only;\0";
const MANAGED_MAIN_ONLY_CHECK: &[u8] = b"SELECT \
    (SELECT count(*) FROM pragma_database_list) = 1 AND \
    (SELECT count(*) FROM pragma_database_list WHERE seq = 0 AND name = 'main') = 1;\0";
const MANAGED_SCHEMA_OBJECT_CHECK: &[u8] = b"SELECT \
    (SELECT count(*) FROM main.sqlite_master) = 2 AND \
    (SELECT count(*) FROM main.sqlite_master \
        WHERE type = 'table' AND name = 'cozo' AND tbl_name = 'cozo' \
          AND rootpage > 1 AND sql = 'CREATE TABLE cozo\n        (\n            k BLOB primary key,\n            v BLOB\n        )') = 1 AND \
    (SELECT count(*) FROM main.sqlite_master \
        WHERE type = 'index' AND name = 'sqlite_autoindex_cozo_1' \
          AND tbl_name = 'cozo' AND rootpage > 1 AND sql IS NULL) = 1 AND \
    (SELECT count(*) FROM main.sqlite_master AS table_object \
        JOIN main.sqlite_master AS index_object \
          ON table_object.rootpage <> index_object.rootpage \
        WHERE table_object.type = 'table' AND table_object.name = 'cozo' \
          AND index_object.type = 'index' \
          AND index_object.name = 'sqlite_autoindex_cozo_1') = 1;\0";
const MANAGED_TABLE_XINFO_CHECK: &[u8] = b"SELECT \
    (SELECT count(*) FROM pragma_table_xinfo('cozo')) = 2 AND \
    (SELECT count(*) FROM pragma_table_xinfo('cozo') \
        WHERE cid = 0 AND name = 'k' AND type = 'BLOB' AND \"notnull\" = 0 \
          AND dflt_value IS NULL AND pk = 1 AND hidden = 0) = 1 AND \
    (SELECT count(*) FROM pragma_table_xinfo('cozo') \
        WHERE cid = 1 AND name = 'v' AND type = 'BLOB' AND \"notnull\" = 0 \
          AND dflt_value IS NULL AND pk = 0 AND hidden = 0) = 1;\0";
const MANAGED_INDEX_LIST_CHECK: &[u8] = b"SELECT \
    (SELECT count(*) FROM pragma_index_list('cozo')) = 1 AND \
    (SELECT count(*) FROM pragma_index_list('cozo') \
        WHERE seq = 0 AND name = 'sqlite_autoindex_cozo_1' AND \"unique\" = 1 \
          AND origin = 'pk' AND partial = 0) = 1;\0";
const MANAGED_INDEX_XINFO_CHECK: &[u8] = b"SELECT \
    (SELECT count(*) FROM pragma_index_xinfo('sqlite_autoindex_cozo_1')) = 2 AND \
    (SELECT count(*) FROM pragma_index_xinfo('sqlite_autoindex_cozo_1') \
        WHERE seqno = 0 AND cid = 0 AND name = 'k' AND [desc] = 0 \
          AND [coll] = 'BINARY' AND [key] = 1) = 1 AND \
    (SELECT count(*) FROM pragma_index_xinfo('sqlite_autoindex_cozo_1') \
        WHERE seqno = 1 AND cid = -1 AND name IS NULL AND [desc] = 0 \
          AND [coll] = 'BINARY' AND [key] = 0) = 1;\0";

#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_CATALOG_FIXTURE_BEGIN_SQL: &[u8] = b"BEGIN IMMEDIATE TRANSACTION;\0";
#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_CATALOG_FIXTURE_COMMIT_SQL: &[u8] = b"COMMIT;\0";
#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_CATALOG_FIXTURE_ROLLBACK_SQL: &[u8] = b"ROLLBACK;\0";
#[cfg(any(test, feature = "test-hooks"))]
const MANAGED_CATALOG_FIXTURE_UPDATE_SQL: &[u8] =
    b"UPDATE main.cozo INDEXED BY sqlite_autoindex_cozo_1 SET v = ?1 WHERE k = ?2;\0";

// The episode successor is a closed alternative, not a relaxed V1 cap. V1
// literals and commitments above remain unchanged. These fingerprints bind the
// frozen predecessor policy plus exactly the changed inventory/query/selector;
// independent known-answer tests below pin every dependency and field order.
const MANAGED_EPISODE_CATALOG_RELATION_COUNT_V1: usize = 31;
const MANAGED_EPISODE_CATALOG_QUERY_V1: &[u8] = b"SELECT length(k), length(v), k, v \
    FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 \
    WHERE k >= x'0000000000000000' AND k < x'0000000000000001' \
    ORDER BY k LIMIT 34;\0";
const MANAGED_SNAPSHOT_EPISODE_POLICY_V1_IDENTITY_BYTES: &[u8] =
    b"mnestic.managed-snapshot-policy.episode.v1";
const MANAGED_EPISODE_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    186, 178, 142, 119, 22, 62, 245, 202, 164, 220, 242, 40, 214, 9, 26, 117, 207, 87, 234, 59,
    159, 147, 154, 249, 142, 49, 106, 216, 113, 40, 229, 199,
];
const MANAGED_EPISODE_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    185, 223, 9, 190, 143, 225, 216, 254, 36, 221, 13, 64, 252, 149, 145, 133, 200, 133, 183, 203,
    76, 51, 107, 76, 157, 47, 183, 145, 17, 254, 33, 233,
];
const MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    229, 208, 243, 171, 239, 125, 35, 224, 71, 248, 242, 22, 240, 199, 105, 39, 58, 131, 33, 142,
    223, 148, 183, 95, 211, 223, 80, 63, 167, 209, 216, 86,
];
const MANAGED_EPISODE_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    151, 212, 248, 90, 157, 124, 92, 192, 22, 33, 67, 107, 133, 123, 176, 31, 199, 39, 86, 65, 138,
    105, 69, 254, 201, 121, 224, 226, 189, 174, 229, 209,
];

// Closed two-status successor; all predecessor literals remain frozen.
const MANAGED_SINGLE_GRAPH_CATALOG_RELATION_COUNT_V1: usize = 29;
const MANAGED_SINGLE_GRAPH_CATALOG_QUERY_V1: &[u8] = b"SELECT length(k), length(v), k, v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' ORDER BY k LIMIT 32;\0";
const MANAGED_SNAPSHOT_SINGLE_GRAPH_POLICY_V1_IDENTITY_BYTES: &[u8] =
    b"mnestic.managed-snapshot-policy.single-graph.v1";
const MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    100, 78, 17, 141, 64, 18, 200, 182, 203, 25, 68, 131, 253, 208, 138, 156, 160, 193, 65, 213,
    139, 12, 14, 94, 140, 28, 49, 166, 204, 255, 254, 189,
];
const MANAGED_SINGLE_GRAPH_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    13, 92, 233, 204, 58, 46, 48, 103, 24, 12, 92, 91, 234, 105, 138, 37, 127, 184, 150, 232, 89,
    188, 166, 236, 38, 167, 224, 203, 82, 83, 133, 51,
];
const MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    66, 226, 254, 49, 184, 240, 210, 118, 223, 68, 48, 5, 27, 62, 206, 139, 153, 187, 136, 14, 183,
    216, 50, 212, 189, 255, 24, 12, 220, 7, 78, 116,
];
const MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    51, 137, 166, 205, 77, 224, 53, 168, 115, 236, 152, 172, 225, 230, 254, 65, 212, 109, 179, 156,
    4, 123, 162, 84, 94, 232, 112, 28, 151, 170, 54, 180,
];

// Exact advisory successor; predecessor policy literals remain frozen.
const MANAGED_CONCERN_CATALOG_RELATION_COUNT_V1: usize = 31;
const MANAGED_CONCERN_CATALOG_QUERY_V1: &[u8] = b"SELECT length(k), length(v), k, v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' ORDER BY k LIMIT 34;\0";
const MANAGED_SNAPSHOT_CONCERN_POLICY_V1_IDENTITY_BYTES: &[u8] =
    b"mnestic.managed-snapshot-policy.concern.v1";
const MANAGED_CONCERN_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    178, 140, 67, 240, 222, 185, 243, 151, 75, 219, 66, 118, 193, 191, 35, 151, 12, 250, 96, 229,
    113, 26, 56, 145, 167, 250, 36, 39, 112, 183, 39, 76,
];
const MANAGED_CONCERN_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    195, 109, 242, 83, 9, 8, 92, 151, 201, 154, 106, 55, 6, 35, 29, 189, 97, 30, 29, 196, 125, 7,
    38, 207, 121, 167, 12, 24, 182, 96, 182, 92,
];
const MANAGED_CONCERN_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    37, 171, 178, 229, 11, 201, 75, 183, 35, 165, 111, 225, 51, 45, 102, 30, 156, 71, 71, 154, 175,
    216, 16, 112, 92, 175, 61, 241, 244, 200, 90, 218,
];
const MANAGED_CONCERN_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    246, 7, 157, 232, 94, 146, 19, 97, 238, 155, 152, 253, 56, 193, 218, 65, 83, 111, 213, 159,
    245, 40, 69, 143, 202, 75, 38, 65, 228, 30, 196, 100,
];

// Exact touchstone successor; all predecessor inventories and policy literals remain frozen.
const MANAGED_TOUCHSTONES_CATALOG_RELATION_COUNT_V1: usize = 34;
const MANAGED_TOUCHSTONES_CATALOG_QUERY_V1: &[u8] = b"SELECT length(k), length(v), k, v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' ORDER BY k LIMIT 37;\0";
const MANAGED_SNAPSHOT_TOUCHSTONES_POLICY_V1_IDENTITY_BYTES: &[u8] =
    b"mnestic.managed-snapshot-policy.touchstones.v1";
const MANAGED_TOUCHSTONES_CATALOG_POLICY_FINGERPRINT_V1: [u8; 32] = [
    160, 95, 232, 52, 189, 248, 54, 38, 187, 2, 152, 58, 50, 114, 91, 120,
    253, 98, 223, 102, 113, 186, 132, 12, 153, 132, 128, 4, 228, 220, 155, 56,
];
const MANAGED_TOUCHSTONES_PHYSICAL_POLICY_FINGERPRINT_V1: [u8; 32] = [
    80, 93, 29, 230, 128, 115, 116, 48, 51, 96, 63, 139, 97, 99, 176, 74,
    144, 181, 39, 13, 107, 175, 95, 125, 141, 85, 172, 219, 156, 69, 1, 90,
];
const MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    19, 76, 30, 182, 127, 106, 108, 19, 164, 202, 74, 228, 175, 91, 40, 94,
    239, 85, 129, 143, 143, 193, 189, 173, 34, 132, 243, 66, 197, 236, 157, 6,
];
const MANAGED_TOUCHSTONES_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1: [u8; 32] = [
    133, 104, 39, 217, 108, 57, 8, 236, 176, 214, 161, 47, 92, 38, 52, 170,
    13, 192, 34, 69, 166, 174, 28, 91, 147, 29, 160, 26, 72, 251, 79, 109,
];

/// Closed policy selector for the fixed managed-snapshot reader boundary.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ManagedSnapshotPolicy {
    /// Frozen 26-relation predecessor catalog.
    V1,
    /// Exact 31-relation episode catalog; all byte/work fuses remain V1.
    EpisodeV1,
    SingleGraphV1,
    ConcernV1,
    /// Exact touchstone-owner/target-index successor; all byte/work fuses remain V1.
    TouchstonesV1,
}

impl ManagedSnapshotPolicy {
    const fn relation_count(self) -> usize {
        match self {
            Self::V1 => MANAGED_CATALOG_RELATION_COUNT,
            Self::EpisodeV1 => MANAGED_EPISODE_CATALOG_RELATION_COUNT_V1,
            Self::SingleGraphV1 => MANAGED_SINGLE_GRAPH_CATALOG_RELATION_COUNT_V1,
            Self::ConcernV1 => MANAGED_CONCERN_CATALOG_RELATION_COUNT_V1,
            Self::TouchstonesV1 => MANAGED_TOUCHSTONES_CATALOG_RELATION_COUNT_V1,
        }
    }
    const fn catalog_row_count(self) -> usize {
        self.relation_count() + 2
    }
    const fn catalog_query(self) -> &'static [u8] {
        match self {
            Self::V1 => MANAGED_CATALOG_QUERY,
            Self::EpisodeV1 => MANAGED_EPISODE_CATALOG_QUERY_V1,
            Self::SingleGraphV1 => MANAGED_SINGLE_GRAPH_CATALOG_QUERY_V1,
            Self::ConcernV1 => MANAGED_CONCERN_CATALOG_QUERY_V1,
            Self::TouchstonesV1 => MANAGED_TOUCHSTONES_CATALOG_QUERY_V1,
        }
    }
    const fn catalog_fingerprint(self) -> &'static [u8; 32] {
        match self {
            Self::V1 => &MANAGED_CATALOG_POLICY_FINGERPRINT_V1,
            Self::EpisodeV1 => &MANAGED_EPISODE_CATALOG_POLICY_FINGERPRINT_V1,
            Self::SingleGraphV1 => &MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1,
            Self::ConcernV1 => &MANAGED_CONCERN_CATALOG_POLICY_FINGERPRINT_V1,
            Self::TouchstonesV1 => &MANAGED_TOUCHSTONES_CATALOG_POLICY_FINGERPRINT_V1,
        }
    }
    const fn physical_fingerprint(self) -> &'static [u8; 32] {
        match self {
            Self::V1 => &MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1,
            Self::EpisodeV1 => &MANAGED_EPISODE_PHYSICAL_POLICY_FINGERPRINT_V1,
            Self::SingleGraphV1 => &MANAGED_SINGLE_GRAPH_PHYSICAL_POLICY_FINGERPRINT_V1,
            Self::ConcernV1 => &MANAGED_CONCERN_PHYSICAL_POLICY_FINGERPRINT_V1,
            Self::TouchstonesV1 => &MANAGED_TOUCHSTONES_PHYSICAL_POLICY_FINGERPRINT_V1,
        }
    }
    const fn source_fingerprint(self) -> &'static [u8; 32] {
        match self {
            Self::V1 => &MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            Self::EpisodeV1 => &MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            Self::SingleGraphV1 => &MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            Self::ConcernV1 => &MANAGED_CONCERN_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            Self::TouchstonesV1 => &MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
        }
    }
    const fn identity_bytes(self) -> &'static [u8] {
        match self {
            Self::V1 => MANAGED_SNAPSHOT_POLICY_V1_IDENTITY_BYTES,
            Self::EpisodeV1 => MANAGED_SNAPSHOT_EPISODE_POLICY_V1_IDENTITY_BYTES,
            Self::SingleGraphV1 => MANAGED_SNAPSHOT_SINGLE_GRAPH_POLICY_V1_IDENTITY_BYTES,
            Self::ConcernV1 => MANAGED_SNAPSHOT_CONCERN_POLICY_V1_IDENTITY_BYTES,
            Self::TouchstonesV1 => MANAGED_SNAPSHOT_TOUCHSTONES_POLICY_V1_IDENTITY_BYTES,
        }
    }
}

/// A fixed-purpose, immutable SQLite reader that has passed the V1 readiness gate.
///
/// It deliberately exposes neither its SQLite handle nor caller-supplied SQL. The
/// marker makes the type non-cloneable, non-sendable, and non-shareable even if a
/// future SQLite binding changes its raw-pointer auto traits.
///
/// Readiness proves only this module's closed schema and connection policy. It
/// does not attest b-tree integrity, row semantics, or independent descendants
/// below the distinct table/index roots; those belong to the later bounded raw
/// census. The boundary trusts the linked SQLite implementation and assumes the
/// embedding process has not installed hostile process-global SQLite extensions.
#[must_use = "close the managed reader explicitly to verify its source identity"]
pub struct ManagedSqliteSnapshotReader {
    path: PathBuf,
    connection: Option<SnapshotConnection>,
    main_file: FrozenSourceMainFile,
    retained_backup_destination: Option<RetainedBackupDestinationIdentity>,
    residuals: FrozenSourceResiduals,
    policy: ManagedSnapshotPolicy,
    state: ManagedReaderState,
    transaction_active: bool,
    catalog: Option<ManagedSqlitePrimaryIndexCatalogV1>,
    census_progress_callbacks_used: u64,
    physical_census: Option<ManagedSqlitePhysicalCensusV1>,
    _thread_bound: PhantomData<Rc<()>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ManagedReaderState {
    Ready,
    Inspecting,
    Observed,
    Auditing,
    Audited,
    Asserting,
    Visiting,
    Poisoned,
}

/// A bounded syntactic observation of primary-index-visible id-zero rows.
///
/// The observation remains borrowed from its live reader. It is not source
/// binding, catalog completeness, F7 semantic admission, physical-integrity
/// evidence, or mutation authority. A corrupt primary index could omit a row;
/// a later consuming audit must match an independent table-b-tree census and
/// integrity pass before treating this conditional view as complete.
///
/// The payload deliberately has no `Clone`, `Debug`, `Default`, or serde
/// implementation. For example, cloning it is a compile-time error:
///
/// ```compile_fail
/// use cozo::ManagedSqlitePrimaryIndexCatalogV1;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<ManagedSqlitePrimaryIndexCatalogV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqlitePrimaryIndexCatalogV1;
/// fn requires_debug<T: std::fmt::Debug>() {}
/// requires_debug::<ManagedSqlitePrimaryIndexCatalogV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqlitePrimaryIndexCatalogV1;
/// fn requires_default<T: Default>() {}
/// requires_default::<ManagedSqlitePrimaryIndexCatalogV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqlitePrimaryIndexCatalogV1;
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<ManagedSqlitePrimaryIndexCatalogV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqlitePrimaryIndexCatalogV1;
/// fn requires_deserialize<T: for<'de> serde::Deserialize<'de>>() {}
/// requires_deserialize::<ManagedSqlitePrimaryIndexCatalogV1>();
/// ```
pub struct ManagedSqlitePrimaryIndexCatalogV1 {
    snapshot_policy: ManagedSnapshotPolicy,
    catalog: ManagedCatalogCensusV1,
    relation_counter: u64,
    storage_version: u8,
    raw_commitment: [u8; 32],
    canonical_commitment: [u8; 32],
    policy_fingerprint: [u8; 32],
    raw_rows: Box<[ManagedCatalogRawRow]>,
    _thread_bound: PhantomData<Rc<()>>,
}

struct ManagedCatalogRawRow {
    key: Box<[u8]>,
    value: Box<[u8]>,
}

impl ManagedSqlitePrimaryIndexCatalogV1 {
    /// Sanitized catalog entries in raw primary-key order.
    pub const fn catalog(&self) -> &ManagedCatalogCensusV1 {
        &self.catalog
    }

    /// Exact big-endian relation counter stored under `[Null]`.
    pub const fn relation_counter(&self) -> u64 {
        self.relation_counter
    }

    /// Exact one-byte storage version stored under `[Null, "STORAGE_VERSION"]`.
    pub const fn storage_version(&self) -> u8 {
        self.storage_version
    }

    /// Ordered, length-framed SHA-256 commitment to the exact raw key/value rows.
    pub const fn raw_commitment(&self) -> &[u8; 32] {
        &self.raw_commitment
    }

    /// Ordered commitment after relation values are normalized to struct-map v1.
    pub const fn canonical_commitment(&self) -> &[u8; 32] {
        &self.canonical_commitment
    }

    /// Literal fingerprint of the fixed V1 codec and projection policy.
    pub const fn policy_fingerprint(&self) -> &[u8; 32] {
        &self.policy_fingerprint
    }

    fn raw_row(&self, key: &[u8]) -> Option<&ManagedCatalogRawRow> {
        self.raw_row_with_index(key).map(|(_, row)| row)
    }

    fn raw_row_with_index(&self, key: &[u8]) -> Option<(usize, &ManagedCatalogRawRow)> {
        self.raw_rows
            .binary_search_by(|row| row.key.as_ref().cmp(key))
            .ok()
            .map(|index| (index, &self.raw_rows[index]))
    }
}

/// One admitted relation id and its exact physical row count.
///
/// The enclosing census rejects unless the table and covering-index counts for
/// every id agree. Id zero is included and therefore has the exact count 28.
pub struct ManagedSqliteRelationRowCountV1 {
    relation_id: u64,
    row_count: u64,
}

impl ManagedSqliteRelationRowCountV1 {
    pub const fn relation_id(&self) -> u64 {
        self.relation_id
    }

    pub const fn row_count(&self) -> u64 {
        self.row_count
    }
}

/// Bounded physical census made under one live managed-reader transaction.
///
/// This is conditional evidence from the linked SQLite implementation and VFS,
/// not F7 semantic admission, source-bound attestation, or mutation authority.
/// The exact table/index equality decision is the table-to-forced-index point
/// inclusion proof plus equal finite cardinality; the two ordered SHA-256
/// commitments are only bounded operation-binding telemetry.
///
/// The fixed V1 boundary admits a main file of at most 1 TiB and at most
/// 2^24 pages, disables mmap, requests a 64 MiB private page-cache target, and
/// enables cell-size checks. Fresh catalog inspection, SQLite's full integrity
/// pass, and every census statement share at most 2^27 progress callbacks at
/// 1,024 VDBE operations per callback. This bounds induced SQL work (with one callback interval of
/// granularity), but the cache setting is a target rather than a hard allocator
/// ceiling: other SQLite/VFS allocations remain bounded by the admitted input.
/// The explicit census is O(N log N) SQLite work for N rows because each table
/// row performs one forced unique-primary-index lookup. Incremental Rust census
/// state is one fixed 27-id admission map plus two 27-entry per-id counter maps,
/// together with one bounded key decode/re-encoding and one bounded nonempty
/// positive-row value decode (64 KiB and 1 MiB input caps respectively); an
/// empty positive-row value denotes zero value components,
/// while id-zero comparison may borrow a 4 MiB cell. This is in addition to the
/// already-bounded cached catalog, which retains at most 8 MiB of raw rows plus
/// its decoder-bounded DTOs. Nested descriptors never expand the physical
/// admission set. V1 fails closed off little-endian targets because persisted
/// row vectors contain host-native payload bytes; this evidence makes no
/// cross-architecture semantic-portability claim.
pub struct ManagedSqlitePhysicalCensusV1 {
    main_file_bytes: u64,
    main_page_count: u64,
    table_row_count: u64,
    index_row_count: u64,
    relation_row_counts: Box<[ManagedSqliteRelationRowCountV1]>,
    table_ordered_commitment: [u8; 32],
    index_ordered_commitment: [u8; 32],
    progress_callbacks_used: u64,
    linked_sqlite_version_number: i32,
    linked_sqlite_version: Box<[u8]>,
    linked_sqlite_source_id: Box<[u8]>,
    linked_sqlite_runtime_identity_fingerprint: [u8; 32],
    stored_relation_id_policy_fingerprint: [u8; 32],
    stored_memcmp_key_policy_fingerprint: [u8; 32],
    stored_msgpack_exact_codec_policy_fingerprint: [u8; 32],
    stored_msgpack_row_policy_fingerprint: [u8; 32],
    stored_msgpack_relation_catalog_policy_fingerprint: [u8; 32],
    stored_row_datavalue_codec_policy_fingerprint: [u8; 32],
    policy_fingerprint: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqlitePhysicalCensusV1 {
    pub const fn main_file_bytes(&self) -> u64 {
        self.main_file_bytes
    }

    pub const fn main_page_count(&self) -> u64 {
        self.main_page_count
    }

    pub const fn table_row_count(&self) -> u64 {
        self.table_row_count
    }

    pub const fn index_row_count(&self) -> u64 {
        self.index_row_count
    }

    pub fn relation_row_counts(&self) -> &[ManagedSqliteRelationRowCountV1] {
        &self.relation_row_counts
    }

    /// Rowid-ordered commitment over the table's exact `(rowid, k, v)` cells.
    pub const fn table_ordered_commitment(&self) -> &[u8; 32] {
        &self.table_ordered_commitment
    }

    /// Key-ordered commitment over the PK index's exact `(k, rowid)` cells.
    ///
    /// No value bytes reside independently in this ordinary rowid-table index.
    pub const fn index_ordered_commitment(&self) -> &[u8; 32] {
        &self.index_ordered_commitment
    }

    pub const fn progress_callbacks_used(&self) -> u64 {
        self.progress_callbacks_used
    }

    /// Numeric identity returned by the linked `sqlite3_libversion_number()`.
    pub const fn linked_sqlite_version_number(&self) -> i32 {
        self.linked_sqlite_version_number
    }

    /// Exact, bounded bytes returned by the linked `sqlite3_libversion()`.
    pub fn linked_sqlite_version(&self) -> &[u8] {
        &self.linked_sqlite_version
    }

    /// Exact, bounded bytes returned by the linked `sqlite3_sourceid()`.
    pub fn linked_sqlite_source_id(&self) -> &[u8] {
        &self.linked_sqlite_source_id
    }

    /// Framed binding of the three linked-runtime identity fields above.
    pub const fn linked_sqlite_runtime_identity_fingerprint(&self) -> &[u8; 32] {
        &self.linked_sqlite_runtime_identity_fingerprint
    }

    pub const fn stored_memcmp_key_policy_fingerprint(&self) -> &[u8; 32] {
        &self.stored_memcmp_key_policy_fingerprint
    }

    pub const fn stored_relation_id_policy_fingerprint(&self) -> &[u8; 32] {
        &self.stored_relation_id_policy_fingerprint
    }

    pub const fn stored_msgpack_exact_codec_policy_fingerprint(&self) -> &[u8; 32] {
        &self.stored_msgpack_exact_codec_policy_fingerprint
    }

    pub const fn stored_msgpack_row_policy_fingerprint(&self) -> &[u8; 32] {
        &self.stored_msgpack_row_policy_fingerprint
    }

    pub const fn stored_msgpack_relation_catalog_policy_fingerprint(&self) -> &[u8; 32] {
        &self.stored_msgpack_relation_catalog_policy_fingerprint
    }

    pub const fn stored_row_datavalue_codec_policy_fingerprint(&self) -> &[u8; 32] {
        &self.stored_row_datavalue_codec_policy_fingerprint
    }

    pub const fn policy_fingerprint(&self) -> &[u8; 32] {
        &self.policy_fingerprint
    }
}

/// One nontransferable borrow of the cached catalog and physical census.
///
/// Request this composite directly when a caller needs both halves: it avoids
/// retaining the catalog's `&mut`-derived borrow across a second reader call.
pub struct ManagedSqliteSnapshotCensusV1<'reader> {
    catalog: &'reader ManagedSqlitePrimaryIndexCatalogV1,
    physical: &'reader ManagedSqlitePhysicalCensusV1,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<'reader> ManagedSqliteSnapshotCensusV1<'reader> {
    pub const fn catalog(&self) -> &'reader ManagedSqlitePrimaryIndexCatalogV1 {
        self.catalog
    }

    pub const fn physical(&self) -> &'reader ManagedSqlitePhysicalCensusV1 {
        self.physical
    }
}

enum ManagedRelationAssertionOutcomeKindV1 {
    ExactRowCount {
        relation_id: u64,
        expected: u64,
    },
    StringPair {
        relation_id: u64,
        key: &'static str,
        value: &'static str,
    },
    StringPairOneOf {
        outcome_tag: &'static str,
        relation_id: u64,
        key: &'static str,
        alternatives: [&'static str; 2],
        selected_index: u8,
    },
}

/// One ordered, closed-vocabulary assertion outcome from a consumed reader.
///
/// The outcome intentionally exposes only exact matchers. In particular, a
/// one-of selection is not available unless the caller supplies the original
/// tag, relation id, key, and both alternatives in their original order.
pub struct ManagedSqliteRelationAssertionOutcomeV1 {
    kind: ManagedRelationAssertionOutcomeKindV1,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteRelationAssertionOutcomeV1 {
    pub fn matches_exact_row_count(&self, relation_id: u64, expected: u64) -> bool {
        matches!(
            &self.kind,
            ManagedRelationAssertionOutcomeKindV1::ExactRowCount {
                relation_id: actual_relation_id,
                expected: actual_expected,
            } if *actual_relation_id == relation_id && *actual_expected == expected
        )
    }

    pub fn matches_string_pair(&self, relation_id: u64, key: &str, value: &str) -> bool {
        matches!(
            &self.kind,
            ManagedRelationAssertionOutcomeKindV1::StringPair {
                relation_id: actual_relation_id,
                key: actual_key,
                value: actual_value,
            } if *actual_relation_id == relation_id && *actual_key == key && *actual_value == value
        )
    }

    pub fn selected_index_if_string_pair_one_of(
        &self,
        outcome_tag: &str,
        relation_id: u64,
        key: &str,
        alternatives: [&str; 2],
    ) -> Option<u8> {
        match &self.kind {
            ManagedRelationAssertionOutcomeKindV1::StringPairOneOf {
                outcome_tag: actual_tag,
                relation_id: actual_relation_id,
                key: actual_key,
                alternatives: actual_alternatives,
                selected_index,
            } if *actual_tag == outcome_tag
                && *actual_relation_id == relation_id
                && *actual_key == key
                && actual_alternatives[0] == alternatives[0]
                && actual_alternatives[1] == alternatives[1] =>
            {
                Some(*selected_index)
            }
            _ => None,
        }
    }
}

/// Domain-separated commitment to the complete ordered assertion transcript.
pub struct ManagedSqliteAssertionTranscriptV1 {
    commitment: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteAssertionTranscriptV1 {
    pub const fn commitment(&self) -> &[u8; 32] {
        &self.commitment
    }
}

/// Bounded typed assertion evidence retained by a successful consuming close.
pub struct ManagedSqliteRelationAssertionEvidenceV1 {
    outcomes: Box<[ManagedSqliteRelationAssertionOutcomeV1]>,
    assertion_count: u64,
    point_read_count: u64,
    progress_callbacks_used: u64,
    transcript: ManagedSqliteAssertionTranscriptV1,
    policy_fingerprint: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteRelationAssertionEvidenceV1 {
    pub fn outcomes(&self) -> &[ManagedSqliteRelationAssertionOutcomeV1] {
        &self.outcomes
    }

    pub const fn assertion_count(&self) -> u64 {
        self.assertion_count
    }

    pub const fn point_read_count(&self) -> u64 {
        self.point_read_count
    }

    pub const fn progress_callbacks_used(&self) -> u64 {
        self.progress_callbacks_used
    }

    pub const fn transcript(&self) -> &ManagedSqliteAssertionTranscriptV1 {
        &self.transcript
    }

    pub const fn transcript_commitment(&self) -> &[u8; 32] {
        self.transcript.commitment()
    }

    pub const fn policy_fingerprint(&self) -> &[u8; 32] {
        &self.policy_fingerprint
    }
}

/// One bounded logical-record visitation event.
///
/// Ordinals are the caller's fixed positions `1..=16`; they are deliberately
/// not persisted relation ids. Records expose only fully decoded key and value
/// components for the duration of the callback. They never expose relation
/// names, SQLite rowids, SQL, raw cells, statements, or cursors.
pub enum ManagedSqliteRecordVisitEventV1<'row> {
    BeginRelation {
        ordinal: u16,
        expected_rows: u64,
    },
    Record {
        ordinal: u16,
        key: &'row [DataValue],
        value: &'row [DataValue],
    },
    EndRelation {
        ordinal: u16,
        rows: u64,
    },
}

/// Opaque evidence for one complete ordered sixteen-relation record visit.
///
/// The transcript commits the exact raw key/value cells in visitation order as
/// well as the ordered selected ids, physical counts, total row count, and
/// cumulative census/visit progress callbacks. None of these values authenticates
/// itself or gives the decoded callback sink authority. This type intentionally
/// implements neither `Clone`, `Debug`, serde, `Send`, nor `Sync`.
pub struct ManagedSqliteRecordVisitEvidenceV1 {
    relation_ids: [u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1],
    relation_row_counts: [u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1],
    total_row_count: u64,
    progress_callbacks_used: u64,
    transcript_commitment: [u8; 32],
    policy_fingerprint: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteRecordVisitEvidenceV1 {
    pub const fn relation_ids(&self) -> &[u64; 16] {
        &self.relation_ids
    }

    pub const fn relation_row_counts(&self) -> &[u64; 16] {
        &self.relation_row_counts
    }

    pub const fn total_row_count(&self) -> u64 {
        self.total_row_count
    }

    /// Cumulative callbacks spent by catalog inspection, physical census, and
    /// this record scan under the one shared census budget.
    pub const fn progress_callbacks_used(&self) -> u64 {
        self.progress_callbacks_used
    }

    pub const fn transcript_commitment(&self) -> &[u8; 32] {
        &self.transcript_commitment
    }

    pub const fn policy_fingerprint(&self) -> &[u8; 32] {
        &self.policy_fingerprint
    }
}

/// Frozen Unix main-file identity retained after the held descriptor is closed.
///
/// This is historical pathname/descriptor identity evidence, not a content
/// digest, authentication claim, freshness proof, or anti-rollback token.
pub struct ManagedSqliteClosedMainFileIdentityV1 {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    link_count: u64,
    length: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
    ctime_seconds: i64,
    ctime_nanoseconds: i64,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedMainFileIdentityV1 {
    pub const fn device(&self) -> u64 {
        self.device
    }

    pub const fn inode(&self) -> u64 {
        self.inode
    }

    pub const fn mode(&self) -> u32 {
        self.mode
    }

    pub const fn uid(&self) -> u32 {
        self.uid
    }

    pub const fn gid(&self) -> u32 {
        self.gid
    }

    pub const fn link_count(&self) -> u64 {
        self.link_count
    }

    pub const fn length(&self) -> u64 {
        self.length
    }

    pub const fn mtime_seconds(&self) -> i64 {
        self.mtime_seconds
    }

    pub const fn mtime_nanoseconds(&self) -> i64 {
        self.mtime_nanoseconds
    }

    pub const fn ctime_seconds(&self) -> i64 {
        self.ctime_seconds
    }

    pub const fn ctime_nanoseconds(&self) -> i64 {
        self.ctime_nanoseconds
    }
}

/// Frozen durable metadata for one present WAL or SHM residual.
pub struct ManagedSqliteClosedResidualMetadataV1 {
    length: u64,
    device: u64,
    inode: u64,
    mode: u32,
    link_count: u64,
    uid: u32,
    gid: u32,
    rdev: u64,
    blocks: u64,
    block_size: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
    ctime_seconds: i64,
    ctime_nanoseconds: i64,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedResidualMetadataV1 {
    pub const fn length(&self) -> u64 {
        self.length
    }

    pub const fn device(&self) -> u64 {
        self.device
    }

    pub const fn inode(&self) -> u64 {
        self.inode
    }

    pub const fn mode(&self) -> u32 {
        self.mode
    }

    pub const fn link_count(&self) -> u64 {
        self.link_count
    }

    pub const fn uid(&self) -> u32 {
        self.uid
    }

    pub const fn gid(&self) -> u32 {
        self.gid
    }

    pub const fn rdev(&self) -> u64 {
        self.rdev
    }

    pub const fn blocks(&self) -> u64 {
        self.blocks
    }

    pub const fn block_size(&self) -> u64 {
        self.block_size
    }

    pub const fn mtime_seconds(&self) -> i64 {
        self.mtime_seconds
    }

    pub const fn mtime_nanoseconds(&self) -> i64 {
        self.mtime_nanoseconds
    }

    pub const fn ctime_seconds(&self) -> i64 {
        self.ctime_seconds
    }

    pub const fn ctime_nanoseconds(&self) -> i64 {
        self.ctime_nanoseconds
    }
}

/// Frozen absence or exact metadata/content fingerprint for one residual role.
pub struct ManagedSqliteClosedResidualV1 {
    present: bool,
    metadata: Option<ManagedSqliteClosedResidualMetadataV1>,
    sha256: Option<[u8; 32]>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedResidualV1 {
    /// Presence is exposed independently so absent and present-empty remain distinct.
    pub const fn is_present(&self) -> bool {
        self.present
    }

    pub const fn metadata(&self) -> Option<&ManagedSqliteClosedResidualMetadataV1> {
        self.metadata.as_ref()
    }

    pub const fn sha256(&self) -> Option<&[u8; 32]> {
        self.sha256.as_ref()
    }
}

/// Descriptor-free historical source evidence from one consuming close.
///
/// The identity fingerprint binds exact frozen Unix identity, supplied path
/// spelling, residual state, snapshot policy, and the source-policy fingerprint.
/// It deliberately does not bind the main file's contents and does not prove
/// authentication, freshness, ownership, or anti-rollback. Path checks do not
/// defeat same-UID rename ABA, and there is no atomic binding between SQLite's
/// separately opened handle and the held main-file descriptor.
pub struct ManagedSqliteClosedSourceEvidenceV1 {
    supplied_path: PathBuf,
    managed_snapshot_policy: ManagedSnapshotPolicy,
    main_identity: ManagedSqliteClosedMainFileIdentityV1,
    wal_residual: ManagedSqliteClosedResidualV1,
    shm_residual: ManagedSqliteClosedResidualV1,
    policy_fingerprint: [u8; 32],
    identity_fingerprint: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedSourceEvidenceV1 {
    pub fn supplied_path(&self) -> &Path {
        &self.supplied_path
    }

    pub const fn managed_snapshot_policy(&self) -> ManagedSnapshotPolicy {
        self.managed_snapshot_policy
    }

    pub const fn main_identity(&self) -> &ManagedSqliteClosedMainFileIdentityV1 {
        &self.main_identity
    }

    pub const fn wal_residual(&self) -> &ManagedSqliteClosedResidualV1 {
        &self.wal_residual
    }

    pub const fn shm_residual(&self) -> &ManagedSqliteClosedResidualV1 {
        &self.shm_residual
    }

    pub const fn policy_fingerprint(&self) -> &[u8; 32] {
        &self.policy_fingerprint
    }

    pub const fn identity_fingerprint(&self) -> &[u8; 32] {
        &self.identity_fingerprint
    }
}

/// Owned historical evidence from one fully consumed managed SQLite reader.
///
/// The catalog, physical census, and typed assertions were observed under one
/// pinned READ transaction. The consuming operation then performed rollback,
/// strict SQLite close, and post-close bracketed source checks. This is not attestation,
/// authentication, freshness, ownership, mutation authority, or a main-file
/// content seal. It does not defeat same-UID rename ABA or atomically bind
/// SQLite's separately opened handle to the held main-file descriptor.
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedAuditV1;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<ManagedSqliteClosedAuditV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedAuditV1;
/// fn requires_send<T: Send>() {}
/// requires_send::<ManagedSqliteClosedAuditV1>();
/// ```
#[must_use = "retain or explicitly inspect the completed closed audit"]
pub struct ManagedSqliteClosedAuditV1 {
    catalog: ManagedSqlitePrimaryIndexCatalogV1,
    physical: ManagedSqlitePhysicalCensusV1,
    assertions: ManagedSqliteRelationAssertionEvidenceV1,
    source: ManagedSqliteClosedSourceEvidenceV1,
    policy: ManagedSnapshotPolicy,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedAuditV1 {
    pub const fn catalog(&self) -> &ManagedSqlitePrimaryIndexCatalogV1 {
        &self.catalog
    }

    pub const fn physical(&self) -> &ManagedSqlitePhysicalCensusV1 {
        &self.physical
    }

    pub const fn assertions(&self) -> &ManagedSqliteRelationAssertionEvidenceV1 {
        &self.assertions
    }

    pub const fn source(&self) -> &ManagedSqliteClosedSourceEvidenceV1 {
        &self.source
    }

    pub const fn policy(&self) -> ManagedSnapshotPolicy {
        self.policy
    }
}

/// A closed managed audit, opaque record-visit evidence, and its tentative sink.
///
/// Downstream code must retain this wrapper intact until it has independently
/// rederived the ordered relation roles and verified the audit, assertion, visit,
/// codec, and source evidence. The sink is deliberately unavailable by borrowed
/// getter: consuming [`Self::into_parts`] is the explicit point at which downstream
/// code accepts responsibility for keeping the three parts associated.
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedRecordVisitV1;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<ManagedSqliteClosedRecordVisitV1<()>>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedRecordVisitV1;
/// fn requires_send<T: Send>() {}
/// requires_send::<ManagedSqliteClosedRecordVisitV1<()>>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedRecordVisitV1;
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<ManagedSqliteClosedRecordVisitV1<()>>();
/// ```
#[must_use = "retain this wrapper through independent downstream verification"]
pub struct ManagedSqliteClosedRecordVisitV1<S> {
    audit: ManagedSqliteClosedAuditV1,
    visit_evidence: ManagedSqliteRecordVisitEvidenceV1,
    sink: S,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<S> ManagedSqliteClosedRecordVisitV1<S> {
    pub const fn audit(&self) -> &ManagedSqliteClosedAuditV1 {
        &self.audit
    }

    pub const fn visit_evidence(&self) -> &ManagedSqliteRecordVisitEvidenceV1 {
        &self.visit_evidence
    }

    pub fn into_parts(
        self,
    ) -> (
        ManagedSqliteClosedAuditV1,
        ManagedSqliteRecordVisitEvidenceV1,
        S,
    ) {
        (self.audit, self.visit_evidence, self.sink)
    }
}

/// Explicit selector for the catalog-only routine-admission fence.
///
/// This is intentionally not [`ManagedSnapshotPolicy`]: the selector names a
/// different authority boundary and has its own pinned policy identity.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ManagedCatalogFencePolicy {
    V1,
    EpisodeV1,
    SingleGraphV1,
    ConcernV1,
    /// Exact touchstone-owner/target-index successor; all byte/work fuses remain V1.
    TouchstonesV1,
}

impl ManagedCatalogFencePolicy {
    const fn snapshot_policy(self) -> ManagedSnapshotPolicy {
        match self {
            Self::V1 => ManagedSnapshotPolicy::V1,
            Self::EpisodeV1 => ManagedSnapshotPolicy::EpisodeV1,
            Self::SingleGraphV1 => ManagedSnapshotPolicy::SingleGraphV1,
            Self::ConcernV1 => ManagedSnapshotPolicy::ConcernV1,
            Self::TouchstonesV1 => ManagedSnapshotPolicy::TouchstonesV1,
        }
    }
    const fn fingerprint(self) -> &'static [u8; 32] {
        match self {
            Self::V1 => &MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
            Self::EpisodeV1 => &MANAGED_EPISODE_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
            Self::SingleGraphV1 => &MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
            Self::ConcernV1 => &MANAGED_CONCERN_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
            Self::TouchstonesV1 => &MANAGED_TOUCHSTONES_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
        }
    }
}

enum ManagedCatalogFenceAssertionOutcomeKindV1 {
    ExactRowCount {
        relation_id: u64,
        expected: u64,
    },
    StringPair {
        relation_id: u64,
        key: &'static str,
        value: &'static str,
    },
    StringPairOneOf {
        outcome_tag: &'static str,
        relation_id: u64,
        key: &'static str,
        alternatives: [&'static str; 2],
        selected_index: u8,
    },
}

/// One ordered conditional observation from the catalog-only fence.
///
/// It observes only forced-primary-index point/range results.  In particular,
/// it is not evidence that the table b-tree is complete or physically sound.
pub struct ManagedSqliteCatalogFenceAssertionOutcomeV1 {
    kind: ManagedCatalogFenceAssertionOutcomeKindV1,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteCatalogFenceAssertionOutcomeV1 {
    pub fn matches_exact_row_count(&self, relation_id: u64, expected: u64) -> bool {
        matches!(&self.kind, ManagedCatalogFenceAssertionOutcomeKindV1::ExactRowCount { relation_id: actual, expected: count } if *actual == relation_id && *count == expected)
    }

    pub fn matches_string_pair(&self, relation_id: u64, key: &str, value: &str) -> bool {
        matches!(&self.kind, ManagedCatalogFenceAssertionOutcomeKindV1::StringPair { relation_id: actual, key: actual_key, value: actual_value } if *actual == relation_id && *actual_key == key && *actual_value == value)
    }

    pub fn selected_index_if_string_pair_one_of(
        &self,
        outcome_tag: &str,
        relation_id: u64,
        key: &str,
        alternatives: [&str; 2],
    ) -> Option<u8> {
        match &self.kind {
            ManagedCatalogFenceAssertionOutcomeKindV1::StringPairOneOf {
                outcome_tag: actual_tag,
                relation_id: actual_id,
                key: actual_key,
                alternatives: actual_alternatives,
                selected_index,
            } if *actual_tag == outcome_tag
                && *actual_id == relation_id
                && *actual_key == key
                && actual_alternatives[0] == alternatives[0]
                && actual_alternatives[1] == alternatives[1] =>
            {
                Some(*selected_index)
            }
            _ => None,
        }
    }
}

/// Domain-separated commitment for the light fence's separate transcript.
pub struct ManagedSqliteCatalogFenceAssertionTranscriptV1 {
    commitment: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteCatalogFenceAssertionTranscriptV1 {
    pub const fn commitment(&self) -> &[u8; 32] {
        &self.commitment
    }
}

/// Bounded light assertion evidence.  This has no physical-census component.
pub struct ManagedSqliteCatalogFenceAssertionEvidenceV1 {
    outcomes: Box<[ManagedSqliteCatalogFenceAssertionOutcomeV1]>,
    assertion_count: u64,
    range_probe_count: u64,
    point_read_count: u64,
    progress_callbacks_used: u64,
    transcript: ManagedSqliteCatalogFenceAssertionTranscriptV1,
    policy_fingerprint: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteCatalogFenceAssertionEvidenceV1 {
    pub fn outcomes(&self) -> &[ManagedSqliteCatalogFenceAssertionOutcomeV1] {
        &self.outcomes
    }
    pub const fn assertion_count(&self) -> u64 {
        self.assertion_count
    }
    pub const fn range_probe_count(&self) -> u64 {
        self.range_probe_count
    }
    pub const fn point_read_count(&self) -> u64 {
        self.point_read_count
    }
    pub const fn progress_callbacks_used(&self) -> u64 {
        self.progress_callbacks_used
    }
    pub const fn transcript(&self) -> &ManagedSqliteCatalogFenceAssertionTranscriptV1 {
        &self.transcript
    }
    pub const fn transcript_commitment(&self) -> &[u8; 32] {
        self.transcript.commitment()
    }
    pub const fn policy_fingerprint(&self) -> &[u8; 32] {
        &self.policy_fingerprint
    }
}

/// Owned evidence from the bounded routine generation fence.
///
/// It contains a primary-index-visible catalog and conditional point/range
/// observations, followed by strict close and source revalidation.  It does
/// not contain a physical census, integrity-check result, page census, whole
/// table commitment, cleanup authority, or a claim of physical completeness.
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn needs_clone<T: Clone>() {}
/// needs_clone::<ManagedSqliteClosedCatalogFenceV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn needs_debug<T: std::fmt::Debug>() {}
/// needs_debug::<ManagedSqliteClosedCatalogFenceV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn needs_default<T: Default>() {}
/// needs_default::<ManagedSqliteClosedCatalogFenceV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn needs_display<T: std::fmt::Display>() {}
/// needs_display::<ManagedSqliteClosedCatalogFenceV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn needs_serialize<T: serde::Serialize>() {}
/// needs_serialize::<ManagedSqliteClosedCatalogFenceV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn no_physical(fence: &ManagedSqliteClosedCatalogFenceV1) { let _ = fence.physical(); }
/// ```
///
/// ```compile_fail
/// use cozo::{ManagedSqliteClosedAuditV1, ManagedSqliteClosedCatalogFenceV1};
/// fn no_upgrade(fence: ManagedSqliteClosedCatalogFenceV1) -> ManagedSqliteClosedAuditV1 { fence.into() }
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn needs_send<T: Send>() {}
/// needs_send::<ManagedSqliteClosedCatalogFenceV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedSqliteClosedCatalogFenceV1;
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<ManagedSqliteClosedCatalogFenceV1>();
/// ```
#[must_use = "retain the closed catalog fence until its downstream verifier consumes it"]
pub struct ManagedSqliteClosedCatalogFenceV1 {
    catalog: ManagedSqlitePrimaryIndexCatalogV1,
    assertions: ManagedSqliteCatalogFenceAssertionEvidenceV1,
    source: ManagedSqliteClosedSourceEvidenceV1,
    snapshot_policy: ManagedSnapshotPolicy,
    selector: ManagedCatalogFencePolicy,
    catalog_fence_policy_fingerprint: [u8; 32],
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedSqliteClosedCatalogFenceV1 {
    pub const fn catalog(&self) -> &ManagedSqlitePrimaryIndexCatalogV1 {
        &self.catalog
    }
    pub const fn assertions(&self) -> &ManagedSqliteCatalogFenceAssertionEvidenceV1 {
        &self.assertions
    }
    pub const fn source(&self) -> &ManagedSqliteClosedSourceEvidenceV1 {
        &self.source
    }
    pub const fn snapshot_policy(&self) -> ManagedSnapshotPolicy {
        self.snapshot_policy
    }
    pub const fn selector(&self) -> ManagedCatalogFencePolicy {
        self.selector
    }
    pub const fn catalog_policy_fingerprint(&self) -> &[u8; 32] {
        self.catalog.policy_fingerprint()
    }
    pub const fn assertion_policy_fingerprint(&self) -> &[u8; 32] {
        self.assertions.policy_fingerprint()
    }
    pub const fn source_policy_fingerprint(&self) -> &[u8; 32] {
        self.source.policy_fingerprint()
    }
    pub const fn catalog_fence_policy_fingerprint(&self) -> &[u8; 32] {
        &self.catalog_fence_policy_fingerprint
    }

    /// Consume this closed fence into the sole authority for a writable
    /// existing-only runtime open.
    ///
    /// The permit preserves the fence's exact supplied pathname, main-file
    /// identity, and admitted residual projections. It cannot be cloned or
    /// reconstructed from a raw path. Consumption reopens the source through
    /// the immutable read-only guard and checks those bindings before any
    /// READWRITE SQLite handle exists.
    pub fn into_existing_sqlite_open_permit_v1(self) -> ManagedExistingSqliteOpenPermitV1 {
        let ManagedSqliteClosedSourceEvidenceV1 {
            supplied_path,
            main_identity,
            wal_residual,
            shm_residual,
            ..
        } = self.source;
        ManagedExistingSqliteOpenPermitV1 {
            supplied_path,
            main_identity,
            wal_residual,
            shm_residual,
            _thread_bound: PhantomData,
        }
    }
}

/// Single-use authority to open one fenced SQLite source as a writable
/// existing-only runtime database.
///
/// This value is minted only by consuming
/// [`ManagedSqliteClosedCatalogFenceV1`]. There is intentionally no raw-path
/// constructor: a READWRITE SQLite open may recover a hot rollback journal or
/// create/update WAL-family files before a later logical-header rejection.
///
/// The runtime consumption check holds the original main-file descriptor and
/// immutable SQLite connection across the separate READWRITE open and
/// read-only initialization, then rechecks the main-file identity. It does not
/// require residual absence afterward: an admitted clean WAL-header database
/// may legitimately create WAL/SHM state once the runtime opens it. This still
/// is not an fd-backed VFS: a same-authority rename ABA or symlink swap that is
/// completed between pathname checks can evade observation. The exact Unix
/// `OsStr` spelling, including non-UTF-8 bytes, is retained by the fence and
/// permit. The current `sqlite` 0.36 runtime connector converts `Path` through
/// `to_str`, however, so a non-UTF-8 path is rejected before READWRITE open; it
/// is never lossily redirected to an alias. No equivalent non-Unix identity
/// boundary is implemented. Later pool reconnects retain no-CREATE semantics
/// but do not consume a new permit or rebind the original inode. Callers must
/// therefore retain an outer lease and recheck their generation/identity
/// boundary around permit consumption and for the runtime handle's lifetime.
///
/// ```compile_fail
/// use cozo::ManagedExistingSqliteOpenPermitV1;
/// fn needs_clone<T: Clone>() {}
/// needs_clone::<ManagedExistingSqliteOpenPermitV1>();
/// ```
///
/// ```compile_fail
/// use cozo::ManagedExistingSqliteOpenPermitV1;
/// fn needs_copy<T: Copy>() {}
/// needs_copy::<ManagedExistingSqliteOpenPermitV1>();
/// ```
#[must_use = "consume the permit exactly once to open its fenced SQLite source"]
pub struct ManagedExistingSqliteOpenPermitV1 {
    supplied_path: PathBuf,
    main_identity: ManagedSqliteClosedMainFileIdentityV1,
    wal_residual: ManagedSqliteClosedResidualV1,
    shm_residual: ManagedSqliteClosedResidualV1,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedExistingSqliteOpenPermitV1 {
    fn into_guarded_source(self) -> Result<ExistingSqliteSnapshotSource> {
        let source = ExistingSqliteSnapshotSource::open(&self.supplied_path)?;
        if !closed_main_identity_matches_frozen(&self.main_identity, &source.main_file) {
            bail!(
                "existing-only SQLite permit main-file identity no longer matches its closed fence"
            );
        }
        if !closed_residual_matches_frozen(&self.wal_residual, &source.residuals.wal)
            || !closed_residual_matches_frozen(&self.shm_residual, &source.residuals.shm)
        {
            bail!("existing-only SQLite permit residual family no longer matches its closed fence");
        }
        source.verify_source("existing-only SQLite permit consumption")?;
        Ok(source)
    }
}

fn closed_main_identity_matches_frozen(
    expected: &ManagedSqliteClosedMainFileIdentityV1,
    observed: &FrozenSourceMainFile,
) -> bool {
    #[cfg(not(unix))]
    {
        let _ = (expected, observed);
        false
    }
    #[cfg(unix)]
    {
        let observed = &observed.identity;
        expected.device == observed.dev
            && expected.inode == observed.ino
            && expected.mode == observed.mode
            && expected.uid == observed.uid
            && expected.gid == observed.gid
            && expected.link_count == observed.nlink
            && expected.length == observed.len
            && expected.mtime_seconds == observed.mtime
            && expected.mtime_nanoseconds == observed.mtime_nsec
            && expected.ctime_seconds == observed.ctime
            && expected.ctime_nanoseconds == observed.ctime_nsec
    }
}

fn closed_residual_matches_frozen(
    expected: &ManagedSqliteClosedResidualV1,
    observed: &FrozenResidualFile,
) -> bool {
    #[cfg(not(unix))]
    {
        let _ = (expected, observed);
        false
    }
    #[cfg(unix)]
    {
        match (
            expected.present,
            expected.metadata.as_ref(),
            expected.sha256,
            observed,
        ) {
            (false, None, None, FrozenResidualFile::Absent) => true,
            (
                true,
                Some(expected),
                Some(expected_sha256),
                FrozenResidualFile::Present {
                    metadata: observed,
                    sha256: observed_sha256,
                },
            ) => {
                expected.length == observed.len
                    && expected.device == observed.dev
                    && expected.inode == observed.ino
                    && expected.mode == observed.mode
                    && expected.link_count == observed.nlink
                    && expected.uid == observed.uid
                    && expected.gid == observed.gid
                    && expected.rdev == observed.rdev
                    && expected.blocks == observed.blocks
                    && expected.block_size == observed.block_size
                    && expected.mtime_seconds == observed.mtime
                    && expected.mtime_nanoseconds == observed.mtime_nsec
                    && expected.ctime_seconds == observed.ctime
                    && expected.ctime_nanoseconds == observed.ctime_nsec
                    && expected_sha256 == *observed_sha256
            }
            _ => false,
        }
    }
}

struct ManagedAssertionTranscriptReservationV1 {
    next_entry_count: u64,
    next_framed_bytes: u64,
}

struct ManagedCatalogFenceTranscriptReservationV1 {
    next_entries: u64,
    next_bytes: u64,
}

struct ManagedCatalogFenceTranscriptBuilderV1 {
    hasher: Sha256,
    entries: u64,
    bytes: u64,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedCatalogFenceTranscriptBuilderV1 {
    fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1);
        Self {
            hasher,
            entries: 0,
            bytes: 0,
            _thread_bound: PhantomData,
        }
    }

    fn reserve(&self, fields: &[&[u8]]) -> Result<ManagedCatalogFenceTranscriptReservationV1> {
        let next_entries = self
            .entries
            .checked_add(1)
            .filter(|count| *count <= MANAGED_CATALOG_FENCE_ASSERTION_LIMIT_V1 as u64)
            .ok_or_else(|| {
                miette!("managed SQLite catalog-fence transcript entry cap was exceeded")
            })?;
        let mut entry_bytes = 1_u64;
        for field in fields {
            entry_bytes = entry_bytes
                .checked_add(8)
                .and_then(|total| total.checked_add(u64::try_from(field.len()).ok()?))
                .ok_or_else(|| {
                    miette!("managed SQLite catalog-fence transcript length overflowed")
                })?;
        }
        entry_bytes = entry_bytes
            .checked_add(8)
            .ok_or_else(|| miette!("managed SQLite catalog-fence transcript length overflowed"))?;
        let next_bytes = self
            .bytes
            .checked_add(entry_bytes)
            .filter(|total| {
                total
                    .checked_add(9)
                    .is_some_and(|end| end <= MANAGED_CATALOG_FENCE_TRANSCRIPT_BYTE_LIMIT_V1)
            })
            .ok_or_else(|| {
                miette!("managed SQLite catalog-fence transcript byte cap was exceeded")
            })?;
        Ok(ManagedCatalogFenceTranscriptReservationV1 {
            next_entries,
            next_bytes,
        })
    }

    fn commit(
        &mut self,
        reservation: ManagedCatalogFenceTranscriptReservationV1,
        fields: &[&[u8]],
    ) -> Result<()> {
        let expected = self.reserve(fields)?;
        if expected.next_entries != reservation.next_entries
            || expected.next_bytes != reservation.next_bytes
        {
            bail!("managed SQLite catalog-fence transcript reservation changed");
        }
        self.hasher.update([0x01]);
        for field in fields {
            self.hasher.update(
                u64::try_from(field.len())
                    .map_err(|_| {
                        miette!("managed SQLite catalog-fence transcript field overflowed")
                    })?
                    .to_be_bytes(),
            );
            self.hasher.update(field);
        }
        self.hasher.update(
            u64::try_from(fields.len())
                .map_err(|_| {
                    miette!("managed SQLite catalog-fence transcript field count overflowed")
                })?
                .to_be_bytes(),
        );
        self.entries = reservation.next_entries;
        self.bytes = reservation.next_bytes;
        Ok(())
    }

    fn finish(mut self) -> Result<[u8; 32]> {
        if self
            .bytes
            .checked_add(9)
            .filter(|bytes| *bytes <= MANAGED_CATALOG_FENCE_TRANSCRIPT_BYTE_LIMIT_V1)
            .is_none()
        {
            bail!("managed SQLite catalog-fence transcript finalization overflowed");
        }
        self.hasher.update([0xff]);
        self.hasher.update(self.entries.to_be_bytes());
        Ok(self.hasher.finalize().into())
    }
}

/// Private-constructor planner for a bounded catalog-only assertion vocabulary.
///
/// The callback receives this only through a higher-ranked consuming boundary;
/// its catalog borrow cannot escape and it has neither a raw SQLite handle nor
/// a physical census.
pub struct ManagedSqliteCatalogAssertionPlannerV1<'reader> {
    catalog: &'reader ManagedSqlitePrimaryIndexCatalogV1,
    connection: &'reader SnapshotConnection,
    range_statement: Option<ManagedStatement>,
    point_statement: Option<ManagedStatement>,
    assertions_used: usize,
    ranges_used: usize,
    points_used: usize,
    count_ids: BTreeSet<u64>,
    point_keys: BTreeSet<(u64, &'static str)>,
    outcome_tags: BTreeSet<&'static str>,
    transcript: ManagedCatalogFenceTranscriptBuilderV1,
    outcomes: Vec<ManagedSqliteCatalogFenceAssertionOutcomeV1>,
    poisoned: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<'reader> ManagedSqliteCatalogAssertionPlannerV1<'reader> {
    pub const fn catalog(&self) -> &'reader ManagedSqlitePrimaryIndexCatalogV1 {
        self.catalog
    }

    pub fn require_exact_row_count(&mut self, relation_id: u64, expected: u64) -> Result<()> {
        self.run(|planner| {
            let relation = planner.admit_relation(relation_id)?;
            if expected > MANAGED_CATALOG_FENCE_EXPECTED_MAX_V1 {
                bail!("managed SQLite catalog-fence expected row count exceeded its cap");
            }
            planner.admit_assertion(false)?;
            if !planner.count_ids.insert(relation_id) {
                bail!("managed SQLite catalog-fence repeated a count relation id");
            }
            let kind = [0_u8];
            let id = relation_id.to_be_bytes();
            let count = expected.to_be_bytes();
            let fields = [kind.as_slice(), id.as_slice(), count.as_slice()];
            let reservation = planner.transcript.reserve(&fields)?;
            planner.visit_range(relation, expected)?;
            planner.transcript.commit(reservation, &fields)?;
            planner
                .outcomes
                .push(ManagedSqliteCatalogFenceAssertionOutcomeV1 {
                    kind: ManagedCatalogFenceAssertionOutcomeKindV1::ExactRowCount {
                        relation_id,
                        expected,
                    },
                    _thread_bound: PhantomData,
                });
            Ok(())
        })
    }

    pub fn require_string_pair(
        &mut self,
        relation_id: u64,
        key: &'static str,
        value: &'static str,
    ) -> Result<()> {
        self.run(|planner| {
            let relation = planner.admit_relation(relation_id)?;
            planner.admit_string(key, "key")?;
            planner.admit_string(value, "value")?;
            planner.admit_assertion(true)?;
            planner.admit_point_key(relation_id, key)?;
            let kind = [1_u8];
            let id = relation_id.to_be_bytes();
            let fields = [
                kind.as_slice(),
                id.as_slice(),
                key.as_bytes(),
                value.as_bytes(),
            ];
            let reservation = planner.transcript.reserve(&fields)?;
            let encoded = vec![DataValue::from(key)].encode_as_key(relation);
            let statement = planner.point_statement.as_ref().ok_or_else(|| {
                miette!("managed SQLite catalog-fence point statement was unavailable")
            })?;
            #[cfg(test)]
            if MANAGED_CATALOG_FENCE_POINT_FAILURE.with(|failure| failure.replace(false)) {
                bail!("injected managed SQLite catalog-fence point execution failure");
            }
            #[cfg(test)]
            MANAGED_CATALOG_FENCE_POINT_EXECUTIONS.with(|count| count.set(count.get() + 1));
            managed_visit_forced_primary_index_point(
                statement,
                &encoded,
                None,
                ManagedPointValueLengthV1::AtMost(MANAGED_CATALOG_FENCE_POINT_VALUE_LIMIT_V1),
                |returned_key, returned_value| {
                    if managed_select_assertion_string_pair(
                        returned_key,
                        returned_value,
                        key,
                        &[value],
                    )? != 0
                    {
                        bail!("managed SQLite catalog-fence exact string-pair assertion failed");
                    }
                    Ok(())
                },
            )?;
            planner.transcript.commit(reservation, &fields)?;
            planner
                .outcomes
                .push(ManagedSqliteCatalogFenceAssertionOutcomeV1 {
                    kind: ManagedCatalogFenceAssertionOutcomeKindV1::StringPair {
                        relation_id,
                        key,
                        value,
                    },
                    _thread_bound: PhantomData,
                });
            Ok(())
        })
    }

    pub fn require_string_pair_one_of(
        &mut self,
        outcome_tag: &'static str,
        relation_id: u64,
        key: &'static str,
        alternatives: [&'static str; 2],
    ) -> Result<()> {
        self.run(|planner| {
            let relation = planner.admit_relation(relation_id)?;
            planner.admit_string(outcome_tag, "outcome tag")?;
            planner.admit_string(key, "key")?;
            planner.admit_string(alternatives[0], "alternative")?;
            planner.admit_string(alternatives[1], "alternative")?;
            if alternatives[0] == alternatives[1] {
                bail!("managed SQLite catalog-fence alternatives must differ");
            }
            planner.admit_assertion(true)?;
            planner.admit_point_key(relation_id, key)?;
            if !planner.outcome_tags.insert(outcome_tag) {
                bail!("managed SQLite catalog-fence repeated an outcome tag");
            }
            let kind = [2_u8];
            let id = relation_id.to_be_bytes();
            let selected_placeholder = [0_u8];
            let preflight = [
                kind.as_slice(),
                outcome_tag.as_bytes(),
                id.as_slice(),
                key.as_bytes(),
                alternatives[0].as_bytes(),
                alternatives[1].as_bytes(),
                selected_placeholder.as_slice(),
            ];
            let reservation = planner.transcript.reserve(&preflight)?;
            let encoded = vec![DataValue::from(key)].encode_as_key(relation);
            let statement = planner.point_statement.as_ref().ok_or_else(|| {
                miette!("managed SQLite catalog-fence point statement was unavailable")
            })?;
            #[cfg(test)]
            if MANAGED_CATALOG_FENCE_POINT_FAILURE.with(|failure| failure.replace(false)) {
                bail!("injected managed SQLite catalog-fence point execution failure");
            }
            #[cfg(test)]
            MANAGED_CATALOG_FENCE_POINT_EXECUTIONS.with(|count| count.set(count.get() + 1));
            let selected_index = managed_visit_forced_primary_index_point(
                statement,
                &encoded,
                None,
                ManagedPointValueLengthV1::AtMost(MANAGED_CATALOG_FENCE_POINT_VALUE_LIMIT_V1),
                |returned_key, returned_value| {
                    managed_select_assertion_string_pair(
                        returned_key,
                        returned_value,
                        key,
                        &alternatives,
                    )
                },
            )?;
            let selected = [selected_index];
            let fields = [
                kind.as_slice(),
                outcome_tag.as_bytes(),
                id.as_slice(),
                key.as_bytes(),
                alternatives[0].as_bytes(),
                alternatives[1].as_bytes(),
                selected.as_slice(),
            ];
            planner.transcript.commit(reservation, &fields)?;
            planner
                .outcomes
                .push(ManagedSqliteCatalogFenceAssertionOutcomeV1 {
                    kind: ManagedCatalogFenceAssertionOutcomeKindV1::StringPairOneOf {
                        outcome_tag,
                        relation_id,
                        key,
                        alternatives,
                        selected_index,
                    },
                    _thread_bound: PhantomData,
                });
            Ok(())
        })
    }

    fn new(
        catalog: &'reader ManagedSqlitePrimaryIndexCatalogV1,
        connection: &'reader SnapshotConnection,
    ) -> Result<Self> {
        managed_require_read_transaction(connection, "catalog-fence assertion planner entry")?;
        Ok(Self {
            catalog,
            connection,
            range_statement: Some(managed_prepare_fixed(
                connection,
                MANAGED_CATALOG_FENCE_RANGE_QUERY_V1,
                "catalog-fence range lookup",
                1,
                3,
            )?),
            point_statement: Some(managed_prepare_fixed(
                connection,
                MANAGED_POINT_QUERY,
                "catalog-fence point lookup",
                5,
                1,
            )?),
            assertions_used: 0,
            ranges_used: 0,
            points_used: 0,
            count_ids: BTreeSet::new(),
            point_keys: BTreeSet::new(),
            outcome_tags: BTreeSet::new(),
            transcript: ManagedCatalogFenceTranscriptBuilderV1::new(),
            outcomes: Vec::with_capacity(MANAGED_CATALOG_FENCE_ASSERTION_LIMIT_V1),
            poisoned: false,
            _thread_bound: PhantomData,
        })
    }

    fn run(&mut self, operation: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        if self.poisoned {
            bail!("managed SQLite catalog-fence planner is poisoned");
        }
        let result = operation(self);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn admit_relation(&self, relation_id: u64) -> Result<RelationId> {
        let relation = RelationId::try_raw_decode(&relation_id.to_be_bytes())
            .map_err(|_| miette!("managed SQLite catalog-fence relation id was invalid"))?;
        if relation == RelationId::SYSTEM
            || !self
                .catalog
                .catalog()
                .entries()
                .iter()
                .any(|entry| entry.relation().id() == relation_id)
        {
            bail!("managed SQLite catalog-fence relation id was not admitted by its catalog");
        }
        Ok(relation)
    }

    fn admit_string(&self, value: &'static str, label: &'static str) -> Result<()> {
        if value.is_empty() || value.len() > MANAGED_CATALOG_FENCE_STRING_LIMIT_V1 {
            bail!("managed SQLite catalog-fence {label} exceeded its static string boundary");
        }
        Ok(())
    }

    fn admit_assertion(&mut self, point: bool) -> Result<()> {
        let assertions = self
            .assertions_used
            .checked_add(1)
            .filter(|count| *count <= MANAGED_CATALOG_FENCE_ASSERTION_LIMIT_V1)
            .ok_or_else(|| miette!("managed SQLite catalog-fence assertion cap was exceeded"))?;
        let ranges = if point {
            self.ranges_used
        } else {
            self.ranges_used
                .checked_add(1)
                .filter(|count| *count <= MANAGED_CATALOG_FENCE_RANGE_LIMIT_V1)
                .ok_or_else(|| miette!("managed SQLite catalog-fence range cap was exceeded"))?
        };
        let points = if point {
            self.points_used
                .checked_add(1)
                .filter(|count| *count <= MANAGED_CATALOG_FENCE_POINT_LIMIT_V1)
                .ok_or_else(|| miette!("managed SQLite catalog-fence point cap was exceeded"))?
        } else {
            self.points_used
        };
        self.assertions_used = assertions;
        self.ranges_used = ranges;
        self.points_used = points;
        Ok(())
    }

    fn admit_point_key(&mut self, relation_id: u64, key: &'static str) -> Result<()> {
        if !self.point_keys.insert((relation_id, key)) {
            bail!("managed SQLite catalog-fence repeated a point key");
        }
        Ok(())
    }

    fn visit_range(&mut self, relation: RelationId, expected: u64) -> Result<()> {
        let lower = relation.raw_encode();
        let upper = relation.next().raw_encode();
        let limit = i64::try_from(
            expected
                .checked_add(1)
                .ok_or_else(|| miette!("managed SQLite catalog-fence range limit overflowed"))?,
        )
        .map_err(|_| miette!("managed SQLite catalog-fence range limit was not representable"))?;
        let statement = self.range_statement.as_ref().ok_or_else(|| {
            miette!("managed SQLite catalog-fence range statement was unavailable")
        })?;
        let initial =
            managed_catalog_fence_reset_and_clear(statement, "catalog-fence range before bind");
        let operation = initial.and_then(|()| {
            for (index, bytes) in [(1, lower.as_slice()), (2, upper.as_slice())] {
                let len = i32::try_from(bytes.len()).map_err(|_| {
                    miette!("managed SQLite catalog-fence bound length was invalid")
                })?;
                let code = unsafe {
                    ffi::sqlite3_bind_blob(
                        statement.raw,
                        index,
                        bytes.as_ptr().cast(),
                        len,
                        managed_sqlite_transient_destructor(),
                    )
                };
                if code != ffi::SQLITE_OK {
                    bail!("managed SQLite catalog-fence range binding failed with code {code}");
                }
            }
            let code = unsafe { ffi::sqlite3_bind_int64(statement.raw, 3, limit) };
            if code != ffi::SQLITE_OK {
                bail!("managed SQLite catalog-fence range limit binding failed with code {code}");
            }
            #[cfg(test)]
            MANAGED_CATALOG_FENCE_RANGE_EXECUTIONS.with(|count| count.set(count.get() + 1));
            #[cfg(test)]
            if MANAGED_CATALOG_FENCE_RANGE_PANIC_AFTER_BIND.with(|panic| panic.replace(false)) {
                panic!("injected managed SQLite catalog-fence range panic after transient bind");
            }
            let mut observed = 0_u64;
            loop {
                let step = unsafe { ffi::sqlite3_step(statement.raw) };
                if step == ffi::SQLITE_DONE {
                    break;
                }
                if step != ffi::SQLITE_ROW {
                    bail!("managed SQLite catalog-fence range lookup failed with code {step}");
                }
                #[cfg(test)]
                MANAGED_CATALOG_FENCE_RANGE_ROWS.with(|count| count.set(count.get() + 1));
                observed = observed.checked_add(1).ok_or_else(|| {
                    miette!("managed SQLite catalog-fence range count overflowed")
                })?;
                if observed > expected {
                    bail!("managed SQLite catalog-fence range assertion found an extra row");
                }
            }
            if observed != expected {
                bail!("managed SQLite catalog-fence exact row-count assertion failed");
            }
            let sort =
                unsafe { ffi::sqlite3_stmt_status(statement.raw, ffi::SQLITE_STMTSTATUS_SORT, 1) };
            let fullscan = unsafe {
                ffi::sqlite3_stmt_status(statement.raw, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1)
            };
            if sort != 0 || fullscan != 0 {
                bail!(
                    "managed SQLite catalog-fence range lookup left its forced primary-index path"
                );
            }
            Ok(())
        });
        combine_with_postcondition(
            operation,
            managed_catalog_fence_reset_and_clear(statement, "catalog-fence range after step"),
            "managed SQLite catalog-fence range cleanup failed",
        )
    }

    fn finish(
        mut self,
        callback: Result<()>,
    ) -> Result<ManagedSqliteCatalogFenceAssertionEvidenceV1> {
        let operation = callback.and_then(|()| {
            if self.poisoned {
                bail!("managed SQLite catalog-fence planner was poisoned");
            }
            if self.outcomes.len() != self.assertions_used
                || self.transcript.entries != self.assertions_used as u64
            {
                bail!("managed SQLite catalog-fence transcript or outcome count was inconsistent");
            }
            managed_require_read_transaction(
                self.connection,
                "catalog-fence assertion planner completion",
            )?;
            Ok(ManagedSqliteCatalogFenceAssertionEvidenceV1 {
                outcomes: self.outcomes.into_boxed_slice(),
                assertion_count: self.assertions_used as u64,
                range_probe_count: self.ranges_used as u64,
                point_read_count: self.points_used as u64,
                progress_callbacks_used: 0,
                transcript: ManagedSqliteCatalogFenceAssertionTranscriptV1 {
                    commitment: self.transcript.finish()?,
                    _thread_bound: PhantomData,
                },
                policy_fingerprint: MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1,
                _thread_bound: PhantomData,
            })
        });
        let range = self
            .range_statement
            .take()
            .map(|statement| statement.finish())
            .ok_or_else(|| {
                miette!("managed SQLite catalog-fence range statement was already finalized")
            });
        #[cfg(test)]
        MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CALLS.with(|calls| calls.set(calls.get() + 1));
        #[cfg(test)]
        let range = range.map(|actual| {
            MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CODE
                .with(|code| code.replace(None))
                .unwrap_or(actual)
        });
        let point = self
            .point_statement
            .take()
            .map(|statement| statement.finish())
            .ok_or_else(|| {
                miette!("managed SQLite catalog-fence point statement was already finalized")
            });
        #[cfg(test)]
        MANAGED_CATALOG_FENCE_POINT_FINALIZE_CALLS.with(|calls| calls.set(calls.get() + 1));
        #[cfg(test)]
        let point = point.map(|actual| {
            MANAGED_CATALOG_FENCE_POINT_FINALIZE_CODE
                .with(|code| code.replace(None))
                .unwrap_or(actual)
        });
        let finalization = match (range, point) {
            (Ok(ffi::SQLITE_OK), Ok(ffi::SQLITE_OK)) => Ok(()),
            (range, point) => Err(miette!(
                "managed SQLite catalog-fence statement finalization failed with codes {:?}/{:?}",
                range.ok(),
                point.ok()
            )),
        };
        combine_with_postcondition(
            operation,
            finalization,
            "managed SQLite catalog-fence assertion completion failed",
        )
    }
}

struct ManagedAssertionTranscriptBuilderV1 {
    hasher: Sha256,
    entry_count: u64,
    framed_bytes: u64,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedAssertionTranscriptBuilderV1 {
    fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1);
        Self {
            hasher,
            entry_count: 0,
            framed_bytes: 0,
            _thread_bound: PhantomData,
        }
    }

    fn reserve(&self, fields: &[&[u8]]) -> Result<ManagedAssertionTranscriptReservationV1> {
        let next_entry_count = self
            .entry_count
            .checked_add(1)
            .filter(|count| *count <= MANAGED_ASSERTION_LIMIT as u64)
            .ok_or_else(|| miette!("managed SQLite assertion transcript entry count overflowed"))?;
        let mut entry_bytes = 1_u64;
        for field in fields {
            let length = u64::try_from(field.len())
                .map_err(|_| miette!("managed SQLite assertion transcript field overflowed"))?;
            entry_bytes = entry_bytes
                .checked_add(8)
                .and_then(|bytes| bytes.checked_add(length))
                .ok_or_else(|| miette!("managed SQLite assertion transcript length overflowed"))?;
        }
        entry_bytes = entry_bytes
            .checked_add(8)
            .ok_or_else(|| miette!("managed SQLite assertion transcript length overflowed"))?;
        let next_framed_bytes = self
            .framed_bytes
            .checked_add(entry_bytes)
            .and_then(|bytes| bytes.checked_add(9))
            .filter(|bytes_with_terminator| {
                *bytes_with_terminator <= MANAGED_ASSERTION_TRANSCRIPT_BYTE_LIMIT
            })
            .and_then(|bytes_with_terminator| bytes_with_terminator.checked_sub(9))
            .ok_or_else(|| miette!("managed SQLite assertion transcript byte cap was exceeded"))?;
        Ok(ManagedAssertionTranscriptReservationV1 {
            next_entry_count,
            next_framed_bytes,
        })
    }

    fn commit(
        &mut self,
        reservation: ManagedAssertionTranscriptReservationV1,
        fields: &[&[u8]],
    ) -> Result<()> {
        let expected = self.reserve(fields)?;
        if expected.next_entry_count != reservation.next_entry_count
            || expected.next_framed_bytes != reservation.next_framed_bytes
        {
            bail!("managed SQLite assertion transcript reservation changed");
        }
        self.hasher.update([0x01]);
        for field in fields {
            let length = u64::try_from(field.len())
                .map_err(|_| miette!("managed SQLite assertion transcript field overflowed"))?;
            self.hasher.update(length.to_be_bytes());
            self.hasher.update(field);
        }
        let field_count = u64::try_from(fields.len())
            .map_err(|_| miette!("managed SQLite assertion transcript field count overflowed"))?;
        self.hasher.update(field_count.to_be_bytes());
        self.entry_count = reservation.next_entry_count;
        self.framed_bytes = reservation.next_framed_bytes;
        Ok(())
    }

    fn finish(mut self) -> Result<[u8; 32]> {
        let final_bytes = self
            .framed_bytes
            .checked_add(9)
            .filter(|bytes| *bytes <= MANAGED_ASSERTION_TRANSCRIPT_BYTE_LIMIT)
            .ok_or_else(|| {
                miette!("managed SQLite assertion transcript finalization overflowed")
            })?;
        self.hasher.update([0xff]);
        self.hasher.update(self.entry_count.to_be_bytes());
        self.framed_bytes = final_bytes;
        Ok(self.hasher.finalize().into())
    }
}

/// A same-reader planner for the closed V1 relation-assertion vocabulary.
///
/// There is deliberately no public constructor. A later consuming reader
/// boundary supplies the planner under an HRTB callback so neither this value
/// nor its catalog/physical borrows can escape. Methods execute immediately;
/// the first failure permanently poisons the planner.
pub struct ManagedSqliteRelationAssertionPlannerV1<'reader> {
    catalog: &'reader ManagedSqlitePrimaryIndexCatalogV1,
    physical: &'reader ManagedSqlitePhysicalCensusV1,
    connection: &'reader SnapshotConnection,
    statement: Option<ManagedStatement>,
    assertions_used: usize,
    point_reads_used: usize,
    count_ids: BTreeSet<u64>,
    point_keys: BTreeSet<(u64, &'static str)>,
    outcome_tags: BTreeSet<&'static str>,
    transcript: ManagedAssertionTranscriptBuilderV1,
    outcomes: Vec<ManagedSqliteRelationAssertionOutcomeV1>,
    poisoned: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

impl<'reader> ManagedSqliteRelationAssertionPlannerV1<'reader> {
    pub const fn catalog(&self) -> &'reader ManagedSqlitePrimaryIndexCatalogV1 {
        self.catalog
    }

    pub const fn physical(&self) -> &'reader ManagedSqlitePhysicalCensusV1 {
        self.physical
    }

    pub fn require_exact_row_count(&mut self, relation_id: u64, expected: u64) -> Result<()> {
        self.run_assertion_method(|planner| {
            let actual = planner.admit_relation_id(relation_id)?;
            planner.admit_assertion(false)?;
            if planner.count_ids.contains(&relation_id) {
                bail!("managed SQLite relation assertion repeated a count relation id");
            }
            let kind = [0_u8];
            let id = relation_id.to_be_bytes();
            let count = expected.to_be_bytes();
            let fields = [kind.as_slice(), id.as_slice(), count.as_slice()];
            let reservation = planner.transcript.reserve(&fields)?;
            planner.count_ids.insert(relation_id);
            if actual != expected {
                bail!("managed SQLite exact relation row-count assertion failed");
            }
            planner.transcript.commit(reservation, &fields)?;
            planner
                .outcomes
                .push(ManagedSqliteRelationAssertionOutcomeV1 {
                    kind: ManagedRelationAssertionOutcomeKindV1::ExactRowCount {
                        relation_id,
                        expected,
                    },
                    _thread_bound: PhantomData,
                });
            Ok(())
        })
    }

    pub fn require_string_pair(
        &mut self,
        relation_id: u64,
        key: &'static str,
        value: &'static str,
    ) -> Result<()> {
        self.run_assertion_method(|planner| {
            let relation = planner.admit_relation_id_value(relation_id)?;
            planner.admit_static_string(key, "key")?;
            planner.admit_static_string(value, "value")?;
            planner.admit_assertion(true)?;
            planner.admit_point_key(relation_id, key)?;
            let kind = [1_u8];
            let id = relation_id.to_be_bytes();
            let fields = [
                kind.as_slice(),
                id.as_slice(),
                key.as_bytes(),
                value.as_bytes(),
            ];
            let reservation = planner.transcript.reserve(&fields)?;
            let encoded_key = vec![DataValue::from(key)].encode_as_key(relation);
            let statement = planner.statement()?;
            managed_visit_forced_primary_index_point(
                statement,
                &encoded_key,
                None,
                ManagedPointValueLengthV1::AtMost(MANAGED_ASSERTION_POINT_VALUE_BYTE_LIMIT),
                |returned_key, returned_value| {
                    if managed_select_assertion_string_pair(
                        returned_key,
                        returned_value,
                        key,
                        &[value],
                    )? != 0
                    {
                        bail!("managed SQLite exact string-pair assertion failed");
                    }
                    Ok(())
                },
            )?;
            planner.transcript.commit(reservation, &fields)?;
            planner
                .outcomes
                .push(ManagedSqliteRelationAssertionOutcomeV1 {
                    kind: ManagedRelationAssertionOutcomeKindV1::StringPair {
                        relation_id,
                        key,
                        value,
                    },
                    _thread_bound: PhantomData,
                });
            Ok(())
        })
    }

    pub fn require_string_pair_one_of(
        &mut self,
        outcome_tag: &'static str,
        relation_id: u64,
        key: &'static str,
        alternatives: [&'static str; 2],
    ) -> Result<()> {
        self.run_assertion_method(|planner| {
            let relation = planner.admit_relation_id_value(relation_id)?;
            planner.admit_static_string(outcome_tag, "outcome tag")?;
            planner.admit_static_string(key, "key")?;
            planner.admit_static_string(alternatives[0], "alternative")?;
            planner.admit_static_string(alternatives[1], "alternative")?;
            if alternatives[0] == alternatives[1] {
                bail!("managed SQLite one-of assertion alternatives were not distinct");
            }
            planner.admit_assertion(true)?;
            planner.admit_point_key(relation_id, key)?;
            if planner.outcome_tags.contains(outcome_tag) {
                bail!("managed SQLite relation assertion repeated an outcome tag");
            }
            let kind = [2_u8];
            let id = relation_id.to_be_bytes();
            let placeholder_selected = [0_u8];
            let preflight_fields = [
                kind.as_slice(),
                outcome_tag.as_bytes(),
                id.as_slice(),
                key.as_bytes(),
                alternatives[0].as_bytes(),
                alternatives[1].as_bytes(),
                placeholder_selected.as_slice(),
            ];
            let reservation = planner.transcript.reserve(&preflight_fields)?;
            planner.outcome_tags.insert(outcome_tag);
            let encoded_key = vec![DataValue::from(key)].encode_as_key(relation);
            let statement = planner.statement()?;
            let selected_index = managed_visit_forced_primary_index_point(
                statement,
                &encoded_key,
                None,
                ManagedPointValueLengthV1::AtMost(MANAGED_ASSERTION_POINT_VALUE_BYTE_LIMIT),
                |returned_key, returned_value| {
                    managed_select_assertion_string_pair(
                        returned_key,
                        returned_value,
                        key,
                        &alternatives,
                    )
                },
            )?;
            let selected = [selected_index];
            let fields = [
                kind.as_slice(),
                outcome_tag.as_bytes(),
                id.as_slice(),
                key.as_bytes(),
                alternatives[0].as_bytes(),
                alternatives[1].as_bytes(),
                selected.as_slice(),
            ];
            planner.transcript.commit(reservation, &fields)?;
            planner
                .outcomes
                .push(ManagedSqliteRelationAssertionOutcomeV1 {
                    kind: ManagedRelationAssertionOutcomeKindV1::StringPairOneOf {
                        outcome_tag,
                        relation_id,
                        key,
                        alternatives,
                        selected_index,
                    },
                    _thread_bound: PhantomData,
                });
            Ok(())
        })
    }

    fn new(
        catalog: &'reader ManagedSqlitePrimaryIndexCatalogV1,
        physical: &'reader ManagedSqlitePhysicalCensusV1,
        connection: &'reader SnapshotConnection,
    ) -> Result<Self> {
        managed_require_read_transaction(connection, "relation assertion planner entry")?;
        let statement = managed_prepare_fixed(
            connection,
            MANAGED_POINT_QUERY,
            "relation assertion point lookup",
            5,
            1,
        )?;
        Ok(Self {
            catalog,
            physical,
            connection,
            statement: Some(statement),
            assertions_used: 0,
            point_reads_used: 0,
            count_ids: BTreeSet::new(),
            point_keys: BTreeSet::new(),
            outcome_tags: BTreeSet::new(),
            transcript: ManagedAssertionTranscriptBuilderV1::new(),
            outcomes: Vec::with_capacity(MANAGED_ASSERTION_LIMIT),
            poisoned: false,
            _thread_bound: PhantomData,
        })
    }

    fn run_assertion_method(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<()>,
    ) -> Result<()> {
        if self.poisoned {
            bail!("managed SQLite relation assertion planner is poisoned");
        }
        let result = operation(self);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn admit_relation_id(&self, relation_id: u64) -> Result<u64> {
        let _relation = self.admit_relation_id_value(relation_id)?;
        self.physical
            .relation_row_counts
            .iter()
            .find(|row| row.relation_id == relation_id)
            .map(|row| row.row_count)
            .ok_or_else(|| miette!("managed SQLite assertion relation id was not admitted"))
    }

    fn admit_relation_id_value(&self, relation_id: u64) -> Result<RelationId> {
        let relation = RelationId::try_raw_decode(&relation_id.to_be_bytes())
            .map_err(|_| miette!("managed SQLite assertion relation id was invalid"))?;
        if relation == RelationId::SYSTEM
            || !self
                .physical
                .relation_row_counts
                .iter()
                .any(|row| row.relation_id == relation_id)
        {
            bail!("managed SQLite assertion relation id was not admitted");
        }
        Ok(relation)
    }

    fn admit_static_string(&self, value: &'static str, label: &'static str) -> Result<()> {
        if value.is_empty() || value.len() > MANAGED_ASSERTION_STRING_BYTE_LIMIT {
            bail!("managed SQLite assertion {label} exceeded its closed string boundary");
        }
        Ok(())
    }

    fn admit_assertion(&mut self, point: bool) -> Result<()> {
        let point_reads = if point {
            self.point_reads_used
                .checked_add(1)
                .filter(|count| *count <= MANAGED_ASSERTION_POINT_LIMIT)
                .ok_or_else(|| miette!("managed SQLite relation point-read cap was exceeded"))?
        } else {
            self.point_reads_used
        };
        let assertions = self
            .assertions_used
            .checked_add(1)
            .filter(|count| *count <= MANAGED_ASSERTION_LIMIT)
            .ok_or_else(|| miette!("managed SQLite relation assertion cap was exceeded"))?;
        self.assertions_used = assertions;
        self.point_reads_used = point_reads;
        Ok(())
    }

    fn admit_point_key(&mut self, relation_id: u64, key: &'static str) -> Result<()> {
        if self.point_keys.contains(&(relation_id, key)) {
            bail!("managed SQLite relation assertion repeated a point key");
        }
        self.point_keys.insert((relation_id, key));
        Ok(())
    }

    fn statement(&self) -> Result<&ManagedStatement> {
        self.statement
            .as_ref()
            .ok_or_else(|| miette!("managed SQLite relation assertion statement was unavailable"))
    }

    fn finish(
        mut self,
        callback_result: Result<()>,
    ) -> Result<ManagedSqliteRelationAssertionEvidenceV1> {
        let operation = callback_result.and_then(|()| {
            if self.poisoned {
                bail!("managed SQLite relation assertion planner was poisoned");
            }
            if self.outcomes.len() != self.assertions_used {
                bail!("managed SQLite relation assertion outcome count was inconsistent");
            }
            managed_require_read_transaction(
                self.connection,
                "relation assertion planner completion",
            )?;
            let assertion_count = u64::try_from(self.assertions_used)
                .map_err(|_| miette!("managed SQLite assertion count was not representable"))?;
            if self.transcript.entry_count != assertion_count {
                bail!("managed SQLite relation assertion transcript count was inconsistent");
            }
            let point_read_count = u64::try_from(self.point_reads_used)
                .map_err(|_| miette!("managed SQLite point-read count was not representable"))?;
            let transcript_commitment = self.transcript.finish()?;
            Ok(ManagedSqliteRelationAssertionEvidenceV1 {
                outcomes: self.outcomes.into_boxed_slice(),
                assertion_count,
                point_read_count,
                progress_callbacks_used: 0,
                transcript: ManagedSqliteAssertionTranscriptV1 {
                    commitment: transcript_commitment,
                    _thread_bound: PhantomData,
                },
                policy_fingerprint: MANAGED_ASSERTION_POLICY_FINGERPRINT_V1,
                _thread_bound: PhantomData,
            })
        });
        let finalize = match self.statement.take() {
            Some(statement) => {
                let actual_code = statement.finish();
                #[cfg(test)]
                let code = MANAGED_ASSERTION_TEST_FINALIZE_CODE
                    .with(|injected| injected.get())
                    .unwrap_or(actual_code);
                #[cfg(not(test))]
                let code = actual_code;
                if code == ffi::SQLITE_OK {
                    Ok(())
                } else {
                    Err(miette!(
                        "managed SQLite relation assertion finalization failed with code {code}"
                    ))
                }
            }
            None => Err(miette!(
                "managed SQLite relation assertion statement was already finalized"
            )),
        };
        combine_with_postcondition(
            operation,
            finalize,
            "managed SQLite relation assertion completion failed",
        )
    }
}

struct ManagedRecordVisitSelectionV1 {
    relation_ids: [u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1],
    relation_row_counts: [u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1],
    total_row_count: u64,
}

struct ManagedRecordVisitTranscriptBuilderV1 {
    hasher: Sha256,
    next_ordinal: u16,
    observed_total: u64,
    declared_total: u64,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ManagedRecordVisitTranscriptBuilderV1 {
    fn new(selection: &ManagedRecordVisitSelectionV1) -> Result<Self> {
        let relation_count = u16::try_from(MANAGED_RECORD_VISIT_RELATION_COUNT_V1)
            .map_err(|_| miette!("managed SQLite record-visit relation count was invalid"))?;
        let mut hasher = Sha256::new();
        hasher.update(MANAGED_RECORD_VISIT_TRANSCRIPT_DOMAIN_V1);
        hasher.update([0x00]);
        hasher.update(relation_count.to_be_bytes());
        for (index, (&relation_id, &row_count)) in selection
            .relation_ids
            .iter()
            .zip(selection.relation_row_counts.iter())
            .enumerate()
        {
            let ordinal = u16::try_from(index + 1)
                .map_err(|_| miette!("managed SQLite record-visit ordinal overflowed"))?;
            hasher.update(ordinal.to_be_bytes());
            hasher.update(relation_id.to_be_bytes());
            hasher.update(row_count.to_be_bytes());
        }
        hasher.update(selection.total_row_count.to_be_bytes());
        Ok(Self {
            hasher,
            next_ordinal: 1,
            observed_total: 0,
            declared_total: selection.total_row_count,
            _thread_bound: PhantomData,
        })
    }

    fn begin_relation(&mut self, ordinal: u16, relation_id: u64, expected: u64) -> Result<()> {
        if ordinal != self.next_ordinal
            || ordinal == 0
            || usize::from(ordinal) > MANAGED_RECORD_VISIT_RELATION_COUNT_V1
        {
            bail!("managed SQLite record-visit transcript relation order was invalid");
        }
        self.hasher.update([0x01]);
        self.hasher.update(ordinal.to_be_bytes());
        self.hasher.update(relation_id.to_be_bytes());
        self.hasher.update(expected.to_be_bytes());
        Ok(())
    }

    fn record(&mut self, ordinal: u16, key: &[u8], value: &[u8]) -> Result<()> {
        if ordinal != self.next_ordinal {
            bail!("managed SQLite record-visit transcript record order was invalid");
        }
        let key_len = u64::try_from(key.len())
            .map_err(|_| miette!("managed SQLite record-visit transcript key overflowed"))?;
        let value_len = u64::try_from(value.len())
            .map_err(|_| miette!("managed SQLite record-visit transcript value overflowed"))?;
        self.observed_total = self
            .observed_total
            .checked_add(1)
            .filter(|count| *count <= self.declared_total)
            .ok_or_else(|| {
                miette!("managed SQLite record-visit transcript row count overflowed")
            })?;
        self.hasher.update([0x02]);
        self.hasher.update(ordinal.to_be_bytes());
        self.hasher.update(key_len.to_be_bytes());
        self.hasher.update(key);
        self.hasher.update(value_len.to_be_bytes());
        self.hasher.update(value);
        Ok(())
    }

    fn end_relation(&mut self, ordinal: u16, rows: u64) -> Result<()> {
        if ordinal != self.next_ordinal {
            bail!("managed SQLite record-visit transcript relation end order was invalid");
        }
        self.hasher.update([0x03]);
        self.hasher.update(ordinal.to_be_bytes());
        self.hasher.update(rows.to_be_bytes());
        self.next_ordinal = self
            .next_ordinal
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite record-visit transcript ordinal overflowed"))?;
        Ok(())
    }

    fn finish(mut self, cumulative_progress_callbacks: u64) -> Result<[u8; 32]> {
        let relation_count = u16::try_from(MANAGED_RECORD_VISIT_RELATION_COUNT_V1)
            .map_err(|_| miette!("managed SQLite record-visit relation count was invalid"))?;
        if self.next_ordinal != relation_count + 1 || self.observed_total != self.declared_total {
            bail!("managed SQLite record-visit transcript did not close exactly");
        }
        self.hasher.update([0xff]);
        self.hasher.update(relation_count.to_be_bytes());
        self.hasher.update(self.observed_total.to_be_bytes());
        self.hasher
            .update(cumulative_progress_callbacks.to_be_bytes());
        Ok(self.hasher.finalize().into())
    }
}

struct ManagedRecordVisitScanOutputV1<S> {
    selection: ManagedRecordVisitSelectionV1,
    transcript: ManagedRecordVisitTranscriptBuilderV1,
    sink: S,
}

impl<S> ManagedRecordVisitScanOutputV1<S> {
    fn finish(
        self,
        cumulative_progress_callbacks: u64,
    ) -> Result<(ManagedSqliteRecordVisitEvidenceV1, S)> {
        let transcript_commitment = self.transcript.finish(cumulative_progress_callbacks)?;
        Ok((
            ManagedSqliteRecordVisitEvidenceV1 {
                relation_ids: self.selection.relation_ids,
                relation_row_counts: self.selection.relation_row_counts,
                total_row_count: self.selection.total_row_count,
                progress_callbacks_used: cumulative_progress_callbacks,
                transcript_commitment,
                policy_fingerprint: MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1,
                _thread_bound: PhantomData,
            },
            self.sink,
        ))
    }
}

/// An existing, quiescent SQLite database opened solely as a physical snapshot source.
///
/// This deliberately does not construct [`crate::Db`] or [`SqliteStorage`], and exposes
/// neither SQL nor transactions. A rollback journal or nonempty WAL is refused. A zero-byte
/// WAL and a bounded shared-memory residual may remain, but their bytes and durable metadata
/// are frozen across open and backup. The source is opened read-only with SQLite's
/// `immutable=1` URI contract, so the caller must hold whatever lease makes the main file and
/// admitted residuals genuinely quiescent for this value's lifetime.
///
/// On Unix, the main file is held through a read-only `O_NOFOLLOW | O_CLOEXEC`
/// descriptor and its durable `lstat`/`fstat` identity is frozen across open and backup.
/// A value returned by [`Self::backup_to_new_snapshot_source_v1`] additionally retains an
/// unexposed duplicate of the create-new destination's read-write descriptor. That duplicate is
/// the only retained descriptor proven to descend from the original `create_new` lineage; it lets
/// later checks bind that provenance to the current pathname alongside the independently opened
/// read-only descriptor. This costs one extra file descriptor and retains latent write capability,
/// though no API exposes it and the ordinary main descriptor plus SQLite connection remain
/// read-only.
/// Aggregate SQLite schema parsing is fused before database open by requiring the
/// schema b-tree root to be one leaf page containing at most 64 records.
/// Non-Unix targets currently fail closed because portable Rust metadata does not expose an
/// equivalent stable file identity. The caller still owns canonical ancestor validation and a
/// lease excluding same-authority rename ABA between checks; a host needing an atomic binding
/// between this descriptor and SQLite's separately opened handle must supply an fd-backed VFS or
/// an OS snapshot.
pub struct ExistingSqliteSnapshotSource {
    path: PathBuf,
    connection: SnapshotConnection,
    main_file: FrozenSourceMainFile,
    retained_backup_destination: Option<RetainedBackupDestinationIdentity>,
    residuals: FrozenSourceResiduals,
}

impl ExistingSqliteSnapshotSource {
    /// Open an existing, clean-residual SQLite database without initializing or repairing it.
    ///
    /// `path` must be absolute; the caller must validate and guard its ancestor chain.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        ensure_snapshot_sqlite_runtime()?;
        ensure_absolute_path(path, "snapshot source")?;
        let main_file = FrozenSourceMainFile::open(path)?;
        let residuals = FrozenSourceResiduals::capture(path)?;
        verify_snapshot_source(
            &main_file,
            &residuals,
            path,
            "snapshot source before SQLite open",
        )?;

        let uri = immutable_source_uri(path)?;
        let operation = (|| {
            let flags = ffi::SQLITE_OPEN_READONLY
                | ffi::SQLITE_OPEN_URI
                | ffi::SQLITE_OPEN_FULLMUTEX
                | ffi::SQLITE_OPEN_PRIVATECACHE
                | ffi::SQLITE_OPEN_NOFOLLOW;
            let connection = SnapshotConnection::open(uri.as_c_str(), flags, "snapshot source")?;
            configure_snapshot_source_guardrails(&connection)?;

            let readonly = unsafe {
                ffi::sqlite3_db_readonly(connection.as_raw(), SQLITE_MAIN_SCHEMA.as_ptr().cast())
            };
            if readonly != 1 {
                bail!(
                    "snapshot source {:?} did not open as a read-only main database (sqlite3_db_readonly returned {readonly})",
                    path
                );
            }
            validate_sqlite_schema(&connection)?;
            Ok(connection)
        })();

        let connection = combine_with_postcondition(
            operation,
            verify_snapshot_source(
                &main_file,
                &residuals,
                path,
                "snapshot source after SQLite open",
            ),
            "snapshot source identity postcondition failed after SQLite open",
        )?;
        Ok(Self {
            path: path.to_path_buf(),
            connection,
            main_file,
            retained_backup_destination: None,
            residuals,
        })
    }

    /// Consume this guarded source and attest the fixed managed-reader V1 boundary.
    pub fn into_managed_reader(
        self,
        policy: ManagedSnapshotPolicy,
    ) -> Result<ManagedSqliteSnapshotReader> {
        self.verify_source("managed SQLite source before readiness setup")?;
        let setup = match policy {
            ManagedSnapshotPolicy::V1
            | ManagedSnapshotPolicy::EpisodeV1
            | ManagedSnapshotPolicy::SingleGraphV1
            | ManagedSnapshotPolicy::ConcernV1
            | ManagedSnapshotPolicy::TouchstonesV1 => {
                configure_managed_snapshot_reader(&self.connection)
            }
        };
        combine_with_postcondition(
            setup,
            self.verify_source("managed SQLite source after readiness setup"),
            "managed SQLite source identity postcondition failed after readiness setup",
        )?;

        let Self {
            path,
            connection,
            main_file,
            retained_backup_destination,
            residuals,
        } = self;
        Ok(ManagedSqliteSnapshotReader {
            path,
            connection: Some(connection),
            main_file,
            retained_backup_destination,
            residuals,
            policy,
            state: ManagedReaderState::Ready,
            transaction_active: false,
            catalog: None,
            census_progress_callbacks_used: 0,
            physical_census: None,
            _thread_bound: PhantomData,
        })
    }

    /// Consume an uninspected source into the bounded V1 catalog fence.
    ///
    /// This is the only public route to this light evidence.  It deliberately
    /// creates its private reader in `Ready` state, so safe callers cannot
    /// first inspect a catalog/census and then launder that reader into a
    /// routine-generation fence.
    ///
    /// ```compile_fail
    /// use cozo::{ExistingSqliteSnapshotSource, ManagedCatalogFencePolicy, ManagedSqlitePrimaryIndexCatalogV1};
    /// use miette::Result;
    /// use std::path::Path;
    /// fn escape(path: &Path) -> Result<&'static ManagedSqlitePrimaryIndexCatalogV1> {
    ///     let mut escaped = None;
    ///     let _ = ExistingSqliteSnapshotSource::open(path)?.into_closed_catalog_fence_v1(
    ///         ManagedCatalogFencePolicy::V1,
    ///         |planner| { escaped = Some(planner.catalog()); Ok(()) },
    ///     )?;
    ///     Ok(escaped.unwrap())
    /// }
    /// ```
    pub fn into_closed_catalog_fence_v1<F>(
        self,
        selector: ManagedCatalogFencePolicy,
        plan: F,
    ) -> Result<ManagedSqliteClosedCatalogFenceV1>
    where
        F: for<'reader> FnOnce(&mut ManagedSqliteCatalogAssertionPlannerV1<'reader>) -> Result<()>,
    {
        let reader = self.into_managed_reader(selector.snapshot_policy())?;
        reader.close_catalog_fence_v1(selector, plan)
    }

    #[cfg(test)]
    fn attach_in_memory_for_managed_reader_test(&self) -> Result<()> {
        let previous_limit =
            unsafe { ffi::sqlite3_limit(self.connection.as_raw(), ffi::SQLITE_LIMIT_ATTACHED, 1) };
        let query_only_off = unsafe {
            ffi::sqlite3_exec(
                self.connection.as_raw(),
                MANAGED_TEST_QUERY_ONLY_OFF_SQL.as_ptr().cast(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if query_only_off != ffi::SQLITE_OK {
            unsafe {
                ffi::sqlite3_limit(
                    self.connection.as_raw(),
                    ffi::SQLITE_LIMIT_ATTACHED,
                    previous_limit,
                );
            }
            bail!("test-only query-only override failed with code {query_only_off}");
        }

        let attach = unsafe {
            ffi::sqlite3_exec(
                self.connection.as_raw(),
                MANAGED_TEST_ATTACH_SQL.as_ptr().cast(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        let query_only_on = unsafe {
            ffi::sqlite3_exec(
                self.connection.as_raw(),
                MANAGED_QUERY_ONLY_SQL.as_ptr().cast(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        unsafe {
            ffi::sqlite3_limit(
                self.connection.as_raw(),
                ffi::SQLITE_LIMIT_ATTACHED,
                previous_limit,
            );
        }
        if attach != ffi::SQLITE_OK {
            bail!("test-only SQLite attachment failed with code {attach}");
        }
        if query_only_on != ffi::SQLITE_OK {
            bail!("test-only query-only restoration failed with code {query_only_on}");
        }
        Ok(())
    }

    /// Copy the source's complete SQLite image into a newly created destination.
    ///
    /// Under a caller-held cooperative lease with guarded ancestor directories, `create_new`
    /// provides no-clobber publication of the destination. A held descriptor is bound to the
    /// pathname before and after SQLite opens it without `SQLITE_OPEN_CREATE`, throughout the
    /// native online-backup operation, and through final sync. A successful return leaves that
    /// exact regular file at mode 0600 with one link and no destination sidecars.
    /// This is process-synchronous staging, not crash-safe durable publication: this layer does
    /// not fsync the parent directory, publish recovery metadata, or classify crash residue.
    ///
    /// On failure, cleanup makes one attempt to remove the main file only when its descriptor and
    /// pathname still prove the exact identity and policy created here. A missing, replaced,
    /// hardlinked, or otherwise unverifiable main file is preserved and reported. Sidecars are
    /// never assumed to be ours merely because they were absent before creation: any observed
    /// sidecar is preserved and reported. Same-authority rename ABA between checks is outside this
    /// path-based boundary; a hostile host must provide an fd-backed VFS or OS snapshot. If the
    /// first descriptor identity cannot be established after \`create_new\`, the unverifiable path
    /// is likewise preserved and reported rather than guessed to be ours.
    pub fn backup_to_new(&self, destination: impl AsRef<Path>) -> Result<()> {
        self.backup_to_new_impl(destination.as_ref(), false, |_, _| Ok(()))
    }

    /// Copy into a new destination and return it as an already guarded snapshot source.
    ///
    /// This retains continuous destination identity custody across native backup and the
    /// returned immutable reader. While the create-new guard is still armed, Mnestic duplicates
    /// its unexposed read-write descriptor, opens the ordinary read-only frozen-main descriptor
    /// and SQLite immutable connection, and verifies all three views against the same pathname.
    /// The duplicate is the only descriptor proven to descend from the exact `create_new`
    /// lineage; retaining it lets the returned source continue checking that provenance and the
    /// current pathname alongside the independently opened read-only descriptor. It costs one
    /// additional file descriptor and retains unexposed write capability, but no mutation API is
    /// provided. Cleanup remains armed until connection guardrails, schema validation, source
    /// verification, and the final no-sidecar handoff all succeed.
    ///
    /// The create-new pathname is operation-owned staging, not crash-safe durable publication.
    /// This layer does not fsync the parent directory, publish a recovery marker, or classify
    /// residue left by a process or host crash. An outer managed-snapshot publisher must own those
    /// durability, recovery, and residue-classification duties.
    ///
    /// SQLite still opens the database by pathname: this handoff does not provide an fd-backed
    /// VFS, close the same-authority rename-ABA gap, or replace the caller's guarded ancestors and
    /// cooperative lease. The destination must be absolute and absent.
    pub fn backup_to_new_snapshot_source_v1(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<ExistingSqliteSnapshotSource> {
        self.backup_to_new_operation(
            destination.as_ref(),
            false,
            |_, _| Ok(()),
            |created| Self::finish_backup_snapshot_source_v1(created, |_, _| Ok(())),
        )
    }

    /// Build one closed hostile-catalog fixture in a newly owned destination.
    ///
    /// The source must first pass the fixed managed catalog and physical census.
    /// The selected fixture supplies no caller-controlled bytes, SQL, names, ids,
    /// or callback. This hook exists only for cross-crate integration tests.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn backup_to_new_with_catalog_fixture_v1_for_tests(
        &self,
        destination: impl AsRef<Path>,
        fixture: ManagedCatalogFixtureV1,
    ) -> Result<()> {
        self.preflight_catalog_fixture_source_v1()?;
        self.backup_to_new_impl(
            destination.as_ref(),
            false,
            move |connection, destination| {
                apply_catalog_fixture_v1(connection, destination, fixture)
            },
        )
    }

    #[cfg(test)]
    fn backup_to_new_failing_after_create(&self, destination: &Path) -> Result<()> {
        self.backup_to_new_impl(destination, true, |_, _| Ok(()))
    }

    #[cfg(test)]
    fn backup_to_new_with_after_native_backup_hook<F>(
        &self,
        destination: &Path,
        hook: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        self.backup_to_new_impl(destination, false, |_, _| hook())
    }

    #[cfg(test)]
    fn backup_to_new_snapshot_source_failing_after_create(
        &self,
        destination: &Path,
    ) -> Result<ExistingSqliteSnapshotSource> {
        self.backup_to_new_operation(
            destination,
            true,
            |_, _| Ok(()),
            |created| Self::finish_backup_snapshot_source_v1(created, |_, _| Ok(())),
        )
    }

    #[cfg(test)]
    fn backup_to_new_snapshot_source_with_after_native_backup_hook<F>(
        &self,
        destination: &Path,
        hook: F,
    ) -> Result<ExistingSqliteSnapshotSource>
    where
        F: FnOnce() -> Result<()>,
    {
        self.backup_to_new_operation(
            destination,
            false,
            |_, _| hook(),
            |created| Self::finish_backup_snapshot_source_v1(created, |_, _| Ok(())),
        )
    }

    #[cfg(test)]
    fn backup_to_new_snapshot_source_with_ready_hook<F>(
        &self,
        destination: &Path,
        hook: F,
    ) -> Result<ExistingSqliteSnapshotSource>
    where
        F: FnOnce(&ExistingSqliteSnapshotSource, &IncompleteDestination) -> Result<()>,
    {
        self.backup_to_new_operation(
            destination,
            false,
            |_, _| Ok(()),
            |created| Self::finish_backup_snapshot_source_v1(created, hook),
        )
    }

    fn backup_to_new_impl<F>(
        &self,
        destination: &Path,
        fail_after_create: bool,
        after_native_backup: F,
    ) -> Result<()>
    where
        F: FnOnce(&SnapshotConnection, &IncompleteDestination) -> Result<()>,
    {
        self.backup_to_new_operation(
            destination,
            fail_after_create,
            after_native_backup,
            IncompleteDestination::publish,
        )
    }

    fn backup_to_new_operation<F, G, T>(
        &self,
        destination: &Path,
        fail_after_create: bool,
        after_native_backup: F,
        finish: G,
    ) -> Result<T>
    where
        F: FnOnce(&SnapshotConnection, &IncompleteDestination) -> Result<()>,
        G: FnOnce(&mut IncompleteDestination) -> Result<T>,
    {
        self.verify_source("snapshot source before backup")?;

        let operation =
            self.backup_to_new_inner(destination, fail_after_create, after_native_backup, finish);
        match operation {
            // The inner success path verifies the source while it still owns
            // the incomplete destination, then disarms cleanup. A later check
            // here could turn success into Err after publication.
            Ok(value) => Ok(value),
            Err(error) => combine_with_postcondition(
                Err(error),
                self.verify_source("snapshot source after failed backup"),
                "snapshot source identity postcondition failed after failed backup",
            ),
        }
    }

    fn backup_to_new_inner<F, G, T>(
        &self,
        destination: &Path,
        fail_after_create: bool,
        after_native_backup: F,
        finish: G,
    ) -> Result<T>
    where
        F: FnOnce(&SnapshotConnection, &IncompleteDestination) -> Result<()>,
        G: FnOnce(&mut IncompleteDestination) -> Result<T>,
    {
        ensure_absolute_path(destination, "snapshot destination")?;
        ensure_sidecars_absent(destination, "snapshot destination before create")?;

        let mut created = IncompleteDestination::create(destination)?;
        let backup_operation = (|| {
            if fail_after_create {
                bail!("injected snapshot backup failure after destination creation");
            }

            let destination_name = path_to_cstring(destination)?;
            let flags = ffi::SQLITE_OPEN_READWRITE
                | ffi::SQLITE_OPEN_FULLMUTEX
                | ffi::SQLITE_OPEN_PRIVATECACHE
                | ffi::SQLITE_OPEN_NOFOLLOW;
            created.verify_owned_path("snapshot destination immediately before SQLite open")?;
            let destination_connection = SnapshotConnection::open(
                destination_name.as_c_str(),
                flags,
                "snapshot destination",
            )?;
            created.verify_owned_path("snapshot destination immediately after SQLite open")?;

            let connection_operation = (|| {
                let destination_readonly = unsafe {
                    ffi::sqlite3_db_readonly(
                        destination_connection.as_raw(),
                        SQLITE_MAIN_SCHEMA.as_ptr().cast(),
                    )
                };
                if destination_readonly != 0 {
                    bail!(
                        "snapshot destination {:?} did not open read-write without create (sqlite3_db_readonly returned {destination_readonly})",
                        destination
                    );
                }

                created
                    .verify_owned_path("snapshot destination immediately before native backup")?;
                let backup_result = native_backup(&destination_connection, &self.connection);
                combine_with_postcondition(
                    backup_result,
                    created.verify_owned_path("snapshot destination after native backup"),
                    "snapshot destination identity postcondition failed after native backup",
                )?;

                let hook_result = after_native_backup(&destination_connection, &created);
                combine_with_postcondition(
                    hook_result,
                    created.verify_owned_path("snapshot destination after fixed backup hook"),
                    "snapshot destination identity postcondition failed after fixed backup hook",
                )
            })();
            let connection_operation = combine_with_postcondition(
                connection_operation,
                destination_connection.close("snapshot destination"),
                "snapshot destination close failed after backup operation",
            );
            combine_with_postcondition(
                connection_operation,
                created.verify_owned_path("snapshot destination after SQLite connection close"),
                "snapshot destination identity postcondition failed after SQLite connection close",
            )?;

            let sync_result = created.sync_all();
            combine_with_postcondition(
                sync_result,
                created.verify_owned_path("snapshot destination immediately after sync"),
                "snapshot destination identity postcondition failed after sync",
            )?;
            ensure_sidecars_absent(destination, "snapshot destination after backup")?;
            Ok(())
        })();
        let operation = combine_with_postcondition(
            backup_operation,
            self.verify_source("snapshot source after native backup"),
            "snapshot source identity postcondition failed after native backup",
        )
        .and_then(|()| finish(&mut created));

        match operation {
            Ok(value) => Ok(value),
            Err(operation_error) => match created.cleanup() {
                Ok(()) => Err(operation_error),
                Err(cleanup_error) => Err(miette!(
                    "{operation_error}; cleanup of incomplete snapshot destination also failed: {cleanup_error}"
                )),
            },
        }
    }

    fn finish_backup_snapshot_source_v1<F>(
        created: &mut IncompleteDestination,
        ready_hook: F,
    ) -> Result<ExistingSqliteSnapshotSource>
    where
        F: FnOnce(&ExistingSqliteSnapshotSource, &IncompleteDestination) -> Result<()>,
    {
        let source = Self::open_incomplete_backup_as_snapshot_source_v1(created)?;
        let completion = (|| {
            let hook = ready_hook(&source, created);
            let source_check =
                source.verify_source("private backup snapshot source after fixed ready hook");
            let guard_check = created
                .verify_owned_path("snapshot destination after private backup source ready hook");
            let completion = combine_with_postcondition(
                hook,
                source_check,
                "private backup snapshot source verification also failed after ready hook",
            );
            combine_with_postcondition(
                completion,
                guard_check,
                "snapshot destination guard verification also failed after ready hook",
            )?;
            created.publish()
        })();

        match completion {
            Ok(()) => Ok(source),
            Err(error) => {
                let closed = source.close_after_failed_backup_handoff(Err(error));
                match closed {
                    Ok(()) => unreachable!("a failed backup handoff cannot close as success"),
                    Err(error) => Err(error),
                }
            }
        }
    }

    fn open_incomplete_backup_as_snapshot_source_v1(
        created: &IncompleteDestination,
    ) -> Result<ExistingSqliteSnapshotSource> {
        ensure_snapshot_sqlite_runtime()?;
        created
            .verify_owned_path("snapshot destination before private backup source construction")?;
        let retained_backup_destination = created.duplicate_retained_backup_identity()?;
        let main_file = FrozenSourceMainFile::open(&created.path)?;

        #[cfg(unix)]
        if main_file.identity != retained_backup_destination.identity {
            bail!(
                "private backup read-only and retained custody descriptors named different files: {:?}",
                created.path
            );
        }

        created.verify_owned_path(
            "snapshot destination after opening private backup read-only descriptor",
        )?;
        let residuals = FrozenSourceResiduals::capture(&created.path)?;
        verify_snapshot_source_with_retained_backup(
            &main_file,
            Some(&retained_backup_destination),
            &residuals,
            &created.path,
            "private backup snapshot source before SQLite open",
        )?;
        created.verify_owned_path(
            "snapshot destination immediately before private backup SQLite open",
        )?;

        let uri = immutable_source_uri(&created.path)?;
        let flags = ffi::SQLITE_OPEN_READONLY
            | ffi::SQLITE_OPEN_URI
            | ffi::SQLITE_OPEN_FULLMUTEX
            | ffi::SQLITE_OPEN_PRIVATECACHE
            | ffi::SQLITE_OPEN_NOFOLLOW;
        let connection =
            SnapshotConnection::open(uri.as_c_str(), flags, "private backup snapshot source")?;
        let setup = (|| {
            configure_snapshot_source_guardrails(&connection)?;
            let readonly = unsafe {
                ffi::sqlite3_db_readonly(connection.as_raw(), SQLITE_MAIN_SCHEMA.as_ptr().cast())
            };
            if readonly != 1 {
                bail!(
                    "private backup snapshot source {:?} did not open as a read-only main database (sqlite3_db_readonly returned {readonly})",
                    created.path
                );
            }
            validate_sqlite_schema(&connection)?;
            let source_check = verify_snapshot_source_with_retained_backup(
                &main_file,
                Some(&retained_backup_destination),
                &residuals,
                &created.path,
                "private backup snapshot source after SQLite open",
            );
            combine_with_postcondition(
                source_check,
                created.verify_owned_path("snapshot destination after private backup SQLite open"),
                "snapshot destination guard verification also failed after private backup SQLite open",
            )
        })();

        if let Err(error) = setup {
            let closed = combine_with_postcondition(
                Err(error),
                connection.close("private backup snapshot source after failed setup"),
                "private backup snapshot source strict close also failed after setup failure",
            );
            return match closed {
                Ok(()) => unreachable!("a failed backup-source setup cannot close as success"),
                Err(error) => Err(error),
            };
        }

        Ok(Self {
            path: created.path.clone(),
            connection,
            main_file,
            retained_backup_destination: Some(retained_backup_destination),
            residuals,
        })
    }

    fn close_after_failed_backup_handoff(self, operation: Result<()>) -> Result<()> {
        let source_check = self
            .verify_source("private backup snapshot source before cleanup after failed handoff");
        let Self {
            connection,
            path: _path,
            main_file: _main_file,
            retained_backup_destination: _retained_backup_destination,
            residuals: _residuals,
        } = self;
        let completion = combine_with_postcondition(
            operation,
            source_check,
            "private backup snapshot source verification also failed during handoff cleanup",
        );
        let close = connection.close("private backup snapshot source after failed handoff");
        #[cfg(test)]
        let close = combine_with_postcondition(
            close,
            if MANAGED_BACKUP_HANDOFF_CLOSE_TEST_FAILURE.with(|failure| failure.replace(false)) {
                Err(miette!(
                    "injected private backup snapshot source strict-close failure"
                ))
            } else {
                Ok(())
            },
            "injected private backup snapshot source strict-close check also failed",
        );
        combine_with_postcondition(
            completion,
            close,
            "private backup snapshot source strict close also failed during handoff cleanup",
        )
    }

    fn verify_source(&self, role: &str) -> Result<()> {
        verify_snapshot_source_with_retained_backup(
            &self.main_file,
            self.retained_backup_destination.as_ref(),
            &self.residuals,
            &self.path,
            role,
        )
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn preflight_catalog_fixture_source_v1(&self) -> Result<()> {
        self.verify_source("catalog fixture source before managed preflight")?;
        let inspection = (|| {
            configure_managed_snapshot_reader(&self.connection)?;
            managed_set_limit(
                &self.connection,
                ffi::SQLITE_LIMIT_LENGTH,
                MANAGED_SQLITE_CATALOG_LENGTH_LIMIT,
                "catalog fixture source row length",
            )?;
            let main_file_bytes = self.main_file.len()?;
            managed_admit_file_bytes(main_file_bytes)?;
            managed_with_progress_budget(
                self.connection.as_raw(),
                ManagedProgressBudget::default(),
                || {
                    managed_catalog_fixture_read_transaction(&self.connection, || {
                        let catalog = scan_managed_catalog_v1(&self.connection)?;
                        scan_managed_physical_census_v1(
                            &self.connection,
                            &catalog,
                            main_file_bytes,
                        )?;
                        Ok(())
                    })
                },
            )?;
            Ok(())
        })();
        combine_with_postcondition(
            inspection,
            self.verify_source("catalog fixture source after managed preflight"),
            "catalog fixture source identity failed after managed preflight",
        )
    }
}

impl ManagedSqliteSnapshotReader {
    /// Inspect primary-index-visible id-zero rows under one pinned read snapshot.
    ///
    /// Exact historical wire syntax is classified and projected into bounded,
    /// immutable DTOs. This does not prove catalog completeness or apply
    /// Mneme's static F7 oracle. The borrow cannot outlive this reader, and a
    /// repeated successful call returns the cached observation without rescanning.
    pub fn inspect_primary_index_catalog_v1(
        &mut self,
    ) -> Result<&ManagedSqlitePrimaryIndexCatalogV1> {
        self.inspect_primary_index_catalog_with_budget_v1(ManagedProgressBudget::default())
    }

    fn inspect_primary_index_catalog_with_budget_v1(
        &mut self,
        budget: ManagedProgressBudget,
    ) -> Result<&ManagedSqlitePrimaryIndexCatalogV1> {
        match self.state {
            ManagedReaderState::Observed | ManagedReaderState::Audited => {
                if let Err(error) = self.require_read_transaction("cached catalog observation") {
                    self.state = ManagedReaderState::Poisoned;
                    return Err(error);
                }
                return self.catalog.as_ref().ok_or_else(|| {
                    miette!("managed SQLite catalog cache violated its internal state")
                });
            }
            ManagedReaderState::Poisoned => {
                bail!("managed SQLite catalog reader is poisoned");
            }
            ManagedReaderState::Inspecting => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite catalog inspection was interrupted");
            }
            ManagedReaderState::Auditing => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite physical census was interrupted");
            }
            ManagedReaderState::Asserting => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite relation assertions were interrupted");
            }
            ManagedReaderState::Visiting => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite record visitation was interrupted");
            }
            ManagedReaderState::Ready => {}
        }

        self.state = ManagedReaderState::Inspecting;
        let connection = self
            .connection
            .as_ref()
            .map(SnapshotConnection::as_raw)
            .ok_or_else(|| miette!("managed SQLite snapshot reader was already closed"));
        let observation = connection.and_then(|connection| {
            managed_with_progress_budget(connection, budget, || self.inspect_catalog_uncached_v1())
        });
        match observation {
            Ok((observation, callbacks_used)) => {
                self.catalog = Some(observation);
                self.census_progress_callbacks_used = callbacks_used;
                self.state = ManagedReaderState::Observed;
                self.catalog.as_ref().ok_or_else(|| {
                    miette!("managed SQLite catalog cache violated its internal state")
                })
            }
            Err(error) => {
                self.state = ManagedReaderState::Poisoned;
                Err(error)
            }
        }
    }

    /// Inspect the catalog and the complete physical table/index census together.
    ///
    /// Calling this directly from a fresh reader performs both phases under the
    /// same pinned read transaction and returns one nontransferable composite
    /// borrow. If the catalog was observed previously and that borrow has ended,
    /// the census continues on its still-pinned transaction. A repeated success
    /// returns both caches without rerunning SQLite work.
    pub fn inspect_physical_census_v1(&mut self) -> Result<ManagedSqliteSnapshotCensusV1<'_>> {
        self.inspect_physical_census_with_budget_v1(ManagedProgressBudget::default())
    }

    fn inspect_physical_census_with_budget_v1(
        &mut self,
        budget: ManagedProgressBudget,
    ) -> Result<ManagedSqliteSnapshotCensusV1<'_>> {
        match self.state {
            ManagedReaderState::Audited => {
                if let Err(error) = self.require_read_transaction("cached physical census") {
                    self.state = ManagedReaderState::Poisoned;
                    return Err(error);
                }
                return self.physical_census_view();
            }
            ManagedReaderState::Poisoned => {
                bail!("managed SQLite physical census reader is poisoned");
            }
            ManagedReaderState::Inspecting => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite catalog inspection was interrupted");
            }
            ManagedReaderState::Auditing => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite physical census was interrupted");
            }
            ManagedReaderState::Asserting => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite relation assertions were interrupted");
            }
            ManagedReaderState::Visiting => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite record visitation was interrupted");
            }
            ManagedReaderState::Ready | ManagedReaderState::Observed => {}
        }

        let needs_catalog = self.state == ManagedReaderState::Ready;
        self.state = ManagedReaderState::Auditing;
        let audit = (|| {
            let callbacks_before = if needs_catalog {
                0
            } else {
                self.census_progress_callbacks_used
            };
            let remaining_budget = budget.after_callbacks(callbacks_before)?;
            let connection = self
                .connection
                .as_ref()
                .map(SnapshotConnection::as_raw)
                .ok_or_else(|| miette!("managed SQLite snapshot reader was already closed"))?;
            let (mut census, callbacks_now) =
                managed_with_progress_budget(connection, remaining_budget, || {
                    let main_file_bytes = self.main_file.len()?;
                    managed_admit_file_bytes(main_file_bytes)?;
                    if needs_catalog {
                        self.catalog = Some(self.inspect_catalog_uncached_v1()?);
                    } else {
                        self.require_read_transaction("physical census entry")?;
                    }
                    self.inspect_physical_uncached_v1(main_file_bytes)
                })?;
            let callbacks_used = callbacks_before
                .checked_add(callbacks_now)
                .ok_or_else(|| miette!("managed SQLite census callback count overflowed"))?;
            census.progress_callbacks_used = callbacks_used;
            self.census_progress_callbacks_used = callbacks_used;
            Ok(census)
        })();
        match audit {
            Ok(census) => {
                self.physical_census = Some(census);
                self.state = ManagedReaderState::Audited;
                self.physical_census_view()
            }
            Err(error) => {
                self.state = ManagedReaderState::Poisoned;
                Err(error)
            }
        }
    }

    fn physical_census_view(&self) -> Result<ManagedSqliteSnapshotCensusV1<'_>> {
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| miette!("managed SQLite catalog cache violated its internal state"))?;
        let physical = self
            .physical_census
            .as_ref()
            .ok_or_else(|| miette!("managed SQLite physical cache violated its internal state"))?;
        Ok(ManagedSqliteSnapshotCensusV1 {
            catalog,
            physical,
            _thread_bound: PhantomData,
        })
    }

    fn run_relation_assertions_v1<F>(
        &mut self,
        plan: F,
    ) -> Result<ManagedSqliteRelationAssertionEvidenceV1>
    where
        F: for<'reader> FnOnce(&mut ManagedSqliteRelationAssertionPlannerV1<'reader>) -> Result<()>,
    {
        self.run_relation_assertions_with_budget_v1(
            ManagedProgressBudget::relation_assertions_v1(),
            plan,
        )
    }

    fn run_relation_assertions_with_budget_v1<F>(
        &mut self,
        budget: ManagedProgressBudget,
        plan: F,
    ) -> Result<ManagedSqliteRelationAssertionEvidenceV1>
    where
        F: for<'reader> FnOnce(&mut ManagedSqliteRelationAssertionPlannerV1<'reader>) -> Result<()>,
    {
        if budget.phase != ManagedProgressPhase::RelationAssertions {
            self.state = ManagedReaderState::Poisoned;
            bail!("managed SQLite relation assertion runner received the wrong progress phase");
        }
        if let Err(error) = self.inspect_physical_census_v1().map(|_| ()) {
            self.state = ManagedReaderState::Poisoned;
            return Err(error);
        }
        if let Err(error) = self.require_read_transaction("relation assertion runner entry") {
            self.state = ManagedReaderState::Poisoned;
            return Err(error);
        }

        self.state = ManagedReaderState::Asserting;
        let mut progress = ManagedProgressState {
            remaining_callbacks: budget.max_callbacks,
            callbacks_used: 0,
            interrupted: false,
        };
        let connection_raw = match self.connection.as_ref() {
            Some(connection) => connection.as_raw(),
            None => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite relation assertion runner lost its connection");
            }
        };
        let handler = match ManagedProgressHandler::install(connection_raw, &mut progress, budget) {
            Ok(handler) => handler,
            Err(error) => {
                self.state = ManagedReaderState::Poisoned;
                return Err(error);
            }
        };

        let assertion_result = (|| {
            let catalog = self.catalog.as_ref().ok_or_else(|| {
                miette!("managed SQLite relation assertions lost the cached catalog")
            })?;
            let physical = self.physical_census.as_ref().ok_or_else(|| {
                miette!("managed SQLite relation assertions lost the physical census")
            })?;
            let connection = self
                .connection
                .as_ref()
                .ok_or_else(|| miette!("managed SQLite relation assertions lost the connection"))?;
            let mut planner =
                ManagedSqliteRelationAssertionPlannerV1::new(catalog, physical, connection)?;
            let callback_result = plan(&mut planner);
            planner.finish(callback_result)
        })();

        // The handler must unregister before its borrowed state is inspected.
        drop(handler);
        let assertion_result = if progress.interrupted {
            Err(miette!("{}", budget.phase.exhausted_message()))
        } else {
            assertion_result.map(|mut evidence| {
                evidence.progress_callbacks_used = progress.callbacks_used;
                evidence
            })
        };
        let postcondition = self.require_read_transaction("relation assertion runner completion");
        let result = combine_with_postcondition(
            assertion_result,
            postcondition,
            "managed SQLite relation assertion READ-state postcondition failed",
        );
        self.state = if result.is_ok() {
            ManagedReaderState::Audited
        } else {
            ManagedReaderState::Poisoned
        };
        result
    }

    fn validate_record_visit_selection_v1(
        &self,
        relation_ids: [u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1],
    ) -> Result<ManagedRecordVisitSelectionV1> {
        let physical = self.physical_census.as_ref().ok_or_else(|| {
            miette!("managed SQLite record-visit selection lost the physical census")
        })?;
        let mut seen = BTreeSet::new();
        let mut relation_row_counts = [0_u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1];
        let mut total_row_count = 0_u64;
        for (index, relation_id) in relation_ids.iter().copied().enumerate() {
            let relation = RelationId::try_raw_decode(&relation_id.to_be_bytes())
                .map_err(|_| miette!("managed SQLite record-visit relation id was invalid"))?;
            if relation == RelationId::SYSTEM {
                bail!("managed SQLite record-visit relation id was zero");
            }
            if !seen.insert(relation_id) {
                bail!("managed SQLite record-visit relation id was repeated");
            }
            let row_count = physical
                .relation_row_counts
                .iter()
                .find(|row| row.relation_id == relation_id)
                .map(|row| row.row_count)
                .ok_or_else(|| {
                    miette!("managed SQLite record-visit relation id was not admitted")
                })?;
            relation_row_counts[index] = row_count;
            total_row_count = total_row_count
                .checked_add(row_count)
                .filter(|total| *total <= MANAGED_RECORD_VISIT_TOTAL_ROW_LIMIT_V1)
                .ok_or_else(|| {
                    miette!("managed SQLite record-visit total row boundary was exceeded")
                })?;
        }
        Ok(ManagedRecordVisitSelectionV1 {
            relation_ids,
            relation_row_counts,
            total_row_count,
        })
    }

    fn run_record_visit_v1<S, V>(
        &mut self,
        sink: S,
        relation_ids: [u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1],
        visit: V,
    ) -> Result<(ManagedSqliteRecordVisitEvidenceV1, S)>
    where
        V: for<'row> FnMut(&mut S, ManagedSqliteRecordVisitEventV1<'row>) -> Result<()>,
    {
        self.run_record_visit_with_budget_v1(
            ManagedProgressBudget::default(),
            sink,
            relation_ids,
            visit,
        )
    }

    fn run_record_visit_with_budget_v1<S, V>(
        &mut self,
        budget: ManagedProgressBudget,
        sink: S,
        relation_ids: [u64; MANAGED_RECORD_VISIT_RELATION_COUNT_V1],
        mut visit: V,
    ) -> Result<(ManagedSqliteRecordVisitEvidenceV1, S)>
    where
        V: for<'row> FnMut(&mut S, ManagedSqliteRecordVisitEventV1<'row>) -> Result<()>,
    {
        if budget.phase != ManagedProgressPhase::Census {
            self.state = ManagedReaderState::Poisoned;
            bail!("managed SQLite record visitor received the wrong progress phase");
        }
        if self.state != ManagedReaderState::Audited {
            self.state = ManagedReaderState::Poisoned;
            bail!("managed SQLite record visitor requires a completed physical census");
        }
        if let Err(error) = self.require_read_transaction("record visitor entry") {
            self.state = ManagedReaderState::Poisoned;
            return Err(error);
        }

        self.state = ManagedReaderState::Visiting;
        let operation = (|| {
            let selection = self.validate_record_visit_selection_v1(relation_ids)?;
            let remaining_budget = budget.after_callbacks(self.census_progress_callbacks_used)?;
            let mut progress = ManagedProgressState {
                remaining_callbacks: remaining_budget.max_callbacks,
                callbacks_used: 0,
                interrupted: false,
            };
            let scan = {
                let connection = self
                    .connection
                    .as_ref()
                    .ok_or_else(|| miette!("managed SQLite record visitor lost its connection"))?;
                let handler = ManagedProgressHandler::install(
                    connection.as_raw(),
                    &mut progress,
                    remaining_budget,
                )?;
                let scan = scan_managed_record_visit_v1(connection, selection, sink, &mut visit);
                // Unregister before reading the borrowed stack state or checking READ state.
                drop(handler);
                scan
            };

            let cumulative = self
                .census_progress_callbacks_used
                .checked_add(progress.callbacks_used)
                .ok_or_else(|| miette!("managed SQLite census callback count overflowed"));
            if let Ok(callbacks) = cumulative {
                self.census_progress_callbacks_used = callbacks;
            }
            let progress_result = if progress.interrupted {
                Err(miette!("{}", budget.phase.exhausted_message()))
            } else {
                cumulative.map(|_| ())
            };
            let scan = combine_with_postcondition(
                scan,
                progress_result,
                "managed SQLite record-visit progress postcondition failed",
            );
            let read = self.require_read_transaction("record visitor completion");
            #[cfg(test)]
            let read = combine_with_postcondition(
                read,
                if MANAGED_RECORD_VISIT_READ_FAILURE.with(|failure| failure.replace(false)) {
                    Err(miette!(
                        "injected managed SQLite record-visit READ-state failure"
                    ))
                } else {
                    Ok(())
                },
                "managed SQLite injected record-visit READ-state failure",
            );
            combine_with_postcondition(
                scan,
                read,
                "managed SQLite record-visit READ-state postcondition failed",
            )?
            .finish(self.census_progress_callbacks_used)
        })();
        self.state = if operation.is_ok() {
            ManagedReaderState::Audited
        } else {
            ManagedReaderState::Poisoned
        };
        operation
    }

    fn inspect_catalog_uncached_v1(&mut self) -> Result<ManagedSqlitePrimaryIndexCatalogV1> {
        let connection = self
            .connection
            .as_ref()
            .ok_or_else(|| miette!("managed SQLite snapshot reader was already closed"))?;

        // The exact schema gate ran during into_managed_reader. Only now may a
        // catalog-sized SQLite value reach the fixed query.
        managed_set_limit(
            connection,
            ffi::SQLITE_LIMIT_LENGTH,
            MANAGED_SQLITE_CATALOG_LENGTH_LIMIT,
            "catalog row length",
        )?;
        let begin = managed_exec_fixed(
            connection,
            MANAGED_BEGIN_READ_SQL,
            "catalog read transaction",
        );
        self.transaction_active = unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } == 0;
        begin?;
        if !self.transaction_active {
            bail!("managed SQLite catalog transaction did not become active");
        }

        // BEGIN DEFERRED alone has no snapshot. This fixed main-schema read is
        // deliberately immediate, and SQLITE_TXN_READ is the authoritative pin.
        managed_boolean_check(connection, SQLITE_SCHEMA_PROBE, "snapshot-pinning read")?;
        managed_require_read_transaction(connection, "snapshot-pinning read")?;

        let observation = scan_managed_catalog_for(connection, self.policy)?;
        managed_require_read_transaction(connection, "catalog inspection")?;
        Ok(observation)
    }

    fn inspect_physical_uncached_v1(
        &self,
        main_file_bytes: u64,
    ) -> Result<ManagedSqlitePhysicalCensusV1> {
        let connection = self
            .connection
            .as_ref()
            .ok_or_else(|| miette!("managed SQLite snapshot reader was already closed"))?;
        let catalog = self
            .catalog
            .as_ref()
            .ok_or_else(|| miette!("managed SQLite physical census requires its cached catalog"))?;
        scan_managed_physical_census_v1(connection, catalog, main_file_bytes)
    }

    fn require_read_transaction(&self, phase: &'static str) -> Result<()> {
        let connection = self
            .connection
            .as_ref()
            .ok_or_else(|| miette!("managed SQLite snapshot reader was already closed"))?;
        managed_require_read_transaction(connection, phase)
    }

    // Kept private so only ExistingSqliteSnapshotSource can establish the
    // required initial-Ready/no-cache/no-census state.
    fn close_catalog_fence_v1<F>(
        mut self,
        selector: ManagedCatalogFencePolicy,
        plan: F,
    ) -> Result<ManagedSqliteClosedCatalogFenceV1>
    where
        F: for<'reader> FnOnce(&mut ManagedSqliteCatalogAssertionPlannerV1<'reader>) -> Result<()>,
    {
        let operation = self.run_catalog_fence_v1(selector, plan);
        let assertions = self.finish_close_preserving(operation)?;
        let Self {
            path,
            main_file,
            residuals,
            policy,
            catalog,
            physical_census,
            ..
        } = self;
        if physical_census.is_some() {
            bail!("managed SQLite catalog fence unexpectedly retained a physical census");
        }
        let catalog = catalog.ok_or_else(|| {
            miette!("managed SQLite catalog fence lost its completed catalog cache")
        })?;
        let source = build_closed_source_evidence_v1(path, policy, main_file, residuals)?;
        Ok(ManagedSqliteClosedCatalogFenceV1 {
            catalog,
            assertions,
            source,
            snapshot_policy: policy,
            selector,
            catalog_fence_policy_fingerprint: *selector.fingerprint(),
            _thread_bound: PhantomData,
        })
    }

    fn run_catalog_fence_v1<F>(
        &mut self,
        selector: ManagedCatalogFencePolicy,
        plan: F,
    ) -> Result<ManagedSqliteCatalogFenceAssertionEvidenceV1>
    where
        F: for<'reader> FnOnce(&mut ManagedSqliteCatalogAssertionPlannerV1<'reader>) -> Result<()>,
    {
        self.run_catalog_fence_with_budget_v1(
            selector,
            ManagedProgressBudget {
                progress_interval: MANAGED_CATALOG_FENCE_PROGRESS_INTERVAL_V1,
                max_callbacks: MANAGED_CATALOG_FENCE_PROGRESS_CALLBACK_LIMIT_V1,
                phase: ManagedProgressPhase::CatalogFence,
            },
            plan,
        )
    }

    fn run_catalog_fence_with_budget_v1<F>(
        &mut self,
        selector: ManagedCatalogFencePolicy,
        budget: ManagedProgressBudget,
        plan: F,
    ) -> Result<ManagedSqliteCatalogFenceAssertionEvidenceV1>
    where
        F: for<'reader> FnOnce(&mut ManagedSqliteCatalogAssertionPlannerV1<'reader>) -> Result<()>,
    {
        if budget.phase != ManagedProgressPhase::CatalogFence {
            self.state = ManagedReaderState::Poisoned;
            bail!("managed SQLite catalog fence received the wrong progress phase");
        }
        if selector.snapshot_policy() != self.policy {
            self.state = ManagedReaderState::Poisoned;
            bail!("managed SQLite catalog fence received an unsupported selector");
        }
        if self.state != ManagedReaderState::Ready
            || self.catalog.is_some()
            || self.physical_census.is_some()
            || self.transaction_active
        {
            self.state = ManagedReaderState::Poisoned;
            bail!("managed SQLite catalog fence requires an initial ready reader state");
        }
        let connection_raw = match self.connection.as_ref() {
            Some(connection) => connection.as_raw(),
            None => {
                self.state = ManagedReaderState::Poisoned;
                bail!("managed SQLite catalog fence lost its connection");
            }
        };
        self.state = ManagedReaderState::Inspecting;
        let mut progress = ManagedProgressState {
            remaining_callbacks: budget.max_callbacks,
            callbacks_used: 0,
            interrupted: false,
        };
        let handler = match ManagedProgressHandler::install(connection_raw, &mut progress, budget) {
            Ok(handler) => handler,
            Err(error) => {
                self.state = ManagedReaderState::Poisoned;
                return Err(error);
            }
        };
        let operation = (|| {
            // Admit metadata before BEGIN and before the catalog scan. The
            // progress handler is already installed, so this ordering cannot
            // create a fresh assertion-only budget after an oversized source.
            let main_file_bytes = self.main_file.len()?;
            managed_admit_file_bytes(main_file_bytes)?;
            let catalog = self.inspect_catalog_uncached_v1()?;
            self.catalog = Some(catalog);
            self.state = ManagedReaderState::Asserting;
            let catalog = self
                .catalog
                .as_ref()
                .ok_or_else(|| miette!("managed SQLite catalog fence lost its catalog"))?;
            let connection = self
                .connection
                .as_ref()
                .ok_or_else(|| miette!("managed SQLite catalog fence lost its connection"))?;
            let mut planner = ManagedSqliteCatalogAssertionPlannerV1::new(catalog, connection)?;
            let callback = plan(&mut planner);
            planner.finish(callback)
        })();
        // Handler removal precedes both inspection and the final READ check.
        drop(handler);
        let operation = if progress.interrupted {
            Err(miette!(
                "managed SQLite catalog fence exhausted its fixed work budget"
            ))
        } else {
            operation.map(|mut evidence| {
                evidence.progress_callbacks_used = progress.callbacks_used;
                evidence
            })
        };
        let read = self.require_read_transaction("catalog-fence runner completion");
        let result = combine_with_postcondition(
            operation,
            read,
            "managed SQLite catalog-fence READ-state postcondition failed",
        );
        self.state = if result.is_ok() {
            ManagedReaderState::Observed
        } else {
            ManagedReaderState::Poisoned
        };
        result
    }

    /// Consume this reader into closed catalog, physical, assertion, and source evidence.
    ///
    /// The callback can borrow the cached catalog and physical census only for
    /// its invocation. Safe code cannot export either borrow through the
    /// higher-ranked callback boundary:
    ///
    /// ```compile_fail
    /// use cozo::{
    ///     ExistingSqliteSnapshotSource, ManagedSnapshotPolicy,
    ///     ManagedSqlitePrimaryIndexCatalogV1,
    /// };
    /// use miette::Result;
    /// use std::path::Path;
    /// fn escape(path: &Path) -> Result<&'static ManagedSqlitePrimaryIndexCatalogV1> {
    ///     let reader = ExistingSqliteSnapshotSource::open(path)?
    ///         .into_managed_reader(ManagedSnapshotPolicy::V1)?;
    ///     let mut escaped = None;
    ///     let _audit = reader.close_with_relation_assertions_v1(|planner| {
    ///         escaped = Some(planner.catalog());
    ///         Ok(())
    ///     })?;
    ///     Ok(escaped.unwrap())
    /// }
    /// ```
    ///
    /// Ordinary errors never bypass cleanup: assertion completion is captured
    /// first, then transaction reconciliation, strict close, and all source
    /// checks run before any transferable evidence is constructed. Panic uses
    /// only RAII cleanup and therefore returns no audit and makes no strict-close claim.
    pub fn close_with_relation_assertions_v1<F>(
        mut self,
        plan: F,
    ) -> Result<ManagedSqliteClosedAuditV1>
    where
        F: for<'reader> FnOnce(&mut ManagedSqliteRelationAssertionPlannerV1<'reader>) -> Result<()>,
    {
        let operation = self.run_relation_assertions_v1(plan);
        let assertions = self.finish_close_preserving(operation)?;

        let Self {
            path,
            main_file,
            residuals,
            policy,
            catalog,
            physical_census,
            ..
        } = self;
        let catalog = catalog.ok_or_else(|| {
            miette!("managed SQLite closed audit lost its completed catalog cache")
        })?;
        let physical = physical_census.ok_or_else(|| {
            miette!("managed SQLite closed audit lost its completed physical cache")
        })?;
        let source = build_closed_source_evidence_v1(path, policy, main_file, residuals)?;
        Ok(ManagedSqliteClosedAuditV1 {
            catalog,
            physical,
            assertions,
            source,
            policy,
            _thread_bound: PhantomData,
        })
    }

    /// Consume this reader through physical census, typed assertions, sixteen
    /// ordered record ranges, strict close, and source validation.
    ///
    /// The plan callback derives the sixteen positive physical relation ids while
    /// it has the ordinary higher-ranked planner borrow. The visit callback
    /// receives decoded one-record-at-a-time events and can mutate only the
    /// tentative caller sink. Neither callback can export a row or reader borrow:
    ///
    /// ```compile_fail
    /// use cozo::{
    ///     DataValue, ManagedSqliteRecordVisitEventV1, ManagedSqliteSnapshotReader,
    /// };
    /// use miette::Result;
    /// fn escape(reader: ManagedSqliteSnapshotReader, ids: [u64; 16])
    ///     -> Result<&'static [DataValue]>
    /// {
    ///     let mut escaped = None;
    ///     let _closed = reader.close_with_relation_assertions_and_record_visit_v1(
    ///         (),
    ///         |_| Ok(ids),
    ///         |_, event| {
    ///             if let ManagedSqliteRecordVisitEventV1::Record { key, .. } = event {
    ///                 escaped = Some(key);
    ///             }
    ///             Ok(())
    ///         },
    ///     )?;
    ///     Ok(escaped.unwrap())
    /// }
    /// ```
    ///
    /// Every ordinary failure still runs transaction reconciliation, rollback,
    /// strict close, and all source checks. The sink is dropped on every error.
    /// A successful wrapper must remain associated through independent downstream
    /// role and semantic verification; the selected id list is not completeness
    /// evidence by itself.
    pub fn close_with_relation_assertions_and_record_visit_v1<S, P, V>(
        mut self,
        sink: S,
        plan: P,
        visit: V,
    ) -> Result<ManagedSqliteClosedRecordVisitV1<S>>
    where
        P: for<'reader> FnOnce(
            &mut ManagedSqliteRelationAssertionPlannerV1<'reader>,
        ) -> Result<[u64; 16]>,
        V: for<'row> FnMut(&mut S, ManagedSqliteRecordVisitEventV1<'row>) -> Result<()>,
    {
        if self.policy != ManagedSnapshotPolicy::V1 {
            return self.finish_close_preserving(Err(miette!(
                "managed SQLite V1 record visitor requires the predecessor snapshot policy"
            )));
        }
        let mut selected_relation_ids = None;
        let assertions = self.run_relation_assertions_v1(|planner| {
            selected_relation_ids = Some(plan(planner)?);
            Ok(())
        });
        let operation = match assertions {
            Ok(assertions) => match selected_relation_ids {
                Some(relation_ids) => self
                    .run_record_visit_v1(sink, relation_ids, visit)
                    .map(|(visit_evidence, sink)| (assertions, visit_evidence, sink)),
                None => {
                    self.state = ManagedReaderState::Poisoned;
                    Err(miette!(
                        "managed SQLite record-visit planner returned no relation selection"
                    ))
                }
            },
            Err(error) => Err(error),
        };
        let (assertions, visit_evidence, sink) = self.finish_close_preserving(operation)?;

        let Self {
            path,
            main_file,
            residuals,
            policy,
            catalog,
            physical_census,
            ..
        } = self;
        let catalog = catalog.ok_or_else(|| {
            miette!("managed SQLite closed record visit lost its completed catalog cache")
        })?;
        let physical = physical_census.ok_or_else(|| {
            miette!("managed SQLite closed record visit lost its completed physical cache")
        })?;
        let source = build_closed_source_evidence_v1(path, policy, main_file, residuals)?;
        let audit = ManagedSqliteClosedAuditV1 {
            catalog,
            physical,
            assertions,
            source,
            policy,
            _thread_bound: PhantomData,
        };
        Ok(ManagedSqliteClosedRecordVisitV1 {
            audit,
            visit_evidence,
            sink,
            _thread_bound: PhantomData,
        })
    }

    fn finish_close_preserving<T>(&mut self, operation: Result<T>) -> Result<T> {
        let state_result = match self.state {
            ManagedReaderState::Ready
            | ManagedReaderState::Observed
            | ManagedReaderState::Audited => Ok(()),
            ManagedReaderState::Poisoned => {
                Err(miette!("managed SQLite snapshot reader was poisoned"))
            }
            ManagedReaderState::Inspecting => Err(miette!(
                "managed SQLite snapshot reader remained in catalog inspection"
            )),
            ManagedReaderState::Auditing => Err(miette!(
                "managed SQLite snapshot reader remained in physical census"
            )),
            ManagedReaderState::Asserting => Err(miette!(
                "managed SQLite snapshot reader remained in relation assertions"
            )),
            ManagedReaderState::Visiting => Err(miette!(
                "managed SQLite snapshot reader remained in record visitation"
            )),
        };

        let tracked_active = self.transaction_active;
        let (live_active, tracked_live_result) = match self.connection.as_ref() {
            Some(connection) => {
                let live_active = unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } == 0;
                let result = if tracked_active == live_active {
                    Ok(())
                } else {
                    Err(miette!(
                        "managed SQLite tracked transaction state disagreed with live autocommit"
                    ))
                };
                (live_active, result)
            }
            None => (
                false,
                Err(miette!(
                    "managed SQLite transaction reconciliation lost its connection"
                )),
            ),
        };

        let rollback_result = if tracked_active || live_active {
            match self.connection.as_ref() {
                Some(connection) => managed_exec_fixed(
                    connection,
                    MANAGED_ROLLBACK_READ_SQL,
                    "managed reader transaction rollback",
                ),
                None => Err(miette!(
                    "managed SQLite transaction rollback lost its connection"
                )),
            }
        } else {
            Ok(())
        };
        let autocommit_result = match self.connection.as_ref() {
            Some(connection) => {
                let autocommit = unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) };
                if autocommit != 0 {
                    Ok(())
                } else {
                    Err(miette!(
                        "managed SQLite transaction cleanup left autocommit disabled"
                    ))
                }
            }
            None => Err(miette!(
                "managed SQLite autocommit postcondition lost its connection"
            )),
        };
        self.transaction_active = false;

        let close_result = match self.connection.take() {
            Some(connection) => connection.close("managed SQLite snapshot reader"),
            None => Err(miette!("managed SQLite snapshot reader was already closed")),
        };
        let source_result = verify_snapshot_source_with_retained_backup(
            &self.main_file,
            self.retained_backup_destination.as_ref(),
            &self.residuals,
            &self.path,
            "managed SQLite source after reader close",
        );

        let lifecycle_result = combine_with_postcondition(
            state_result,
            tracked_live_result,
            "managed SQLite tracked/live transaction reconciliation also failed",
        );
        let lifecycle_result = combine_with_postcondition(
            lifecycle_result,
            rollback_result,
            "managed SQLite transaction rollback also failed",
        );
        let lifecycle_result = combine_with_postcondition(
            lifecycle_result,
            autocommit_result,
            "managed SQLite autocommit postcondition also failed",
        );
        let lifecycle_result = combine_with_postcondition(
            lifecycle_result,
            close_result,
            "managed SQLite strict close also failed",
        );
        let lifecycle_result = combine_with_postcondition(
            lifecycle_result,
            source_result,
            "managed SQLite source postcondition also failed",
        );
        combine_with_postcondition(
            operation,
            lifecycle_result,
            "managed SQLite consuming cleanup/postconditions also failed",
        )
    }

    /// Close SQLite explicitly, then revalidate the held descriptor, pathname,
    /// and admitted residuals. Any pinned read transaction is first rolled back.
    /// This deliberately returns no transferable seal: every catalog/census
    /// borrow ends before consuming close, and a later source-binding boundary
    /// must combine its own authenticated inputs rather than laundering this
    /// conditional SQLite/VFS observation into attestation.
    pub fn close_and_verify(mut self) -> Result<()> {
        self.finish_close_preserving(Ok(()))
    }
}

fn scan_managed_catalog_v1(
    connection: &SnapshotConnection,
) -> Result<ManagedSqlitePrimaryIndexCatalogV1> {
    scan_managed_catalog_for(connection, ManagedSnapshotPolicy::V1)
}

fn scan_managed_catalog_for(
    connection: &SnapshotConnection,
    policy: ManagedSnapshotPolicy,
) -> Result<ManagedSqlitePrimaryIndexCatalogV1> {
    let relation_count = policy.relation_count();
    let expected_rows = policy.catalog_row_count();
    let query_row_limit = expected_rows + 1;
    let mut raw = ptr::null_mut();
    let mut tail = ptr::null();
    let prepare = unsafe {
        ffi::sqlite3_prepare_v3(
            connection.as_raw(),
            policy.catalog_query().as_ptr().cast(),
            -1,
            ffi::SQLITE_PREPARE_PERSISTENT as u32,
            &mut raw,
            &mut tail,
        )
    };
    if prepare != ffi::SQLITE_OK {
        if !raw.is_null() {
            unsafe {
                ffi::sqlite3_finalize(raw);
            }
        }
        bail!("managed SQLite catalog query preparation failed with code {prepare}");
    }
    if raw.is_null() {
        bail!("managed SQLite catalog query preparation returned no statement");
    }
    let statement = ManagedStatement { raw };
    if tail.is_null() || unsafe { *tail } != 0 {
        bail!("managed SQLite internal catalog query contained trailing SQL");
    }
    if unsafe { ffi::sqlite3_stmt_readonly(statement.raw) } != 1 {
        bail!("managed SQLite internal catalog query was not read-only");
    }
    if unsafe { ffi::sqlite3_column_count(statement.raw) } != 4 {
        bail!("managed SQLite internal catalog query had an invalid result shape");
    }

    let mut entries = Vec::with_capacity(relation_count);
    let mut raw_rows = Vec::with_capacity(expected_rows);
    let mut relation_counter = None;
    let mut storage_version = None;
    let mut previous_key = Vec::new();
    let mut row_count = 0_usize;
    let mut cumulative_raw = 0_usize;
    let mut cumulative_canonical = 0_usize;
    let mut raw_commitment = Sha256::new();
    raw_commitment.update(MANAGED_RAW_COMMITMENT_DOMAIN_V1);
    let mut canonical_commitment = Sha256::new();
    canonical_commitment.update(MANAGED_CANONICAL_COMMITMENT_DOMAIN_V1);

    loop {
        let step = unsafe { ffi::sqlite3_step(statement.raw) };
        if step == ffi::SQLITE_DONE {
            break;
        }
        if step != ffi::SQLITE_ROW {
            bail!("managed SQLite catalog query failed with code {step}");
        }
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite catalog row count overflowed"))?;
        if row_count >= query_row_limit {
            bail!("managed SQLite catalog exceeded its fixed row boundary");
        }

        let (key_cell, value_cell) = managed_catalog_blob_cells(&statement, cumulative_raw)?;
        cumulative_raw = checked_catalog_total(
            cumulative_raw,
            key_cell.len,
            value_cell.len,
            MANAGED_CATALOG_CUMULATIVE_RAW_LIMIT,
            "raw",
        )?;
        // SAFETY: both cells were obtained from the current SQLITE_ROW. Their
        // lengths exactly match SQLite's byte counts, and no step/reset/finalize
        // occurs until all uses below are complete.
        let key = unsafe { key_cell.as_slice() };
        let value = unsafe { value_cell.as_slice() };

        if !previous_key.is_empty() && previous_key.as_slice() >= key {
            bail!("managed SQLite catalog keys were not strictly ordered");
        }
        previous_key.clear();
        previous_key.extend_from_slice(key);
        update_catalog_commitment(&mut raw_commitment, key, value)?;
        raw_rows.push(ManagedCatalogRawRow {
            key: managed_copy_bounded_cell(key, "catalog key")?,
            value: managed_copy_bounded_cell(value, "catalog value")?,
        });

        let tuple = try_decode_tuple_from_key(key, 2)
            .map_err(|_| miette!("managed SQLite catalog key rejected by exact decoder"))?;
        if key.get(..8) != Some(&[0_u8; 8]) {
            bail!("managed SQLite catalog query returned a foreign relation id");
        }

        match tuple.as_slice() {
            [DataValue::Str(key_name)] => {
                if entries.len() >= relation_count {
                    bail!("managed SQLite catalog contained too many relation entries");
                }
                let record = decode_unattested_catalog(value).map_err(|_| {
                    miette!("managed SQLite relation catalog value rejected by exact codec")
                })?;
                let canonical = record.canonical_struct_map().map_err(|_| {
                    miette!("managed SQLite relation catalog canonicalization failed")
                })?;
                cumulative_canonical = checked_catalog_total(
                    cumulative_canonical,
                    key.len(),
                    canonical.len(),
                    MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT,
                    "canonical",
                )?;
                update_catalog_commitment(&mut canonical_commitment, key, &canonical)?;
                let entry = record.into_managed_entry_v1();
                if entry.relation().name() != key_name.as_str() {
                    bail!("managed SQLite catalog key/name linkage mismatch");
                }
                entries.push(entry);
            }
            [DataValue::Null] => {
                if relation_counter.is_some() || value.len() != 8 {
                    bail!("managed SQLite relation counter row was not exact");
                }
                let bytes: [u8; 8] = value
                    .try_into()
                    .map_err(|_| miette!("managed SQLite relation counter width changed"))?;
                relation_counter = Some(u64::from_be_bytes(bytes));
                cumulative_canonical = checked_catalog_total(
                    cumulative_canonical,
                    key.len(),
                    value.len(),
                    MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT,
                    "canonical",
                )?;
                update_catalog_commitment(&mut canonical_commitment, key, value)?;
            }
            [DataValue::Null, DataValue::Str(marker)] if marker == "STORAGE_VERSION" => {
                if storage_version.is_some() || value != [0_u8] {
                    bail!("managed SQLite storage-version row was not exact");
                }
                storage_version = Some(0);
                cumulative_canonical = checked_catalog_total(
                    cumulative_canonical,
                    key.len(),
                    value.len(),
                    MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT,
                    "canonical",
                )?;
                update_catalog_commitment(&mut canonical_commitment, key, value)?;
            }
            _ => bail!("managed SQLite catalog contained a foreign id-zero key shape"),
        }
    }

    let finalize = statement.finish();
    if finalize != ffi::SQLITE_OK {
        bail!("managed SQLite catalog query finalization failed with code {finalize}");
    }
    if row_count != expected_rows
        || entries.len() != relation_count
        || relation_counter.is_none()
        || storage_version.is_none()
    {
        bail!("managed SQLite catalog did not contain the exact V1 syntactic domain");
    }

    finish_catalog_commitment(&mut raw_commitment, row_count)?;
    finish_catalog_commitment(&mut canonical_commitment, row_count)?;
    Ok(ManagedSqlitePrimaryIndexCatalogV1 {
        snapshot_policy: policy,
        catalog: ManagedCatalogCensusV1::from_entries(entries),
        relation_counter: relation_counter
            .ok_or_else(|| miette!("managed SQLite relation counter disappeared"))?,
        storage_version: storage_version
            .ok_or_else(|| miette!("managed SQLite storage version disappeared"))?,
        raw_commitment: raw_commitment.finalize().into(),
        canonical_commitment: canonical_commitment.finalize().into(),
        policy_fingerprint: *policy.catalog_fingerprint(),
        raw_rows: raw_rows.into_boxed_slice(),
        _thread_bound: PhantomData,
    })
}

fn managed_copy_bounded_cell(bytes: &[u8], label: &'static str) -> Result<Box<[u8]>> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(bytes.len())
        .map_err(|_| miette!("managed SQLite could not reserve its bounded {label} cache"))?;
    copied.extend_from_slice(bytes);
    Ok(copied.into_boxed_slice())
}

struct ManagedCatalogBlobCell {
    pointer: *const u8,
    len: usize,
}

impl ManagedCatalogBlobCell {
    unsafe fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            // SAFETY: the caller upholds the SQLITE_ROW lifetime contract.
            unsafe { std::slice::from_raw_parts(self.pointer, self.len) }
        }
    }
}

fn managed_catalog_blob_cells(
    statement: &ManagedStatement,
    cumulative_raw: usize,
) -> Result<(ManagedCatalogBlobCell, ManagedCatalogBlobCell)> {
    if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER
        || unsafe { ffi::sqlite3_column_type(statement.raw, 1) } != ffi::SQLITE_INTEGER
    {
        bail!("managed SQLite catalog length query returned a non-integer");
    }
    if unsafe { ffi::sqlite3_column_type(statement.raw, 2) } != ffi::SQLITE_BLOB
        || unsafe { ffi::sqlite3_column_type(statement.raw, 3) } != ffi::SQLITE_BLOB
    {
        bail!("managed SQLite catalog row contained a non-BLOB cell");
    }

    let key_sql_len = unsafe { ffi::sqlite3_column_int64(statement.raw, 0) };
    let value_sql_len = unsafe { ffi::sqlite3_column_int64(statement.raw, 1) };
    if key_sql_len < 0 || value_sql_len < 0 {
        bail!("managed SQLite catalog row reported a negative length");
    }
    let key_len = usize::try_from(key_sql_len)
        .map_err(|_| miette!("managed SQLite catalog key length was not representable"))?;
    let value_len = usize::try_from(value_sql_len)
        .map_err(|_| miette!("managed SQLite catalog value length was not representable"))?;
    if key_len > MAX_ENCODED_KEY_BYTES {
        bail!("managed SQLite catalog key exceeded its byte boundary");
    }
    if value_len > MANAGED_CATALOG_VALUE_LIMIT {
        bail!("managed SQLite catalog value exceeded its byte boundary");
    }
    // Validate cumulative work before asking SQLite for either blob pointer.
    checked_catalog_total(
        cumulative_raw,
        key_len,
        value_len,
        MANAGED_CATALOG_CUMULATIVE_RAW_LIMIT,
        "raw",
    )?;

    #[cfg(test)]
    MANAGED_CATALOG_BLOB_POINTER_REQUESTS.with(|requests| requests.set(requests.get() + 2));
    let key_pointer = unsafe { ffi::sqlite3_column_blob(statement.raw, 2) }.cast::<u8>();
    let value_pointer = unsafe { ffi::sqlite3_column_blob(statement.raw, 3) }.cast::<u8>();
    let key_bytes = unsafe { ffi::sqlite3_column_bytes(statement.raw, 2) };
    let value_bytes = unsafe { ffi::sqlite3_column_bytes(statement.raw, 3) };
    if key_bytes < 0 || value_bytes < 0 {
        bail!("managed SQLite catalog row reported a negative byte count");
    }
    if usize::try_from(key_bytes).ok() != Some(key_len)
        || usize::try_from(value_bytes).ok() != Some(value_len)
    {
        bail!("managed SQLite catalog SQL and blob lengths disagreed");
    }
    if (key_len != 0 && key_pointer.is_null()) || (value_len != 0 && value_pointer.is_null()) {
        bail!("managed SQLite catalog returned a null nonempty BLOB");
    }
    Ok((
        ManagedCatalogBlobCell {
            pointer: key_pointer,
            len: key_len,
        },
        ManagedCatalogBlobCell {
            pointer: value_pointer,
            len: value_len,
        },
    ))
}

fn checked_catalog_total(
    current: usize,
    key_len: usize,
    value_len: usize,
    limit: usize,
    label: &'static str,
) -> Result<usize> {
    let observed = current
        .checked_add(key_len)
        .and_then(|total| total.checked_add(value_len))
        .ok_or_else(|| miette!("managed SQLite {label} catalog byte count overflowed"))?;
    if observed > limit {
        bail!("managed SQLite {label} catalog exceeded its cumulative byte boundary");
    }
    Ok(observed)
}

fn update_catalog_commitment(hasher: &mut Sha256, key: &[u8], value: &[u8]) -> Result<()> {
    let key_len = u64::try_from(key.len())
        .map_err(|_| miette!("managed SQLite catalog key length exceeded commitment width"))?;
    let value_len = u64::try_from(value.len())
        .map_err(|_| miette!("managed SQLite catalog value length exceeded commitment width"))?;
    hasher.update(MANAGED_COMMITMENT_ROW_MARKER_V1);
    hasher.update(key_len.to_be_bytes());
    hasher.update(key);
    hasher.update(value_len.to_be_bytes());
    hasher.update(value);
    Ok(())
}

fn finish_catalog_commitment(hasher: &mut Sha256, row_count: usize) -> Result<()> {
    let row_count = u64::try_from(row_count)
        .map_err(|_| miette!("managed SQLite catalog row count exceeded commitment width"))?;
    hasher.update(MANAGED_COMMITMENT_TERMINATOR_V1);
    hasher.update(row_count.to_be_bytes());
    Ok(())
}

#[cfg(any(test, feature = "test-hooks"))]
struct ManagedCatalogFixtureUpdateV1 {
    key: Box<[u8]>,
    value: Box<[u8]>,
}

#[cfg(any(test, feature = "test-hooks"))]
fn apply_catalog_fixture_v1(
    connection: &SnapshotConnection,
    destination: &IncompleteDestination,
    fixture: ManagedCatalogFixtureV1,
) -> Result<()> {
    let main_file_bytes = destination.file_len()?;
    managed_admit_file_bytes(main_file_bytes)?;
    configure_snapshot_source_guardrails(connection)?;
    configure_managed_physical_guardrails(connection)?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_LENGTH,
        MANAGED_SQLITE_CATALOG_LENGTH_LIMIT,
        "catalog fixture row length",
    )?;

    let (preimage, _) = managed_with_progress_budget(
        connection.as_raw(),
        ManagedProgressBudget::default(),
        || {
            managed_catalog_fixture_read_transaction(connection, || {
                let catalog = scan_managed_catalog_v1(connection)?;
                scan_managed_physical_census_v1(connection, &catalog, main_file_bytes)?;
                Ok(catalog)
            })
        },
    )?;
    let updates = plan_catalog_fixture_updates_v1(&preimage, fixture)?;

    managed_exec_fixed(
        connection,
        MANAGED_TEST_QUERY_ONLY_OFF_SQL,
        "catalog fixture write mode",
    )?;
    managed_with_progress_budget(
        connection.as_raw(),
        ManagedProgressBudget {
            progress_interval: MANAGED_PHYSICAL_PROGRESS_INTERVAL,
            max_callbacks: MANAGED_CATALOG_FIXTURE_WRITE_CALLBACK_LIMIT,
            phase: ManagedProgressPhase::Census,
        },
        || apply_catalog_fixture_updates_v1(connection, &updates),
    )?;
    managed_exec_fixed(
        connection,
        MANAGED_QUERY_ONLY_SQL,
        "catalog fixture restored query-only mode",
    )?;
    let post_mutation_file_bytes = destination.file_len()?;
    managed_admit_file_bytes(post_mutation_file_bytes)?;

    // Recheck the exact bounded catalog syntax and SQLite integrity after the
    // intentional mutation, including geometry against the remeasured owned
    // destination. Do not run the semantic oracle here, and do not claim a
    // complete physical census: some reviewed fixtures are meant to be rejected
    // by those later layers.
    managed_with_progress_budget(
        connection.as_raw(),
        ManagedProgressBudget::default(),
        || {
            managed_catalog_fixture_read_transaction(connection, || {
                let _catalog = scan_managed_catalog_v1(connection)?;
                managed_page_admission(connection, post_mutation_file_bytes)?;
                managed_integrity_check(connection)?;
                managed_require_read_transaction(connection, "catalog fixture postcondition")
            })
        },
    )?;
    Ok(())
}

#[cfg(any(test, feature = "test-hooks"))]
fn managed_catalog_fixture_read_transaction<T>(
    connection: &SnapshotConnection,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } != 1 {
        bail!("managed SQLite catalog fixture read entered with an active transaction");
    }
    managed_exec_fixed(
        connection,
        MANAGED_BEGIN_READ_SQL,
        "catalog fixture read transaction",
    )?;
    let operation = (|| {
        managed_boolean_check(
            connection,
            SQLITE_SCHEMA_PROBE,
            "catalog fixture snapshot pin",
        )?;
        managed_require_read_transaction(connection, "catalog fixture snapshot pin")?;
        operation()
    })();
    let rollback = managed_exec_fixed(
        connection,
        MANAGED_ROLLBACK_READ_SQL,
        "catalog fixture read rollback",
    );
    let rollback = combine_with_postcondition(
        rollback,
        if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } == 1 {
            Ok(())
        } else {
            Err(miette!(
                "managed SQLite catalog fixture rollback left an active transaction"
            ))
        },
        "catalog fixture read rollback postcondition failed",
    );
    combine_with_postcondition(
        operation,
        rollback,
        "catalog fixture read transaction cleanup failed",
    )
}

#[cfg(any(test, feature = "test-hooks"))]
fn plan_catalog_fixture_updates_v1(
    catalog: &ManagedSqlitePrimaryIndexCatalogV1,
    fixture: ManagedCatalogFixtureV1,
) -> Result<Box<[ManagedCatalogFixtureUpdateV1]>> {
    let mut relation_rows = Vec::with_capacity(MANAGED_CATALOG_RELATION_COUNT);
    let mut counter_row = None;
    for row in &catalog.raw_rows {
        let tuple = try_decode_tuple_from_key(&row.key, 2)
            .map_err(|_| miette!("managed SQLite catalog fixture key precondition failed"))?;
        match tuple.as_slice() {
            [DataValue::Str(_)] => relation_rows.push(row),
            [DataValue::Null] => counter_row = Some(row),
            [DataValue::Null, DataValue::Str(marker)] if marker == "STORAGE_VERSION" => {}
            _ => bail!("managed SQLite catalog fixture key precondition failed"),
        }
    }
    if relation_rows.len() != MANAGED_CATALOG_RELATION_COUNT || counter_row.is_none() {
        bail!("managed SQLite catalog fixture row precondition failed");
    }

    let values = relation_rows
        .iter()
        .map(|row| row.value.as_ref())
        .collect::<Vec<_>>();
    let rewrite = rewrite_catalog_fixture_records_v1(&values, fixture)
        .map_err(|_| miette!("managed SQLite catalog fixture rewrite precondition failed"))?;
    match rewrite {
        CatalogFixtureRewriteV1::RelationValues(rewritten) => {
            if rewritten.is_empty() || rewritten.len() > MANAGED_CATALOG_RELATION_COUNT {
                bail!("managed SQLite catalog fixture rewrite count was invalid");
            }
            let mut updates = Vec::with_capacity(rewritten.len());
            let mut previous_position = None;
            for rewritten in rewritten.iter() {
                let position = rewritten.position();
                if position >= relation_rows.len()
                    || previous_position.is_some_and(|previous| previous >= position)
                {
                    bail!("managed SQLite catalog fixture rewrite positions were invalid");
                }
                previous_position = Some(position);
                updates.push(ManagedCatalogFixtureUpdateV1 {
                    key: relation_rows[position].key.to_vec().into_boxed_slice(),
                    value: rewritten.value().to_vec().into_boxed_slice(),
                });
            }
            Ok(updates.into_boxed_slice())
        }
        CatalogFixtureRewriteV1::RelationCounter(value) => {
            let row = counter_row
                .ok_or_else(|| miette!("managed SQLite catalog fixture counter disappeared"))?;
            Ok(vec![ManagedCatalogFixtureUpdateV1 {
                key: row.key.to_vec().into_boxed_slice(),
                value: Box::new(value),
            }]
            .into_boxed_slice())
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
fn apply_catalog_fixture_updates_v1(
    connection: &SnapshotConnection,
    updates: &[ManagedCatalogFixtureUpdateV1],
) -> Result<()> {
    if updates.is_empty() || updates.len() > MANAGED_CATALOG_RELATION_COUNT {
        bail!("managed SQLite catalog fixture update count was invalid");
    }
    if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } != 1 {
        bail!("managed SQLite catalog fixture write entered with an active transaction");
    }
    managed_exec_fixed(
        connection,
        MANAGED_CATALOG_FIXTURE_BEGIN_SQL,
        "catalog fixture write transaction",
    )?;

    let operation = (|| {
        let statement = prepare_catalog_fixture_update_v1(connection)?;
        let writes = (|| {
            for update in updates {
                apply_one_catalog_fixture_update_v1(connection, &statement, update)?;
            }
            Ok(())
        })();
        let finalize = statement.finish();
        combine_with_postcondition(
            writes,
            if finalize == ffi::SQLITE_OK {
                Ok(())
            } else {
                Err(miette!(
                    "managed SQLite catalog fixture update finalization failed with code {finalize}"
                ))
            },
            "catalog fixture update statement cleanup failed",
        )?;
        managed_exec_fixed(
            connection,
            MANAGED_CATALOG_FIXTURE_COMMIT_SQL,
            "catalog fixture write commit",
        )?;
        if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } != 1 {
            bail!("managed SQLite catalog fixture commit left an active transaction");
        }
        Ok(())
    })();

    match operation {
        Ok(()) => Ok(()),
        Err(error) => {
            let rollback = if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } == 0 {
                managed_exec_fixed(
                    connection,
                    MANAGED_CATALOG_FIXTURE_ROLLBACK_SQL,
                    "catalog fixture write rollback",
                )
            } else {
                Ok(())
            };
            let rollback = combine_with_postcondition(
                rollback,
                if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } == 1 {
                    Ok(())
                } else {
                    Err(miette!(
                        "managed SQLite catalog fixture rollback left an active transaction"
                    ))
                },
                "catalog fixture write rollback postcondition failed",
            );
            combine_with_postcondition(
                Err(error),
                rollback,
                "catalog fixture write transaction cleanup failed",
            )
        }
    }
}

#[cfg(any(test, feature = "test-hooks"))]
fn prepare_catalog_fixture_update_v1(connection: &SnapshotConnection) -> Result<ManagedStatement> {
    let mut raw = ptr::null_mut();
    let mut tail = ptr::null();
    let code = unsafe {
        ffi::sqlite3_prepare_v3(
            connection.as_raw(),
            MANAGED_CATALOG_FIXTURE_UPDATE_SQL.as_ptr().cast(),
            -1,
            ffi::SQLITE_PREPARE_PERSISTENT as u32,
            &mut raw,
            &mut tail,
        )
    };
    if code != ffi::SQLITE_OK {
        if !raw.is_null() {
            unsafe {
                ffi::sqlite3_finalize(raw);
            }
        }
        bail!("managed SQLite catalog fixture update preparation failed with code {code}");
    }
    if raw.is_null() {
        bail!("managed SQLite catalog fixture update preparation returned no statement");
    }
    let statement = ManagedStatement { raw };
    if tail.is_null() || unsafe { *tail } != 0 {
        bail!("managed SQLite catalog fixture update contained trailing SQL");
    }
    if unsafe { ffi::sqlite3_stmt_readonly(statement.raw) } != 0
        || unsafe { ffi::sqlite3_column_count(statement.raw) } != 0
        || unsafe { ffi::sqlite3_bind_parameter_count(statement.raw) } != 2
    {
        bail!("managed SQLite catalog fixture update had an invalid shape");
    }
    Ok(statement)
}

#[cfg(any(test, feature = "test-hooks"))]
fn apply_one_catalog_fixture_update_v1(
    connection: &SnapshotConnection,
    statement: &ManagedStatement,
    update: &ManagedCatalogFixtureUpdateV1,
) -> Result<()> {
    managed_reset_and_clear(statement, "catalog fixture update before bind")?;
    let operation = (|| {
        let value_len = i32::try_from(update.value.len())
            .map_err(|_| miette!("managed SQLite catalog fixture value length was invalid"))?;
        let key_len = i32::try_from(update.key.len())
            .map_err(|_| miette!("managed SQLite catalog fixture key length was invalid"))?;
        let value_bind = unsafe {
            ffi::sqlite3_bind_blob(
                statement.raw,
                1,
                update.value.as_ptr().cast(),
                value_len,
                None,
            )
        };
        if value_bind != ffi::SQLITE_OK {
            bail!("managed SQLite catalog fixture value binding failed with code {value_bind}");
        }
        let key_bind = unsafe {
            ffi::sqlite3_bind_blob(statement.raw, 2, update.key.as_ptr().cast(), key_len, None)
        };
        if key_bind != ffi::SQLITE_OK {
            bail!("managed SQLite catalog fixture key binding failed with code {key_bind}");
        }
        let step = unsafe { ffi::sqlite3_step(statement.raw) };
        if step != ffi::SQLITE_DONE {
            bail!("managed SQLite catalog fixture update failed with code {step}");
        }
        let fullscan_steps = unsafe {
            ffi::sqlite3_stmt_status(statement.raw, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1)
        };
        if fullscan_steps != 0 {
            bail!("managed SQLite catalog fixture update did not stay on its forced index path");
        }
        let changes = unsafe { ffi::sqlite3_changes64(connection.as_raw()) };
        if changes != 1 {
            bail!("managed SQLite catalog fixture update changed other than exactly one row");
        }
        Ok(())
    })();
    combine_with_postcondition(
        operation,
        managed_reset_and_clear(statement, "catalog fixture update after step"),
        "catalog fixture update reset failed",
    )
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ManagedProgressPhase {
    Census,
    RelationAssertions,
    CatalogFence,
}

impl ManagedProgressPhase {
    const fn invalid_message(self) -> &'static str {
        match self {
            Self::Census => "managed SQLite census progress budget was invalid",
            Self::RelationAssertions => {
                "managed SQLite relation assertion progress budget was invalid"
            }
            Self::CatalogFence => "managed SQLite catalog-fence progress budget was invalid",
        }
    }

    const fn exhausted_message(self) -> &'static str {
        match self {
            Self::Census => "managed SQLite census exhausted its fixed work budget",
            Self::RelationAssertions => {
                "managed SQLite relation assertions exhausted their fixed work budget"
            }
            Self::CatalogFence => "managed SQLite catalog fence exhausted its fixed work budget",
        }
    }
}

#[derive(Clone, Copy)]
struct ManagedProgressBudget {
    progress_interval: i32,
    max_callbacks: u64,
    phase: ManagedProgressPhase,
}

impl Default for ManagedProgressBudget {
    fn default() -> Self {
        Self {
            progress_interval: MANAGED_PHYSICAL_PROGRESS_INTERVAL,
            max_callbacks: MANAGED_PHYSICAL_PROGRESS_CALLBACK_LIMIT,
            phase: ManagedProgressPhase::Census,
        }
    }
}

impl ManagedProgressBudget {
    const fn relation_assertions_v1() -> Self {
        Self {
            progress_interval: MANAGED_ASSERTION_PROGRESS_INTERVAL,
            max_callbacks: MANAGED_ASSERTION_PROGRESS_CALLBACK_LIMIT,
            phase: ManagedProgressPhase::RelationAssertions,
        }
    }

    fn after_callbacks(self, callbacks_used: u64) -> Result<Self> {
        let max_callbacks = self
            .max_callbacks
            .checked_sub(callbacks_used)
            .filter(|remaining| *remaining != 0)
            .ok_or_else(|| miette!("{}", self.phase.exhausted_message()))?;
        Ok(Self {
            progress_interval: self.progress_interval,
            max_callbacks,
            phase: self.phase,
        })
    }
}

struct ManagedProgressState {
    remaining_callbacks: u64,
    callbacks_used: u64,
    interrupted: bool,
}

unsafe extern "C" fn managed_progress_callback(context: *mut c_void) -> i32 {
    if context.is_null() {
        return 1;
    }
    // SAFETY: ManagedProgressHandler unregisters this callback before the
    // borrowed stack state dies, including during Rust unwinding.
    let state = unsafe { &mut *context.cast::<ManagedProgressState>() };
    state.callbacks_used = state.callbacks_used.saturating_add(1);
    if state.remaining_callbacks <= 1 {
        state.remaining_callbacks = 0;
        state.interrupted = true;
        1
    } else {
        state.remaining_callbacks -= 1;
        0
    }
}

struct ManagedProgressHandler<'state> {
    connection: *mut ffi::sqlite3,
    #[cfg(test)]
    phase: ManagedProgressPhase,
    _state: PhantomData<&'state mut ManagedProgressState>,
}

impl<'state> ManagedProgressHandler<'state> {
    fn install(
        connection: *mut ffi::sqlite3,
        state: &'state mut ManagedProgressState,
        budget: ManagedProgressBudget,
    ) -> Result<Self> {
        if connection.is_null() || budget.progress_interval <= 0 || state.remaining_callbacks == 0 {
            bail!("{}", budget.phase.invalid_message());
        }
        unsafe {
            ffi::sqlite3_progress_handler(
                connection,
                budget.progress_interval,
                Some(managed_progress_callback),
                ptr::from_mut(state).cast(),
            );
        }
        #[cfg(test)]
        if budget.phase == ManagedProgressPhase::RelationAssertions {
            MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.set(count.get() + 1));
        }
        #[cfg(test)]
        if budget.phase == ManagedProgressPhase::CatalogFence {
            MANAGED_CATALOG_FENCE_PROGRESS_INSTALLS.with(|count| count.set(count.get() + 1));
        }
        Ok(Self {
            connection,
            #[cfg(test)]
            phase: budget.phase,
            _state: PhantomData,
        })
    }
}

impl Drop for ManagedProgressHandler<'_> {
    fn drop(&mut self) {
        unsafe {
            ffi::sqlite3_progress_handler(self.connection, 0, None, ptr::null_mut());
        }
        #[cfg(test)]
        if self.phase == ManagedProgressPhase::RelationAssertions {
            MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.set(count.get() + 1));
        }
        #[cfg(test)]
        if self.phase == ManagedProgressPhase::CatalogFence {
            MANAGED_CATALOG_FENCE_PROGRESS_UNREGISTRATIONS.with(|count| count.set(count.get() + 1));
        }
    }
}

fn managed_with_progress_budget<T>(
    connection: *mut ffi::sqlite3,
    budget: ManagedProgressBudget,
    operation: impl FnOnce() -> Result<T>,
) -> Result<(T, u64)> {
    let mut progress = ManagedProgressState {
        remaining_callbacks: budget.max_callbacks,
        callbacks_used: 0,
        interrupted: false,
    };
    let handler = ManagedProgressHandler::install(connection, &mut progress, budget)?;
    let result = operation();

    // The handler must unregister before its borrowed stack context is
    // inspected or dies. Drop also runs during unwinding.
    drop(handler);
    if progress.interrupted {
        bail!("{}", budget.phase.exhausted_message());
    }
    Ok((result?, progress.callbacks_used))
}

struct ManagedTableCensus {
    row_count: u64,
    counts: BTreeMap<u64, u64>,
    ordered_commitment: [u8; 32],
}

struct ManagedIndexCensus {
    row_count: u64,
    counts: BTreeMap<u64, u64>,
    ordered_commitment: [u8; 32],
}

struct ManagedLinkedSqliteRuntimeIdentity {
    version_number: i32,
    version: Box<[u8]>,
    source_id: Box<[u8]>,
    fingerprint: [u8; 32],
}

fn managed_require_little_endian() -> Result<()> {
    if !cfg!(target_endian = "little") {
        bail!(
            "managed SQLite physical V1 requires a little-endian target for persisted row vectors"
        );
    }
    Ok(())
}

fn managed_linked_sqlite_runtime_identity() -> Result<ManagedLinkedSqliteRuntimeIdentity> {
    let version_number = unsafe { ffi::sqlite3_libversion_number() };
    if version_number < MIN_SNAPSHOT_SQLITE_VERSION {
        bail!("managed SQLite linked runtime fell below the admitted version floor");
    }
    let version = managed_linked_sqlite_identity_bytes(
        unsafe { ffi::sqlite3_libversion() },
        MANAGED_SQLITE_RUNTIME_VERSION_LIMIT,
        "version",
    )?;
    let source_id = managed_linked_sqlite_identity_bytes(
        unsafe { ffi::sqlite3_sourceid() },
        MANAGED_SQLITE_RUNTIME_SOURCE_ID_LIMIT,
        "source id",
    )?;
    let fingerprint =
        managed_linked_sqlite_runtime_identity_fingerprint(version_number, &version, &source_id)?;
    Ok(ManagedLinkedSqliteRuntimeIdentity {
        version_number,
        version,
        source_id,
        fingerprint,
    })
}

fn managed_linked_sqlite_identity_bytes(
    pointer: *const std::ffi::c_char,
    limit: usize,
    label: &'static str,
) -> Result<Box<[u8]>> {
    if pointer.is_null() {
        bail!("managed SQLite linked runtime returned a null {label}");
    }
    // SAFETY: SQLite documents both identity pointers as process-lifetime,
    // immutable NUL-terminated strings owned by the linked library.
    let value = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    if value.is_empty() || value.len() > limit {
        bail!("managed SQLite linked runtime returned an invalid bounded {label}");
    }
    Ok(value.into())
}

fn managed_linked_sqlite_runtime_identity_fingerprint(
    version_number: i32,
    version: &[u8],
    source_id: &[u8],
) -> Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(MANAGED_SQLITE_RUNTIME_IDENTITY_FINGERPRINT_DOMAIN_V1);
    let mut field_count = 0_u64;
    for (label, value) in [
        (
            b"version-number".as_slice(),
            version_number.to_be_bytes().as_slice(),
        ),
        (b"version".as_slice(), version),
        (b"source-id".as_slice(), source_id),
    ] {
        field_count = field_count
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite runtime identity field count overflowed"))?;
        let label_len = u64::try_from(label.len())
            .map_err(|_| miette!("managed SQLite runtime identity label exceeded framing"))?;
        let value_len = u64::try_from(value.len())
            .map_err(|_| miette!("managed SQLite runtime identity value exceeded framing"))?;
        hasher.update([0x01]);
        hasher.update(label_len.to_be_bytes());
        hasher.update(label);
        hasher.update(value_len.to_be_bytes());
        hasher.update(value);
    }
    hasher.update([0xff]);
    hasher.update(field_count.to_be_bytes());
    Ok(hasher.finalize().into())
}

const fn managed_covering_index_plan_tree_is_admitted_v1(
    plan_id: i64,
    parent_id: i64,
    _auxiliary: i64,
) -> bool {
    plan_id >= 0 && parent_id == 0
}

fn managed_covering_index_plan_gate(connection: &SnapshotConnection) -> Result<()> {
    let statement = managed_prepare_fixed(
        connection,
        MANAGED_COVERING_INDEX_PLAN_QUERY,
        "covering primary-index plan gate",
        4,
        0,
    )?;
    let operation = (|| {
        let first = unsafe { ffi::sqlite3_step(statement.raw) };
        if first != ffi::SQLITE_ROW {
            bail!("managed SQLite covering primary-index plan gate returned no exact plan row");
        }
        if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER
            || unsafe { ffi::sqlite3_column_type(statement.raw, 1) } != ffi::SQLITE_INTEGER
            || unsafe { ffi::sqlite3_column_type(statement.raw, 2) } != ffi::SQLITE_INTEGER
            || unsafe { ffi::sqlite3_column_type(statement.raw, 3) } != ffi::SQLITE_TEXT
        {
            bail!("managed SQLite covering primary-index plan gate returned invalid cell types");
        }
        let plan_id = unsafe { ffi::sqlite3_column_int64(statement.raw, 0) };
        let parent_id = unsafe { ffi::sqlite3_column_int64(statement.raw, 1) };
        let auxiliary = unsafe { ffi::sqlite3_column_int64(statement.raw, 2) };
        if !managed_covering_index_plan_tree_is_admitted_v1(plan_id, parent_id, auxiliary) {
            bail!("managed SQLite covering primary-index plan gate returned an invalid plan tree");
        }
        let detail_len = unsafe { ffi::sqlite3_column_bytes(statement.raw, 3) };
        if usize::try_from(detail_len).ok() != Some(MANAGED_COVERING_INDEX_PLAN_DETAIL_V1.len()) {
            bail!("managed SQLite covering primary-index plan gate returned an unrecognized plan");
        }
        let detail = unsafe { ffi::sqlite3_column_text(statement.raw, 3) };
        if detail.is_null() {
            bail!("managed SQLite covering primary-index plan gate returned a null plan");
        }
        // SAFETY: the statement remains on this row and the exact byte length
        // was checked against a fixed small policy literal before dereference.
        let detail = unsafe {
            std::slice::from_raw_parts(detail, MANAGED_COVERING_INDEX_PLAN_DETAIL_V1.len())
        };
        if detail != MANAGED_COVERING_INDEX_PLAN_DETAIL_V1 {
            bail!("managed SQLite covering primary-index plan gate returned an unrecognized plan");
        }
        let second = unsafe { ffi::sqlite3_step(statement.raw) };
        if second != ffi::SQLITE_DONE {
            bail!("managed SQLite covering primary-index plan gate returned extra plan rows");
        }
        Ok(())
    })();
    let finalize = statement.finish();
    combine_with_postcondition(
        operation,
        if finalize == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(miette!(
                "managed SQLite covering primary-index plan finalization failed with code {finalize}"
            ))
        },
        "managed SQLite covering primary-index plan cleanup failed",
    )
}

fn scan_managed_physical_census_v1(
    connection: &SnapshotConnection,
    catalog: &ManagedSqlitePrimaryIndexCatalogV1,
    main_file_bytes: u64,
) -> Result<ManagedSqlitePhysicalCensusV1> {
    managed_require_little_endian()?;
    let runtime = managed_linked_sqlite_runtime_identity()?;
    managed_require_read_transaction(connection, "physical census entry")?;
    managed_integer_check(connection, MANAGED_MMAP_CHECK, "stable disabled mmap", 0)?;
    managed_integer_check(
        connection,
        MANAGED_CACHE_SIZE_CHECK,
        "stable bounded private page-cache target",
        -i64::from(MANAGED_PHYSICAL_CACHE_SIZE_KIB),
    )?;
    managed_integer_check(
        connection,
        MANAGED_CELL_SIZE_CHECK,
        "stable enabled cell-size checking",
        1,
    )?;
    managed_require_read_transaction(connection, "physical guardrail verification")?;

    let (main_page_count, _main_page_size) = managed_page_admission(connection, main_file_bytes)?;
    managed_require_read_transaction(connection, "physical size admission")?;

    managed_boolean_check(
        connection,
        MANAGED_QUERY_ONLY_CHECK,
        "stable query-only mode",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_MAIN_ONLY_CHECK,
        "stable main-only attachment set",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_SCHEMA_OBJECT_CHECK,
        "in-transaction exact schema objects and roots",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_TABLE_XINFO_CHECK,
        "in-transaction exact cozo table shape",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_INDEX_LIST_CHECK,
        "in-transaction exact cozo index set",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_INDEX_XINFO_CHECK,
        "in-transaction exact cozo primary-key index shape",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_MAIN_ONLY_CHECK,
        "in-transaction stable main-only attachment set",
    )?;
    managed_require_read_transaction(connection, "physical schema recheck")?;
    managed_covering_index_plan_gate(connection)?;
    managed_require_read_transaction(connection, "covering primary-index plan gate")?;
    managed_physical_test_panic(1);

    managed_integrity_check(connection)?;
    managed_require_read_transaction(connection, "full integrity check")?;
    managed_physical_test_panic(2);

    let admitted_ids = managed_admitted_relation_ids(catalog)?;
    let table = managed_table_census(connection, catalog, &admitted_ids)?;
    managed_require_read_transaction(connection, "table and point census")?;
    managed_physical_test_panic(3);

    let index = managed_covering_index_census(connection, catalog, &admitted_ids)?;
    managed_require_read_transaction(connection, "covering primary-index census")?;
    managed_physical_test_panic(4);

    if table.row_count != index.row_count {
        bail!("managed SQLite table/index cardinality mismatch");
    }
    if table.counts != index.counts {
        bail!("managed SQLite table/index per-id cardinality mismatch");
    }
    if table.counts.get(&0) != Some(&(catalog.snapshot_policy.catalog_row_count() as u64)) {
        bail!("managed SQLite table did not contain the exact cached id-zero domain");
    }
    let relation_row_counts = table
        .counts
        .into_iter()
        .map(|(relation_id, row_count)| ManagedSqliteRelationRowCountV1 {
            relation_id,
            row_count,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();

    Ok(ManagedSqlitePhysicalCensusV1 {
        main_file_bytes,
        main_page_count,
        table_row_count: table.row_count,
        index_row_count: index.row_count,
        relation_row_counts,
        table_ordered_commitment: table.ordered_commitment,
        index_ordered_commitment: index.ordered_commitment,
        progress_callbacks_used: 0,
        linked_sqlite_version_number: runtime.version_number,
        linked_sqlite_version: runtime.version,
        linked_sqlite_source_id: runtime.source_id,
        linked_sqlite_runtime_identity_fingerprint: runtime.fingerprint,
        stored_relation_id_policy_fingerprint: STORED_RELATION_ID_POLICY_FINGERPRINT_V1,
        stored_memcmp_key_policy_fingerprint: STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1,
        stored_msgpack_exact_codec_policy_fingerprint:
            STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1,
        stored_msgpack_row_policy_fingerprint: STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1,
        stored_msgpack_relation_catalog_policy_fingerprint:
            STORED_MSGPACK_RELATION_CATALOG_POLICY_FINGERPRINT_V1,
        stored_row_datavalue_codec_policy_fingerprint:
            STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1,
        policy_fingerprint: *catalog.snapshot_policy.physical_fingerprint(),
        _thread_bound: PhantomData,
    })
}

fn managed_page_admission(
    connection: &SnapshotConnection,
    main_file_bytes: u64,
) -> Result<(u64, u64)> {
    let page_count =
        managed_integer_query(connection, MANAGED_PAGE_COUNT_QUERY, "page-count admission")?;
    let page_size =
        managed_integer_query(connection, MANAGED_PAGE_SIZE_QUERY, "page-size admission")?;
    if page_count <= 0 || page_size <= 0 {
        bail!("managed SQLite page admission returned a non-positive value");
    }
    let page_count = u64::try_from(page_count)
        .map_err(|_| miette!("managed SQLite page count was not representable"))?;
    let page_size = u64::try_from(page_size)
        .map_err(|_| miette!("managed SQLite page size was not representable"))?;
    managed_validate_page_geometry(main_file_bytes, page_count, page_size)?;
    Ok((page_count, page_size))
}

fn managed_admit_file_bytes(main_file_bytes: u64) -> Result<()> {
    if main_file_bytes > MANAGED_PHYSICAL_MAX_FILE_BYTES {
        bail!("managed SQLite source exceeded the physical file-byte boundary");
    }
    Ok(())
}

fn managed_validate_page_geometry(
    main_file_bytes: u64,
    page_count: u64,
    page_size: u64,
) -> Result<()> {
    if page_count > MANAGED_PHYSICAL_MAX_PAGE_COUNT {
        bail!("managed SQLite source exceeded the physical page-count boundary");
    }
    if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
        bail!("managed SQLite source reported an invalid page size");
    }
    let represented_bytes = page_count
        .checked_mul(page_size)
        .ok_or_else(|| miette!("managed SQLite page geometry overflowed"))?;
    if represented_bytes != main_file_bytes {
        bail!("managed SQLite page geometry did not match the frozen file length");
    }
    Ok(())
}

fn managed_integrity_check(connection: &SnapshotConnection) -> Result<()> {
    let statement = managed_prepare_fixed(
        connection,
        MANAGED_INTEGRITY_CHECK_QUERY,
        "full integrity check",
        1,
        0,
    )?;
    managed_accept_exact_integrity_result(&statement)?;
    let finalize = statement.finish();
    if finalize != ffi::SQLITE_OK {
        bail!("managed SQLite full integrity check finalization failed with code {finalize}");
    }
    Ok(())
}

fn managed_accept_exact_integrity_result(statement: &ManagedStatement) -> Result<()> {
    let first = unsafe { ffi::sqlite3_step(statement.raw) };
    if first != ffi::SQLITE_ROW {
        bail!("managed SQLite full integrity check failed with code {first}");
    }
    if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_TEXT {
        bail!("managed SQLite full integrity check returned a non-TEXT result");
    }
    let bytes = unsafe { ffi::sqlite3_column_bytes(statement.raw, 0) };
    if bytes != 2 {
        bail!("managed SQLite full integrity check did not return the exact ok envelope");
    }
    let pointer = unsafe { ffi::sqlite3_column_text(statement.raw, 0) };
    if pointer.is_null() {
        bail!("managed SQLite full integrity check returned a null TEXT result");
    }
    // SAFETY: the statement remains on this SQLITE_ROW and the exact length was
    // established before the pointer was requested.
    if unsafe { std::slice::from_raw_parts(pointer, 2) } != b"ok" {
        bail!("managed SQLite full integrity check did not return the exact ok envelope");
    }
    let second = unsafe { ffi::sqlite3_step(statement.raw) };
    if second != ffi::SQLITE_DONE {
        bail!(
            "managed SQLite full integrity check returned extra rows or failed with code {second}"
        );
    }
    Ok(())
}

fn managed_admitted_relation_ids(
    catalog: &ManagedSqlitePrimaryIndexCatalogV1,
) -> Result<BTreeMap<u64, u64>> {
    if catalog.raw_rows.len() != catalog.snapshot_policy.catalog_row_count() {
        bail!("managed SQLite cached catalog raw domain violated its internal state");
    }
    let mut admitted = BTreeMap::new();
    admitted.insert(0, 0);
    for entry in catalog.catalog.entries() {
        let id = entry.relation().id();
        let decoded = RelationId::try_raw_decode(&id.to_be_bytes())
            .map_err(|_| miette!("managed SQLite catalog contained an invalid relation id"))?;
        if decoded == RelationId::SYSTEM {
            bail!("managed SQLite catalog attributed a relation to system id zero");
        }
        if admitted.insert(id, 0).is_some() {
            bail!("managed SQLite catalog repeated a top-level relation id");
        }
    }
    Ok(admitted)
}

struct ManagedPhysicalBlobCell {
    pointer: *const u8,
    len: usize,
}

impl ManagedPhysicalBlobCell {
    unsafe fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            // SAFETY: the caller upholds the current-SQLITE_ROW lifetime.
            unsafe { std::slice::from_raw_parts(self.pointer, self.len) }
        }
    }
}

struct ManagedObservedTableRow {
    rowid: i64,
    relation_id: u64,
    key: ManagedPhysicalBlobCell,
    value: ManagedPhysicalBlobCell,
}

fn managed_table_row(
    statement: &ManagedStatement,
    catalog: &ManagedSqlitePrimaryIndexCatalogV1,
    admitted_ids: &BTreeMap<u64, u64>,
) -> Result<ManagedObservedTableRow> {
    if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER
        || unsafe { ffi::sqlite3_column_type(statement.raw, 1) } != ffi::SQLITE_INTEGER
        || unsafe { ffi::sqlite3_column_type(statement.raw, 2) } != ffi::SQLITE_INTEGER
    {
        bail!("managed SQLite table census returned a non-integer rowid or length");
    }
    if unsafe { ffi::sqlite3_column_type(statement.raw, 3) } != ffi::SQLITE_BLOB
        || unsafe { ffi::sqlite3_column_type(statement.raw, 4) } != ffi::SQLITE_BLOB
    {
        bail!("managed SQLite table census encountered a non-BLOB storage cell");
    }
    let rowid = unsafe { ffi::sqlite3_column_int64(statement.raw, 0) };
    let key_len = managed_nonnegative_sql_length(statement, 1, "table key")?;
    let value_len = managed_nonnegative_sql_length(statement, 2, "table value")?;
    if key_len > MAX_ENCODED_KEY_BYTES {
        bail!("managed SQLite table key exceeded its byte boundary");
    }
    if value_len > MANAGED_CATALOG_VALUE_LIMIT {
        bail!("managed SQLite table value exceeded the id-zero byte boundary");
    }

    let key = managed_physical_blob_cell(statement, 3, key_len, "table key")?;
    // SAFETY: the table statement remains on the current SQLITE_ROW.
    let key_bytes = unsafe { key.as_slice() };
    let relation = RelationId::try_raw_decode_prefix(key_bytes)
        .map_err(|_| miette!("managed SQLite table key rejected its relation-id prefix"))?;
    let tuple = try_decode_tuple_from_key(key_bytes, 16)
        .map_err(|_| miette!("managed SQLite table key rejected by the exact decoder"))?;
    if tuple.encode_as_key(relation).as_slice() != key_bytes {
        bail!("managed SQLite table key was not canonically encoded");
    }
    let relation_id = relation.0;

    let expected_catalog_value = if relation == RelationId::SYSTEM {
        match tuple.as_slice() {
            [DataValue::Str(_)] | [DataValue::Null] => {}
            [DataValue::Null, DataValue::Str(marker)] if marker == "STORAGE_VERSION" => {}
            _ => bail!("managed SQLite table contained a foreign id-zero key shape"),
        }
        let raw = catalog
            .raw_row(key_bytes)
            .ok_or_else(|| miette!("managed SQLite table contained an unobserved id-zero key"))?;
        if value_len != raw.value.len() {
            bail!("managed SQLite table id-zero value differed from the cached catalog");
        }
        Some(raw.value.as_ref())
    } else {
        if !admitted_ids.contains_key(&relation_id) {
            bail!("managed SQLite table contained an unknown positive relation id");
        }
        if value_len > MANAGED_ROW_VALUE_LIMIT {
            bail!("managed SQLite positive-relation value exceeded its byte boundary");
        }
        None
    };

    let value = managed_physical_blob_cell(statement, 4, value_len, "table value")?;
    // SAFETY: the table statement remains on the current SQLITE_ROW.
    let value_bytes = unsafe { value.as_slice() };
    if let Some(expected) = expected_catalog_value {
        if value_bytes != expected {
            bail!("managed SQLite table id-zero value differed from the cached catalog");
        }
    } else {
        try_decode_val_only(key_bytes, value_bytes).map_err(|_| {
            miette!("managed SQLite positive-relation value rejected by the exact decoder")
        })?;
    }

    Ok(ManagedObservedTableRow {
        rowid,
        relation_id,
        key,
        value,
    })
}

fn managed_table_census(
    connection: &SnapshotConnection,
    catalog: &ManagedSqlitePrimaryIndexCatalogV1,
    admitted_ids: &BTreeMap<u64, u64>,
) -> Result<ManagedTableCensus> {
    let table =
        managed_prepare_fixed(connection, MANAGED_TABLE_CENSUS_QUERY, "table census", 5, 0)?;
    let point = managed_prepare_fixed(connection, MANAGED_POINT_QUERY, "point lookup", 5, 1)?;
    let mut counts = admitted_ids.clone();
    let mut row_count = 0_u64;
    let mut seen_id_zero = BTreeSet::new();
    let mut commitment = Sha256::new();
    commitment.update(MANAGED_PHYSICAL_TABLE_COMMITMENT_DOMAIN_V1);

    loop {
        let step = unsafe { ffi::sqlite3_step(table.raw) };
        if step == ffi::SQLITE_DONE {
            break;
        }
        if step != ffi::SQLITE_ROW {
            bail!("managed SQLite table census failed with code {step}");
        }
        let row = managed_table_row(&table, catalog, admitted_ids)?;
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite table row count overflowed"))?;
        let count = counts
            .get_mut(&row.relation_id)
            .ok_or_else(|| miette!("managed SQLite table count lost an admitted relation id"))?;
        *count = count
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite per-id table row count overflowed"))?;

        // SAFETY: neither the table statement nor its row advances until the
        // reused point lookup and commitment have finished with both slices.
        let key = unsafe { row.key.as_slice() };
        // SAFETY: same current-row lifetime as `key`.
        let value = unsafe { row.value.as_slice() };
        if row.relation_id == 0 {
            let index = catalog
                .raw_row_with_index(key)
                .map(|(index, _)| index)
                .ok_or_else(|| miette!("managed SQLite table lost a cached id-zero key"))?;
            if !seen_id_zero.insert(index) {
                bail!("managed SQLite table repeated an id-zero key");
            }
        }
        managed_verify_point_image(&point, row.rowid, key, value)?;
        update_table_commitment(&mut commitment, row.rowid, key, value)?;
    }

    if seen_id_zero.len() != catalog.raw_rows.len() {
        bail!("managed SQLite table did not contain the exact cached id-zero key domain");
    }
    let sort_count = unsafe { ffi::sqlite3_stmt_status(table.raw, ffi::SQLITE_STMTSTATUS_SORT, 0) };
    if sort_count != 0 {
        bail!("managed SQLite table census unexpectedly used temporary sort work");
    }
    let point_finalize = point.finish();
    let table_finalize = table.finish();
    if point_finalize != ffi::SQLITE_OK || table_finalize != ffi::SQLITE_OK {
        bail!(
            "managed SQLite table/point finalization failed with codes {table_finalize}/{point_finalize}"
        );
    }
    finish_physical_commitment(&mut commitment, row_count)?;
    Ok(ManagedTableCensus {
        row_count,
        counts,
        ordered_commitment: commitment.finalize().into(),
    })
}

fn managed_verify_point_image(
    statement: &ManagedStatement,
    expected_rowid: i64,
    expected_key: &[u8],
    expected_value: &[u8],
) -> Result<()> {
    managed_visit_forced_primary_index_point(
        statement,
        expected_key,
        Some(expected_rowid),
        ManagedPointValueLengthV1::Exact(expected_value.len()),
        |_returned_key, returned_value| {
            if returned_value != expected_value {
                bail!(
                    "managed SQLite forced primary-index point image differed from its table row"
                );
            }
            Ok(())
        },
    )
}

#[derive(Clone, Copy)]
enum ManagedPointValueLengthV1 {
    Exact(usize),
    AtMost(usize),
}

fn managed_select_assertion_string_pair(
    returned_key: &[u8],
    returned_value: &[u8],
    expected_key: &str,
    alternatives: &[&str],
) -> Result<u8> {
    if alternatives.is_empty() || alternatives.len() > usize::from(u8::MAX) {
        bail!("managed SQLite string-pair assertion alternatives were invalid");
    }
    let tuple = try_decode_tuple_from_kv(returned_key, returned_value, Some(2)).map_err(|_| {
        miette!("managed SQLite string-pair assertion rejected by the exact decoder")
    })?;
    let [DataValue::Str(actual_key), DataValue::Str(actual_value)] = tuple.as_slice() else {
        bail!("managed SQLite string-pair assertion returned the wrong tuple shape");
    };
    if actual_key.as_str() != expected_key {
        bail!("managed SQLite string-pair assertion returned the wrong key");
    }
    alternatives
        .iter()
        .position(|candidate| actual_value.as_str() == *candidate)
        .map(u8::try_from)
        .transpose()
        .map_err(|_| miette!("managed SQLite string-pair alternative index overflowed"))?
        .ok_or_else(|| miette!("managed SQLite string-pair assertion returned another value"))
}

fn managed_sqlite_transient_destructor() -> ffi::sqlite3_destructor_type {
    // SAFETY: SQLite defines SQLITE_TRANSIENT as the sqlite3_destructor_type
    // bit pattern -1. sqlite3-sys 0.17 does not expose the C macro helper.
    unsafe { std::mem::transmute(-1_isize) }
}

fn managed_point_test_panic_after_bind() {
    #[cfg(test)]
    if MANAGED_POINT_TEST_PANIC_AFTER_BIND.with(|panic| panic.replace(false)) {
        panic!("injected managed SQLite point panic after transient bind");
    }
}

fn managed_visit_forced_primary_index_point<T>(
    statement: &ManagedStatement,
    expected_key: &[u8],
    expected_rowid: Option<i64>,
    value_length: ManagedPointValueLengthV1,
    visitor: impl FnOnce(&[u8], &[u8]) -> Result<T>,
) -> Result<T> {
    let initial = managed_reset_and_clear(statement, "point lookup before bind");
    let operation = initial.and_then(|()| {
        if expected_key.len() > MAX_ENCODED_KEY_BYTES {
            bail!("managed SQLite point key exceeded its encoded-key byte boundary");
        }
        let key_len = i32::try_from(expected_key.len())
            .map_err(|_| miette!("managed SQLite point key length was not representable"))?;
        let bind = unsafe {
            ffi::sqlite3_bind_blob(
                statement.raw,
                1,
                expected_key.as_ptr().cast(),
                key_len,
                managed_sqlite_transient_destructor(),
            )
        };
        if bind != ffi::SQLITE_OK {
            bail!("managed SQLite point binding failed with code {bind}");
        }
        managed_point_test_panic_after_bind();
        let first = unsafe { ffi::sqlite3_step(statement.raw) };
        if first == ffi::SQLITE_DONE {
            bail!("managed SQLite forced primary-index point lookup found no row");
        }
        if first != ffi::SQLITE_ROW {
            bail!("managed SQLite forced primary-index point lookup failed with code {first}");
        }
        let row_result = (|| {
            if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER
                || unsafe { ffi::sqlite3_column_type(statement.raw, 1) } != ffi::SQLITE_INTEGER
                || unsafe { ffi::sqlite3_column_type(statement.raw, 2) } != ffi::SQLITE_INTEGER
                || unsafe { ffi::sqlite3_column_type(statement.raw, 3) } != ffi::SQLITE_BLOB
                || unsafe { ffi::sqlite3_column_type(statement.raw, 4) } != ffi::SQLITE_BLOB
            {
                bail!(
                    "managed SQLite forced primary-index point lookup returned invalid cell types"
                );
            }
            let rowid = unsafe { ffi::sqlite3_column_int64(statement.raw, 0) };
            let key_len = managed_nonnegative_sql_length(statement, 1, "point key")?;
            let value_len = managed_nonnegative_sql_length(statement, 2, "point value")?;
            if expected_rowid.is_some_and(|expected| rowid != expected)
                || key_len != expected_key.len()
            {
                bail!(
                    "managed SQLite forced primary-index point image differed from its table row"
                );
            }
            match value_length {
                ManagedPointValueLengthV1::Exact(expected) if value_len != expected => {
                    bail!(
                        "managed SQLite forced primary-index point image differed from its table row"
                    );
                }
                ManagedPointValueLengthV1::AtMost(limit) if value_len > limit => {
                    bail!(
                        "managed SQLite forced primary-index point value exceeded its byte boundary"
                    );
                }
                ManagedPointValueLengthV1::Exact(_) | ManagedPointValueLengthV1::AtMost(_) => {}
            }
            let key = managed_physical_blob_cell(statement, 3, key_len, "point key")?;
            // SAFETY: the point statement remains on the first SQLITE_ROW.
            let returned_key = unsafe { key.as_slice() };
            if returned_key != expected_key {
                bail!(
                    "managed SQLite forced primary-index point image differed from its table row"
                );
            }
            let value = managed_physical_blob_cell(statement, 4, value_len, "point value")?;
            // SAFETY: the statement remains on the first SQLITE_ROW until the
            // visitor has reduced these borrowed cells to owned/scalar state.
            visitor(returned_key, unsafe { value.as_slice() })
        })();

        let second = unsafe { ffi::sqlite3_step(statement.raw) };
        let unique = if second == ffi::SQLITE_ROW {
            Err(miette!(
                "managed SQLite forced primary-index point lookup returned multiple rows"
            ))
        } else if second != ffi::SQLITE_DONE {
            Err(miette!(
                "managed SQLite forced primary-index point lookup failed with code {second}"
            ))
        } else {
            Ok(())
        };
        combine_with_postcondition(
            row_result,
            unique,
            "managed SQLite forced primary-index point uniqueness check failed",
        )
    });
    combine_with_postcondition(
        operation,
        managed_reset_and_clear(statement, "point lookup after step"),
        "managed SQLite point lookup cleanup failed",
    )
}

fn scan_managed_record_visit_v1<S, V>(
    connection: &SnapshotConnection,
    selection: ManagedRecordVisitSelectionV1,
    mut sink: S,
    visit: &mut V,
) -> Result<ManagedRecordVisitScanOutputV1<S>>
where
    V: for<'row> FnMut(&mut S, ManagedSqliteRecordVisitEventV1<'row>) -> Result<()>,
{
    #[cfg(test)]
    if MANAGED_RECORD_VISIT_PREPARE_FAILURE.with(|failure| failure.replace(false)) {
        bail!("injected managed SQLite record-visit statement preparation failure");
    }
    let statement = managed_prepare_fixed(
        connection,
        MANAGED_RECORD_VISIT_RANGE_QUERY_V1,
        "record-visit range",
        4,
        3,
    )?;
    let mut transcript = ManagedRecordVisitTranscriptBuilderV1::new(&selection)?;
    let operation = (|| {
        for index in 0..MANAGED_RECORD_VISIT_RELATION_COUNT_V1 {
            let ordinal = u16::try_from(index + 1)
                .map_err(|_| miette!("managed SQLite record-visit ordinal overflowed"))?;
            managed_visit_relation_records_v1(
                &statement,
                ordinal,
                selection.relation_ids[index],
                selection.relation_row_counts[index],
                &mut transcript,
                &mut sink,
                visit,
            )?;
        }
        Ok(ManagedRecordVisitScanOutputV1 {
            selection,
            transcript,
            sink,
        })
    })();
    let actual_finalize = statement.finish();
    #[cfg(test)]
    let finalize = MANAGED_RECORD_VISIT_FINALIZE_CODE
        .with(|code| code.replace(None))
        .unwrap_or(actual_finalize);
    #[cfg(not(test))]
    let finalize = actual_finalize;
    combine_with_postcondition(
        operation,
        if finalize == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(miette!(
                "managed SQLite record-visit statement finalization failed with code {finalize}"
            ))
        },
        "managed SQLite record-visit statement completion also failed",
    )
}

fn managed_visit_relation_records_v1<S, V>(
    statement: &ManagedStatement,
    ordinal: u16,
    relation_id: u64,
    expected_rows: u64,
    transcript: &mut ManagedRecordVisitTranscriptBuilderV1,
    sink: &mut S,
    visit: &mut V,
) -> Result<()>
where
    V: for<'row> FnMut(&mut S, ManagedSqliteRecordVisitEventV1<'row>) -> Result<()>,
{
    let relation = RelationId::try_raw_decode(&relation_id.to_be_bytes())
        .map_err(|_| miette!("managed SQLite record-visit relation id became invalid"))?;
    if relation == RelationId::SYSTEM {
        bail!("managed SQLite record-visit relation id became zero");
    }
    let lower = relation.raw_encode();
    let upper = relation.next().raw_encode();
    let limit = i64::try_from(
        expected_rows
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite record-visit range limit overflowed"))?,
    )
    .map_err(|_| miette!("managed SQLite record-visit range limit was not representable"))?;

    let initial = managed_record_visit_reset_and_clear(statement, "record-visit range before bind");
    let operation = initial.and_then(|()| {
        for (parameter, bytes) in [(1, lower.as_slice()), (2, upper.as_slice())] {
            let length = i32::try_from(bytes.len())
                .map_err(|_| miette!("managed SQLite record-visit bound length was invalid"))?;
            let code = unsafe {
                ffi::sqlite3_bind_blob(
                    statement.raw,
                    parameter,
                    bytes.as_ptr().cast(),
                    length,
                    managed_sqlite_transient_destructor(),
                )
            };
            if code != ffi::SQLITE_OK {
                bail!("managed SQLite record-visit range binding failed with code {code}");
            }
        }
        let bind_limit = unsafe { ffi::sqlite3_bind_int64(statement.raw, 3, limit) };
        if bind_limit != ffi::SQLITE_OK {
            bail!("managed SQLite record-visit range limit binding failed with code {bind_limit}");
        }

        transcript.begin_relation(ordinal, relation_id, expected_rows)?;
        visit(
            sink,
            ManagedSqliteRecordVisitEventV1::BeginRelation {
                ordinal,
                expected_rows,
            },
        )?;

        let mut previous_key = Vec::new();
        let mut rows = 0_u64;
        loop {
            let step = unsafe { ffi::sqlite3_step(statement.raw) };
            if step == ffi::SQLITE_DONE {
                break;
            }
            if step != ffi::SQLITE_ROW {
                bail!("managed SQLite record-visit range failed with code {step}");
            }
            rows = rows
                .checked_add(1)
                .ok_or_else(|| miette!("managed SQLite record-visit row count overflowed"))?;
            if rows > expected_rows {
                bail!("managed SQLite record-visit range found an extra row");
            }
            let (key, value) = managed_record_visit_row_v1(
                statement,
                relation,
                &lower,
                &mut previous_key,
                transcript,
                ordinal,
            )?;
            visit(
                sink,
                ManagedSqliteRecordVisitEventV1::Record {
                    ordinal,
                    key: key.as_slice(),
                    value: value.as_slice(),
                },
            )?;
        }
        if rows != expected_rows {
            bail!("managed SQLite record-visit range did not match its physical row count");
        }
        Ok(rows)
    });

    let sort = unsafe { ffi::sqlite3_stmt_status(statement.raw, ffi::SQLITE_STMTSTATUS_SORT, 1) };
    let fullscan =
        unsafe { ffi::sqlite3_stmt_status(statement.raw, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1) };
    let operation = combine_with_postcondition(
        operation,
        if sort == 0 && fullscan == 0 {
            Ok(())
        } else {
            Err(miette!(
                "managed SQLite record-visit range left its forced primary-index path"
            ))
        },
        "managed SQLite record-visit query-plan postcondition failed",
    )
    .and_then(|rows| {
        transcript.end_relation(ordinal, rows)?;
        visit(
            sink,
            ManagedSqliteRecordVisitEventV1::EndRelation { ordinal, rows },
        )
    });

    combine_with_postcondition(
        operation,
        managed_record_visit_reset_and_clear(statement, "record-visit range after step"),
        "managed SQLite record-visit range cleanup failed",
    )
}

fn managed_record_visit_row_v1(
    statement: &ManagedStatement,
    expected_relation: RelationId,
    expected_prefix: &[u8; 8],
    previous_key: &mut Vec<u8>,
    transcript: &mut ManagedRecordVisitTranscriptBuilderV1,
    ordinal: u16,
) -> Result<(Tuple, Tuple)> {
    if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER
        || unsafe { ffi::sqlite3_column_type(statement.raw, 1) } != ffi::SQLITE_INTEGER
    {
        bail!("managed SQLite record-visit range returned non-integer lengths");
    }
    if unsafe { ffi::sqlite3_column_type(statement.raw, 2) } != ffi::SQLITE_BLOB
        || unsafe { ffi::sqlite3_column_type(statement.raw, 3) } != ffi::SQLITE_BLOB
    {
        bail!("managed SQLite record-visit range returned non-BLOB storage cells");
    }

    // Both SQL lengths and both allocation-independent fuses are checked before
    // requesting either BLOB pointer or invoking either decoder.
    let key_len = managed_nonnegative_sql_length(statement, 0, "record-visit key")?;
    let value_len = managed_nonnegative_sql_length(statement, 1, "record-visit value")?;
    if key_len > MAX_ENCODED_KEY_BYTES {
        bail!("managed SQLite record-visit key exceeded its byte boundary");
    }
    if value_len > MANAGED_ROW_VALUE_LIMIT {
        bail!("managed SQLite record-visit value exceeded its byte boundary");
    }

    let key_cell = managed_physical_blob_cell(statement, 2, key_len, "record-visit key")?;
    // SAFETY: the reused range statement remains on this SQLITE_ROW through all
    // raw-byte, decode, transcript, and callback work below.
    let raw_key = unsafe { key_cell.as_slice() };
    if raw_key.get(..8) != Some(expected_prefix.as_slice()) {
        bail!("managed SQLite record-visit key had a foreign relation prefix");
    }
    if !previous_key.is_empty() && previous_key.as_slice() >= raw_key {
        bail!("managed SQLite record-visit keys were not strictly increasing");
    }
    let relation = RelationId::try_raw_decode_prefix(raw_key)
        .map_err(|_| miette!("managed SQLite record-visit key relation prefix was invalid"))?;
    if relation != expected_relation {
        bail!("managed SQLite record-visit key had a foreign relation prefix");
    }
    let key = try_decode_tuple_from_key(raw_key, 16)
        .map_err(|_| miette!("managed SQLite record-visit key rejected by the exact decoder"))?;
    if key.encode_as_key(expected_relation).as_slice() != raw_key {
        bail!("managed SQLite record-visit key was not canonically encoded");
    }

    let value_cell = managed_physical_blob_cell(statement, 3, value_len, "record-visit value")?;
    // SAFETY: the statement is still on the same SQLITE_ROW.
    let raw_value = unsafe { value_cell.as_slice() };
    let value = try_decode_val_only(raw_key, raw_value)
        .map_err(|_| miette!("managed SQLite record-visit value rejected by the exact decoder"))?;
    transcript.record(ordinal, raw_key, raw_value)?;
    previous_key.clear();
    previous_key.extend_from_slice(raw_key);
    Ok((key, value))
}

fn managed_record_visit_reset_and_clear(
    statement: &ManagedStatement,
    label: &'static str,
) -> Result<()> {
    let result = managed_reset_and_clear(statement, label);
    #[cfg(test)]
    let result = combine_with_postcondition(
        result,
        if MANAGED_RECORD_VISIT_RESET_FAILURE.with(|failure| failure.replace(false)) {
            Err(miette!(
                "injected managed SQLite record-visit reset failure"
            ))
        } else {
            Ok(())
        },
        "managed SQLite injected record-visit reset failure",
    );
    #[cfg(test)]
    let result = combine_with_postcondition(
        result,
        if MANAGED_RECORD_VISIT_CLEAR_FAILURE.with(|failure| failure.replace(false)) {
            Err(miette!(
                "injected managed SQLite record-visit clear failure"
            ))
        } else {
            Ok(())
        },
        "managed SQLite injected record-visit clear failure",
    );
    result
}

fn managed_covering_index_census(
    connection: &SnapshotConnection,
    catalog: &ManagedSqlitePrimaryIndexCatalogV1,
    admitted_ids: &BTreeMap<u64, u64>,
) -> Result<ManagedIndexCensus> {
    let statement = managed_prepare_fixed(
        connection,
        MANAGED_COVERING_INDEX_QUERY,
        "covering primary-index census",
        3,
        0,
    )?;
    let mut counts = admitted_ids.clone();
    let mut row_count = 0_u64;
    let mut previous_key = Vec::new();
    let mut seen_id_zero = BTreeSet::new();
    let mut commitment = Sha256::new();
    commitment.update(MANAGED_PHYSICAL_INDEX_COMMITMENT_DOMAIN_V1);

    loop {
        let step = unsafe { ffi::sqlite3_step(statement.raw) };
        if step == ffi::SQLITE_DONE {
            break;
        }
        if step != ffi::SQLITE_ROW {
            bail!("managed SQLite covering primary-index census failed with code {step}");
        }
        if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER
            || unsafe { ffi::sqlite3_column_type(statement.raw, 1) } != ffi::SQLITE_BLOB
            || unsafe { ffi::sqlite3_column_type(statement.raw, 2) } != ffi::SQLITE_INTEGER
        {
            bail!("managed SQLite covering primary-index census returned invalid cell types");
        }
        let key_len = managed_nonnegative_sql_length(&statement, 0, "covering-index key")?;
        if key_len > MAX_ENCODED_KEY_BYTES {
            bail!("managed SQLite covering-index key exceeded its byte boundary");
        }
        let key = managed_physical_blob_cell(&statement, 1, key_len, "covering-index key")?;
        // SAFETY: the statement remains on the current SQLITE_ROW.
        let key_bytes = unsafe { key.as_slice() };
        if !previous_key.is_empty() && previous_key.as_slice() >= key_bytes {
            bail!("managed SQLite covering primary-index keys were not strictly ordered");
        }
        let relation = RelationId::try_raw_decode_prefix(key_bytes).map_err(|_| {
            miette!("managed SQLite covering-index key rejected its relation-id prefix")
        })?;
        let tuple = try_decode_tuple_from_key(key_bytes, 16).map_err(|_| {
            miette!("managed SQLite covering-index key rejected by the exact decoder")
        })?;
        if tuple.encode_as_key(relation).as_slice() != key_bytes {
            bail!("managed SQLite covering-index key was not canonically encoded");
        }
        let relation_id = relation.0;
        if relation == RelationId::SYSTEM {
            let index = catalog
                .raw_row_with_index(key_bytes)
                .map(|(index, _)| index)
                .ok_or_else(|| {
                    miette!("managed SQLite covering index contained an unobserved id-zero key")
                })?;
            if !seen_id_zero.insert(index) {
                bail!("managed SQLite covering index repeated an id-zero key");
            }
        } else if !admitted_ids.contains_key(&relation_id) {
            bail!("managed SQLite covering index contained an unknown positive relation id");
        }
        let rowid = unsafe { ffi::sqlite3_column_int64(statement.raw, 2) };
        row_count = row_count
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite covering-index row count overflowed"))?;
        let count = counts.get_mut(&relation_id).ok_or_else(|| {
            miette!("managed SQLite covering-index count lost an admitted relation id")
        })?;
        *count = count
            .checked_add(1)
            .ok_or_else(|| miette!("managed SQLite per-id covering-index count overflowed"))?;
        update_index_commitment(&mut commitment, key_bytes, rowid)?;
        previous_key.clear();
        previous_key.extend_from_slice(key_bytes);
    }

    if seen_id_zero.len() != catalog.raw_rows.len() {
        bail!("managed SQLite covering index did not contain the exact cached id-zero key domain");
    }
    let sort_count =
        unsafe { ffi::sqlite3_stmt_status(statement.raw, ffi::SQLITE_STMTSTATUS_SORT, 0) };
    if sort_count != 0 {
        bail!("managed SQLite covering primary-index census used temporary sort work");
    }
    let finalize = statement.finish();
    if finalize != ffi::SQLITE_OK {
        bail!("managed SQLite covering primary-index finalization failed with code {finalize}");
    }
    finish_physical_commitment(&mut commitment, row_count)?;
    Ok(ManagedIndexCensus {
        row_count,
        counts,
        ordered_commitment: commitment.finalize().into(),
    })
}

fn managed_nonnegative_sql_length(
    statement: &ManagedStatement,
    column: i32,
    label: &'static str,
) -> Result<usize> {
    let length = unsafe { ffi::sqlite3_column_int64(statement.raw, column) };
    if length < 0 {
        bail!("managed SQLite {label} reported a negative length");
    }
    usize::try_from(length)
        .map_err(|_| miette!("managed SQLite {label} length was not representable"))
}

fn managed_physical_blob_cell(
    statement: &ManagedStatement,
    column: i32,
    expected_len: usize,
    label: &'static str,
) -> Result<ManagedPhysicalBlobCell> {
    let observed = unsafe { ffi::sqlite3_column_bytes(statement.raw, column) };
    if observed < 0 || usize::try_from(observed).ok() != Some(expected_len) {
        bail!("managed SQLite {label} SQL and BLOB lengths disagreed");
    }
    #[cfg(test)]
    MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.set(requests.get() + 1));
    let pointer = unsafe { ffi::sqlite3_column_blob(statement.raw, column) }.cast::<u8>();
    if expected_len != 0 && pointer.is_null() {
        bail!("managed SQLite {label} returned a null nonempty BLOB");
    }
    Ok(ManagedPhysicalBlobCell {
        pointer,
        len: expected_len,
    })
}

fn managed_reset_and_clear(statement: &ManagedStatement, label: &'static str) -> Result<()> {
    let reset = unsafe { ffi::sqlite3_reset(statement.raw) };
    let clear = unsafe { ffi::sqlite3_clear_bindings(statement.raw) };
    if reset != ffi::SQLITE_OK || clear != ffi::SQLITE_OK {
        bail!("managed SQLite {label} failed with reset/clear codes {reset}/{clear}");
    }
    Ok(())
}

fn managed_catalog_fence_reset_and_clear(
    statement: &ManagedStatement,
    label: &'static str,
) -> Result<()> {
    let result = managed_reset_and_clear(statement, label);
    #[cfg(test)]
    let result = combine_with_postcondition(
        result,
        if MANAGED_CATALOG_FENCE_RANGE_RESET_FAILURE.with(|failure| failure.replace(false)) {
            Err(miette!(
                "injected managed SQLite catalog-fence range reset failure"
            ))
        } else {
            Ok(())
        },
        "managed SQLite catalog-fence injected range reset failure",
    );
    #[cfg(test)]
    let result = combine_with_postcondition(
        result,
        if MANAGED_CATALOG_FENCE_RANGE_CLEAR_FAILURE.with(|failure| failure.replace(false)) {
            Err(miette!(
                "injected managed SQLite catalog-fence range clear failure"
            ))
        } else {
            Ok(())
        },
        "managed SQLite catalog-fence injected range clear failure",
    );
    result
}

fn update_table_commitment(
    hasher: &mut Sha256,
    rowid: i64,
    key: &[u8],
    value: &[u8],
) -> Result<()> {
    let key_len = u64::try_from(key.len())
        .map_err(|_| miette!("managed SQLite table key exceeded commitment width"))?;
    let value_len = u64::try_from(value.len())
        .map_err(|_| miette!("managed SQLite table value exceeded commitment width"))?;
    hasher.update(MANAGED_COMMITMENT_ROW_MARKER_V1);
    hasher.update(rowid.to_be_bytes());
    hasher.update(key_len.to_be_bytes());
    hasher.update(key);
    hasher.update(value_len.to_be_bytes());
    hasher.update(value);
    Ok(())
}

fn update_index_commitment(hasher: &mut Sha256, key: &[u8], rowid: i64) -> Result<()> {
    let key_len = u64::try_from(key.len())
        .map_err(|_| miette!("managed SQLite covering-index key exceeded commitment width"))?;
    hasher.update(MANAGED_COMMITMENT_ROW_MARKER_V1);
    hasher.update(key_len.to_be_bytes());
    hasher.update(key);
    hasher.update(rowid.to_be_bytes());
    Ok(())
}

fn finish_physical_commitment(hasher: &mut Sha256, row_count: u64) -> Result<()> {
    hasher.update(MANAGED_COMMITMENT_TERMINATOR_V1);
    hasher.update(row_count.to_be_bytes());
    Ok(())
}

fn managed_require_read_transaction(
    connection: &SnapshotConnection,
    phase: &'static str,
) -> Result<()> {
    let autocommit = unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) };
    let transaction =
        unsafe { ffi::sqlite3_txn_state(connection.as_raw(), SQLITE_MAIN_SCHEMA.as_ptr().cast()) };
    if autocommit != 0 || transaction != ffi::SQLITE_TXN_READ {
        bail!("managed SQLite lost its pinned READ transaction after {phase}");
    }
    Ok(())
}

fn managed_physical_test_panic(phase: u8) {
    #[cfg(test)]
    MANAGED_PHYSICAL_TEST_PANIC_PHASE.with(|requested| {
        if requested.get() == phase {
            requested.set(0);
            panic!("injected managed SQLite physical census panic at phase {phase}");
        }
    });
    #[cfg(not(test))]
    let _ = phase;
}

struct SnapshotConnection {
    raw: *mut ffi::sqlite3,
}

impl SnapshotConnection {
    fn open(filename: &CStr, flags: i32, role: &str) -> Result<Self> {
        let mut raw = ptr::null_mut();
        let code = unsafe { ffi::sqlite3_open_v2(filename.as_ptr(), &mut raw, flags, ptr::null()) };
        if code != ffi::SQLITE_OK {
            let detail = sqlite_error_detail(raw, code);
            if !raw.is_null() {
                unsafe {
                    ffi::sqlite3_close(raw);
                }
            }
            bail!("failed to open {role}: {detail}");
        }
        if raw.is_null() {
            bail!("failed to open {role}: SQLite returned a null connection");
        }
        Ok(Self { raw })
    }

    fn as_raw(&self) -> *mut ffi::sqlite3 {
        self.raw
    }

    fn close(mut self, role: &str) -> Result<()> {
        let raw = std::mem::replace(&mut self.raw, ptr::null_mut());
        let code = unsafe { ffi::sqlite3_close(raw) };
        if code != ffi::SQLITE_OK {
            // A strict close failure is an internal-lifecycle violation, so it
            // must withhold readiness evidence. Hand the connection to
            // close_v2 as a zombie before returning: outstanding statements
            // can then finalize safely without losing the only cleanup path.
            let deferred = unsafe { ffi::sqlite3_close_v2(raw) };
            if deferred != ffi::SQLITE_OK {
                self.raw = raw;
                bail!(
                    "failed to close {role}: SQLite code {code}; deferred cleanup failed with code {deferred}"
                );
            }
            bail!("failed to close {role}: SQLite code {code}; deferred cleanup was armed");
        }
        Ok(())
    }
}

impl Drop for SnapshotConnection {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                // Drop cannot report a lifecycle error. close_v2 at least
                // preserves SQLite's deferred-cleanup contract if a forgotten
                // statement or backup still exists.
                ffi::sqlite3_close_v2(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

struct ManagedStatement {
    raw: *mut ffi::sqlite3_stmt,
}

impl ManagedStatement {
    fn finish(mut self) -> i32 {
        let raw = std::mem::replace(&mut self.raw, ptr::null_mut());
        unsafe { ffi::sqlite3_finalize(raw) }
    }
}

impl Drop for ManagedStatement {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                ffi::sqlite3_finalize(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

fn managed_prepare_fixed(
    connection: &SnapshotConnection,
    sql: &'static [u8],
    label: &'static str,
    column_count: i32,
    parameter_count: i32,
) -> Result<ManagedStatement> {
    if sql.last() != Some(&0) || sql[..sql.len().saturating_sub(1)].contains(&0) {
        bail!("managed SQLite internal {label} query was not exactly NUL-terminated");
    }
    let mut raw = ptr::null_mut();
    let mut tail = ptr::null();
    let code = unsafe {
        ffi::sqlite3_prepare_v3(
            connection.as_raw(),
            sql.as_ptr().cast(),
            -1,
            ffi::SQLITE_PREPARE_PERSISTENT as u32,
            &mut raw,
            &mut tail,
        )
    };
    if code != ffi::SQLITE_OK {
        if !raw.is_null() {
            unsafe {
                ffi::sqlite3_finalize(raw);
            }
        }
        bail!("managed SQLite {label} query preparation failed with code {code}");
    }
    if raw.is_null() {
        bail!("managed SQLite {label} query preparation returned no statement");
    }
    let statement = ManagedStatement { raw };
    if tail.is_null() || unsafe { *tail } != 0 {
        bail!("managed SQLite internal {label} query contained trailing SQL");
    }
    if unsafe { ffi::sqlite3_stmt_readonly(statement.raw) } != 1 {
        bail!("managed SQLite internal {label} query was not read-only");
    }
    if unsafe { ffi::sqlite3_column_count(statement.raw) } != column_count
        || unsafe { ffi::sqlite3_bind_parameter_count(statement.raw) } != parameter_count
    {
        bail!("managed SQLite internal {label} query had an invalid shape");
    }
    Ok(statement)
}

fn configure_managed_snapshot_reader(connection: &SnapshotConnection) -> Result<()> {
    configure_snapshot_source_guardrails(connection)?;
    configure_managed_physical_guardrails(connection)?;

    let readonly = unsafe {
        ffi::sqlite3_db_readonly(connection.as_raw(), SQLITE_MAIN_SCHEMA.as_ptr().cast())
    };
    if readonly != 1 {
        bail!("managed SQLite readiness rejected a non-read-only main database");
    }
    if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } != 1 {
        bail!("managed SQLite readiness rejected an active transaction");
    }
    let changes_before = unsafe { ffi::sqlite3_total_changes64(connection.as_raw()) };

    managed_boolean_check(connection, MANAGED_QUERY_ONLY_CHECK, "query-only mode")?;
    managed_boolean_check(
        connection,
        MANAGED_MAIN_ONLY_CHECK,
        "main-only attachment set",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_SCHEMA_OBJECT_CHECK,
        "exact schema objects",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_TABLE_XINFO_CHECK,
        "exact cozo table shape",
    )?;
    managed_boolean_check(connection, MANAGED_INDEX_LIST_CHECK, "exact cozo index set")?;
    managed_boolean_check(
        connection,
        MANAGED_INDEX_XINFO_CHECK,
        "exact cozo primary-key index shape",
    )?;
    managed_boolean_check(
        connection,
        MANAGED_MAIN_ONLY_CHECK,
        "stable main-only attachment set",
    )?;

    if unsafe { ffi::sqlite3_total_changes64(connection.as_raw()) } != changes_before {
        bail!("managed SQLite readiness unexpectedly changed the database");
    }
    if unsafe { ffi::sqlite3_get_autocommit(connection.as_raw()) } != 1 {
        bail!("managed SQLite readiness left an active transaction");
    }
    Ok(())
}

fn configure_managed_physical_guardrails(connection: &SnapshotConnection) -> Result<()> {
    // mmap_size cannot be changed while a read transaction is active, so these
    // physical-audit controls are installed before the catalog can pin one.
    managed_exec_fixed(connection, MANAGED_MMAP_DISABLE_SQL, "disabled mmap")?;
    managed_exec_fixed(
        connection,
        MANAGED_CACHE_SIZE_SQL,
        "bounded private page-cache target",
    )?;
    managed_exec_fixed(
        connection,
        MANAGED_CELL_SIZE_CHECK_SQL,
        "cell-size checking",
    )?;
    managed_integer_check(connection, MANAGED_MMAP_CHECK, "disabled mmap", 0)?;
    managed_integer_check(
        connection,
        MANAGED_CACHE_SIZE_CHECK,
        "bounded private page-cache target",
        -i64::from(MANAGED_PHYSICAL_CACHE_SIZE_KIB),
    )?;
    managed_integer_check(
        connection,
        MANAGED_CELL_SIZE_CHECK,
        "enabled cell-size checking",
        1,
    )?;
    Ok(())
}

fn configure_snapshot_source_guardrails(connection: &SnapshotConnection) -> Result<()> {
    // These per-connection limits are an allocation fuse for SQLite work we
    // induce, including hostile schema records forced through the fixed probe.
    // They are defense in depth, not a proof of bounded total process memory:
    // SQLite and the VFS still have allocations outside sqlite3_limit().
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_LENGTH,
        MANAGED_SQLITE_SCHEMA_LENGTH_LIMIT,
        "pre-schema value length",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_SQL_LENGTH,
        MANAGED_SQLITE_SQL_LENGTH_LIMIT,
        "SQL length",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_COLUMN,
        MANAGED_SQLITE_COLUMN_LIMIT,
        "column count",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_EXPR_DEPTH,
        MANAGED_SQLITE_EXPR_DEPTH_LIMIT,
        "expression depth",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_COMPOUND_SELECT,
        MANAGED_SQLITE_COMPOUND_SELECT_LIMIT,
        "compound SELECT terms",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_VDBE_OP,
        MANAGED_SQLITE_VDBE_OP_LIMIT,
        "virtual-machine opcodes",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_FUNCTION_ARG,
        MANAGED_SQLITE_FUNCTION_ARG_LIMIT,
        "function arguments",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_LIKE_PATTERN_LENGTH,
        MANAGED_SQLITE_LIKE_PATTERN_LIMIT,
        "LIKE pattern length",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_VARIABLE_NUMBER,
        MANAGED_SQLITE_VARIABLE_LIMIT,
        "bound variables",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_ATTACHED,
        MANAGED_SQLITE_ATTACHED_LIMIT,
        "attached databases",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_TRIGGER_DEPTH,
        MANAGED_SQLITE_TRIGGER_DEPTH_LIMIT,
        "trigger depth",
    )?;
    managed_set_limit(
        connection,
        ffi::SQLITE_LIMIT_WORKER_THREADS,
        MANAGED_SQLITE_WORKER_THREADS_LIMIT,
        "worker threads",
    )?;

    managed_set_db_config(
        connection,
        ffi::SQLITE_DBCONFIG_DEFENSIVE,
        MANAGED_SQLITE_DEFENSIVE_VALUE,
        "defensive mode",
    )?;
    managed_set_db_config(
        connection,
        ffi::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        MANAGED_SQLITE_TRUSTED_SCHEMA_VALUE,
        "untrusted schema mode",
    )?;
    managed_set_db_config(
        connection,
        ffi::SQLITE_DBCONFIG_ENABLE_TRIGGER,
        MANAGED_SQLITE_ENABLE_TRIGGER_VALUE,
        "disabled triggers",
    )?;
    managed_exec_fixed(connection, MANAGED_QUERY_ONLY_SQL, "query-only mode")?;
    Ok(())
}

fn managed_set_limit(
    connection: &SnapshotConnection,
    category: i32,
    value: i32,
    label: &'static str,
) -> Result<()> {
    unsafe {
        ffi::sqlite3_limit(connection.as_raw(), category, value);
    }
    let observed = unsafe { ffi::sqlite3_limit(connection.as_raw(), category, -1) };
    if observed != value {
        bail!(
            "managed SQLite could not enforce the {label} limit (requested {value}, observed {observed})"
        );
    }
    Ok(())
}

fn managed_set_db_config(
    connection: &SnapshotConnection,
    option: i32,
    value: i32,
    label: &'static str,
) -> Result<()> {
    let mut observed = -1_i32;
    let code = unsafe {
        ffi::sqlite3_db_config(
            connection.as_raw(),
            option,
            value,
            &mut observed as *mut i32,
        )
    };
    if code != ffi::SQLITE_OK || observed != value {
        bail!("managed SQLite could not enforce {label} (SQLite code {code}, observed {observed})");
    }
    Ok(())
}

fn managed_exec_fixed(
    connection: &SnapshotConnection,
    sql: &'static [u8],
    label: &'static str,
) -> Result<()> {
    if sql.last() != Some(&0) || sql[..sql.len().saturating_sub(1)].contains(&0) {
        bail!("managed SQLite internal {label} statement was not exactly NUL-terminated");
    }
    let code = unsafe {
        ffi::sqlite3_exec(
            connection.as_raw(),
            sql.as_ptr().cast(),
            None,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if code != ffi::SQLITE_OK {
        bail!("managed SQLite could not enforce {label}: SQLite code {code}");
    }
    Ok(())
}

fn managed_boolean_check(
    connection: &SnapshotConnection,
    sql: &'static [u8],
    label: &'static str,
) -> Result<()> {
    if sql.last() != Some(&0) || sql[..sql.len().saturating_sub(1)].contains(&0) {
        bail!("managed SQLite internal {label} query was not exactly NUL-terminated");
    }

    let mut raw = ptr::null_mut();
    let mut tail = ptr::null();
    let code = unsafe {
        ffi::sqlite3_prepare_v3(
            connection.as_raw(),
            sql.as_ptr().cast(),
            -1,
            ffi::SQLITE_PREPARE_PERSISTENT as u32,
            &mut raw,
            &mut tail,
        )
    };
    if code != ffi::SQLITE_OK {
        if !raw.is_null() {
            unsafe {
                ffi::sqlite3_finalize(raw);
            }
        }
        bail!("managed SQLite {label} query preparation failed with code {code}");
    }
    if raw.is_null() {
        bail!("managed SQLite {label} query preparation returned no statement");
    }
    let statement = ManagedStatement { raw };

    if tail.is_null() || unsafe { *tail } != 0 {
        bail!("managed SQLite internal {label} query contained trailing SQL");
    }
    if unsafe { ffi::sqlite3_stmt_readonly(statement.raw) } != 1 {
        bail!("managed SQLite internal {label} query was not read-only");
    }
    if unsafe { ffi::sqlite3_column_count(statement.raw) } != 1 {
        bail!("managed SQLite internal {label} query had an invalid result shape");
    }

    let first = unsafe { ffi::sqlite3_step(statement.raw) };
    if first != ffi::SQLITE_ROW {
        bail!("managed SQLite {label} query failed with code {first}");
    }
    if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER {
        bail!("managed SQLite {label} query returned a non-integer result");
    }
    let accepted = unsafe { ffi::sqlite3_column_int64(statement.raw, 0) }
        == i64::from(MANAGED_SQLITE_QUERY_ONLY_VALUE);
    let second = unsafe { ffi::sqlite3_step(statement.raw) };
    if second != ffi::SQLITE_DONE {
        bail!("managed SQLite {label} query returned extra rows or failed with code {second}");
    }
    let finalize = statement.finish();
    if finalize != ffi::SQLITE_OK {
        bail!("managed SQLite {label} query finalization failed with code {finalize}");
    }
    if !accepted {
        bail!("managed SQLite readiness rejected {label}");
    }
    Ok(())
}

fn managed_integer_check(
    connection: &SnapshotConnection,
    sql: &'static [u8],
    label: &'static str,
    expected: i64,
) -> Result<()> {
    let observed = managed_integer_query(connection, sql, label)?;
    if observed != expected {
        bail!("managed SQLite rejected {label} (expected {expected}, observed {observed})");
    }
    Ok(())
}

fn managed_integer_query(
    connection: &SnapshotConnection,
    sql: &'static [u8],
    label: &'static str,
) -> Result<i64> {
    let statement = managed_prepare_fixed(connection, sql, label, 1, 0)?;
    let first = unsafe { ffi::sqlite3_step(statement.raw) };
    if first != ffi::SQLITE_ROW {
        bail!("managed SQLite {label} query failed with code {first}");
    }
    if unsafe { ffi::sqlite3_column_type(statement.raw, 0) } != ffi::SQLITE_INTEGER {
        bail!("managed SQLite {label} query returned a non-integer result");
    }
    let observed = unsafe { ffi::sqlite3_column_int64(statement.raw, 0) };
    let second = unsafe { ffi::sqlite3_step(statement.raw) };
    if second != ffi::SQLITE_DONE {
        bail!("managed SQLite {label} query returned extra rows or failed with code {second}");
    }
    let finalize = statement.finish();
    if finalize != ffi::SQLITE_OK {
        bail!("managed SQLite {label} query finalization failed with code {finalize}");
    }
    Ok(observed)
}

struct SnapshotBackup {
    raw: *mut ffi::sqlite3_backup,
}

impl SnapshotBackup {
    fn finish(mut self) -> i32 {
        let raw = std::mem::replace(&mut self.raw, ptr::null_mut());
        unsafe { ffi::sqlite3_backup_finish(raw) }
    }
}

impl Drop for SnapshotBackup {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                ffi::sqlite3_backup_finish(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

fn native_backup(destination: &SnapshotConnection, source: &SnapshotConnection) -> Result<()> {
    let raw = unsafe {
        ffi::sqlite3_backup_init(
            destination.as_raw(),
            SQLITE_MAIN_SCHEMA.as_ptr().cast(),
            source.as_raw(),
            SQLITE_MAIN_SCHEMA.as_ptr().cast(),
        )
    };
    if raw.is_null() {
        let code = unsafe { ffi::sqlite3_errcode(destination.as_raw()) };
        bail!(
            "failed to initialize native SQLite backup: {}",
            sqlite_error_detail(destination.as_raw(), code)
        );
    }

    let backup = SnapshotBackup { raw };
    let step_code = unsafe { ffi::sqlite3_backup_step(backup.raw, -1) };
    let step_detail = if step_code == ffi::SQLITE_DONE {
        None
    } else {
        Some(sqlite_error_detail(destination.as_raw(), step_code))
    };
    let finish_code = backup.finish();

    if let Some(detail) = step_detail {
        bail!("native SQLite backup did not complete: {detail}");
    }
    if finish_code != ffi::SQLITE_OK {
        bail!(
            "failed to finish native SQLite backup: {}",
            sqlite_error_detail(destination.as_raw(), finish_code)
        );
    }
    Ok(())
}

fn validate_sqlite_schema(connection: &SnapshotConnection) -> Result<()> {
    let code = unsafe {
        ffi::sqlite3_exec(
            connection.as_raw(),
            SQLITE_SCHEMA_PROBE.as_ptr().cast(),
            None,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if code != ffi::SQLITE_OK {
        bail!("snapshot source schema probe failed with SQLite code {code}");
    }
    Ok(())
}

fn sqlite_error_detail(connection: *mut ffi::sqlite3, code: i32) -> String {
    if connection.is_null() {
        return format!("SQLite error code {code}");
    }
    let message = unsafe { ffi::sqlite3_errmsg(connection) };
    if message.is_null() {
        format!("SQLite error code {code}")
    } else {
        let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        format!("{message} (code {code})")
    }
}

fn ensure_snapshot_sqlite_runtime() -> Result<()> {
    let linked = unsafe { ffi::sqlite3_libversion_number() };
    if linked < MIN_SNAPSHOT_SQLITE_VERSION {
        bail!(
            "existing-only SQLite snapshots require linked SQLite >= 3.31.0 for immutable read-only WAL handling plus SQLITE_OPEN_NOFOLLOW; found numeric version {linked}"
        );
    }
    Ok(())
}

fn ensure_absolute_path(path: &Path, role: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{role} path must be absolute, got {:?}", path);
    }
    Ok(())
}

struct FrozenSourceMainFile {
    #[cfg(unix)]
    file: File,
    #[cfg(unix)]
    identity: FrozenMainFileIdentity,
}

/// A duplicated create-new destination descriptor retained only to preserve the
/// continuous identity-custody chain into a private backup source.
///
/// Unlike [`FrozenSourceMainFile`], this descriptor inherits the destination
/// creator's read-write access mode and is the only descriptor retained directly
/// from that `create_new` lineage. It is never exposed and this type offers no
/// mutation method; the separately opened SQLite connection and ordinary frozen
/// main descriptor remain read-only. The custody proof therefore costs one file
/// descriptor and retains latent write capability. It can keep comparing creation
/// provenance, the independent read-only descriptor, and the current pathname, but
/// does not bind SQLite's path-opened handle or close the same-authority rename-ABA gap.
struct RetainedBackupDestinationIdentity {
    #[cfg(unix)]
    file: File,
    #[cfg(unix)]
    identity: FrozenMainFileIdentity,
}

impl RetainedBackupDestinationIdentity {
    fn verify(&self, path: &Path, role: &str) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = (path, role);
            bail!(
                "existing-only SQLite backup custody requires Unix lstat/fstat file identity; \
                 this target has no implemented equivalent and is refused"
            );
        }

        #[cfg(unix)]
        {
            let descriptor_before = FrozenMainFileIdentity::from_metadata(
                &self.file.metadata().map_err(|error| {
                    miette!("cannot fstat {role} descriptor {:?}: {error}", path)
                })?,
                path,
                role,
            )?;
            let named_before = frozen_named_main_identity(path, role)?;
            let descriptor_after = FrozenMainFileIdentity::from_metadata(
                &self.file.metadata().map_err(|error| {
                    miette!("cannot refstat {role} descriptor {:?}: {error}", path)
                })?,
                path,
                role,
            )?;
            let named_after = frozen_named_main_identity(path, role)?;
            if descriptor_before != self.identity
                || named_before != self.identity
                || descriptor_after != self.identity
                || named_after != self.identity
            {
                bail!(
                    "{role} durable identity changed or its path names a different file: {:?}",
                    path
                );
            }
            Ok(())
        }
    }
}

impl FrozenSourceMainFile {
    fn open(path: &Path) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = path;
            bail!(
                "existing-only SQLite snapshots require Unix lstat/fstat file identity; \
                 this target has no implemented equivalent and is refused"
            );
        }

        #[cfg(unix)]
        {
            let mut options = OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
            let file = options.open(path).map_err(|error| {
                miette!(
                    "cannot open snapshot source {:?} read-only and nofollow: {error}",
                    path
                )
            })?;

            let opened_before = FrozenMainFileIdentity::from_metadata(
                &file.metadata().map_err(|error| {
                    miette!("cannot fstat opened snapshot source {:?}: {error}", path)
                })?,
                path,
                "opened snapshot source",
            )?;
            clear_nonblocking_and_verify_cloexec(&file, path, "snapshot source")?;
            let opened_after = FrozenMainFileIdentity::from_metadata(
                &file.metadata().map_err(|error| {
                    miette!("cannot refstat opened snapshot source {:?}: {error}", path)
                })?,
                path,
                "opened snapshot source",
            )?;
            let named = frozen_named_main_identity(path, "named snapshot source")?;
            let opened_final = FrozenMainFileIdentity::from_metadata(
                &file.metadata().map_err(|error| {
                    miette!(
                        "cannot finally fstat opened snapshot source {:?}: {error}",
                        path
                    )
                })?,
                path,
                "opened snapshot source",
            )?;
            if opened_before != opened_after || opened_after != named || opened_final != named {
                bail!(
                    "snapshot source {:?} changed or resolved to a different file while binding its descriptor",
                    path
                );
            }

            let frozen = Self {
                file,
                identity: named,
            };
            frozen.validate_bounded_schema_root(path)?;
            frozen.verify(path, "snapshot source after bounded schema-root preflight")?;
            Ok(frozen)
        }
    }

    fn len(&self) -> Result<u64> {
        #[cfg(not(unix))]
        {
            bail!("managed SQLite physical census requires Unix descriptor identity");
        }

        #[cfg(unix)]
        {
            Ok(self.identity.len)
        }
    }

    /// Bound aggregate schema-cache work before any SQLite API can force a
    /// schema parse. Mneme's SQLite container has a tiny schema (the `cozo`
    /// table plus its primary-key autoindex), so a multi-page schema b-tree is
    /// outside this fixed snapshot boundary. SQLite performs the authoritative
    /// structural and semantic checks after this allocation-independent fuse.
    #[cfg(unix)]
    fn validate_bounded_schema_root(&self, path: &Path) -> Result<()> {
        const PREFIX_LEN: usize = 108;
        const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
        const LEAF_TABLE_PAGE: u8 = 0x0d;

        let mut prefix = [0_u8; PREFIX_LEN];
        self.file.read_exact_at(&mut prefix, 0).map_err(|error| {
            miette!(
                "cannot read bounded SQLite header from snapshot source {:?}: {error}",
                path
            )
        })?;
        if &prefix[..SQLITE_HEADER.len()] != SQLITE_HEADER {
            bail!("snapshot source did not contain the SQLite format-3 header");
        }

        let encoded_page_size = u16::from_be_bytes([prefix[16], prefix[17]]);
        let page_size = if encoded_page_size == 1 {
            65_536_u64
        } else {
            u64::from(encoded_page_size)
        };
        if !(512..=65_536).contains(&page_size) || !page_size.is_power_of_two() {
            bail!("snapshot source declared an invalid SQLite page size");
        }
        if self.identity.len < page_size || self.identity.len % page_size != 0 {
            bail!("snapshot source length did not match its SQLite page size");
        }
        if prefix[100] != LEAF_TABLE_PAGE {
            bail!("snapshot source schema b-tree exceeded the single-page snapshot boundary");
        }

        let cells = u16::from_be_bytes([prefix[103], prefix[104]]);
        if cells > MAX_SNAPSHOT_SCHEMA_CELLS {
            bail!(
                "snapshot source schema exceeded the {MAX_SNAPSHOT_SCHEMA_CELLS}-record snapshot boundary"
            );
        }
        let pointer_array_end = PREFIX_LEN
            .checked_add(usize::from(cells) * 2)
            .ok_or_else(|| miette!("snapshot source schema pointer array overflowed"))?;
        if pointer_array_end > page_size as usize {
            bail!("snapshot source schema pointer array exceeded its SQLite page");
        }
        Ok(())
    }

    fn verify(&self, path: &Path, role: &str) -> Result<()> {
        #[cfg(test)]
        MANAGED_SOURCE_MAIN_VERIFY_CALLS.with(|calls| calls.set(calls.get() + 1));
        #[cfg(not(unix))]
        {
            let _ = (path, role);
            bail!(
                "existing-only SQLite snapshots require Unix lstat/fstat file identity; \
                 this target has no implemented equivalent and is refused"
            );
        }

        #[cfg(unix)]
        {
            let descriptor_before = FrozenMainFileIdentity::from_metadata(
                &self.file.metadata().map_err(|error| {
                    miette!("cannot fstat {role} descriptor {:?}: {error}", path)
                })?,
                path,
                role,
            )?;
            let named_before = frozen_named_main_identity(path, role)?;
            let descriptor_after = FrozenMainFileIdentity::from_metadata(
                &self.file.metadata().map_err(|error| {
                    miette!("cannot refstat {role} descriptor {:?}: {error}", path)
                })?,
                path,
                role,
            )?;
            let named_after = frozen_named_main_identity(path, role)?;
            if descriptor_before != self.identity
                || named_before != self.identity
                || descriptor_after != self.identity
                || named_after != self.identity
            {
                bail!(
                    "{role} main-file durable identity changed or its path names a different file: {:?}",
                    path
                );
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenMainFileIdentity {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

#[cfg(unix)]
impl FrozenMainFileIdentity {
    fn from_metadata(metadata: &fs::Metadata, path: &Path, role: &str) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;

        if !metadata.file_type().is_file() {
            bail!("{role} {:?} is not a regular file", path);
        }
        if metadata.nlink() != 1 {
            bail!(
                "{role} {:?} must have exactly one link, found {}",
                path,
                metadata.nlink()
            );
        }
        Ok(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            nlink: metadata.nlink(),
            len: metadata.len(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        })
    }
}

#[cfg(unix)]
fn frozen_named_main_identity(path: &Path, role: &str) -> Result<FrozenMainFileIdentity> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| miette!("cannot lstat {role} {:?}: {error}", path))?;
    FrozenMainFileIdentity::from_metadata(&metadata, path, role)
}

#[cfg(unix)]
fn clear_nonblocking_and_verify_cloexec(file: &File, path: &Path, role: &str) -> Result<()> {
    let fd = file.as_raw_fd();
    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags == -1 {
        return Err(std::io::Error::last_os_error())
            .map_err(|error| miette!("cannot inspect {role} fd {:?}: {error}", path));
    }
    if descriptor_flags & libc::FD_CLOEXEC == 0 {
        bail!("{role} fd {:?} is missing FD_CLOEXEC", path);
    }

    let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status_flags == -1 {
        return Err(std::io::Error::last_os_error())
            .map_err(|error| miette!("cannot inspect {role} flags {:?}: {error}", path));
    }
    if status_flags & libc::O_NONBLOCK != 0 {
        let code = unsafe { libc::fcntl(fd, libc::F_SETFL, status_flags & !libc::O_NONBLOCK) };
        if code == -1 {
            return Err(std::io::Error::last_os_error()).map_err(|error| {
                miette!("cannot clear O_NONBLOCK on {role} fd {:?}: {error}", path)
            });
        }
    }
    let final_status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if final_status_flags == -1 {
        return Err(std::io::Error::last_os_error())
            .map_err(|error| miette!("cannot re-inspect {role} flags {:?}: {error}", path));
    }
    if final_status_flags & libc::O_NONBLOCK != 0 {
        bail!("{role} fd {:?} retained O_NONBLOCK", path);
    }
    let final_descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if final_descriptor_flags == -1 {
        return Err(std::io::Error::last_os_error())
            .map_err(|error| miette!("cannot re-inspect {role} fd {:?}: {error}", path));
    }
    if final_descriptor_flags & libc::FD_CLOEXEC == 0 {
        bail!(
            "{role} fd {:?} lost FD_CLOEXEC while clearing O_NONBLOCK",
            path
        );
    }
    Ok(())
}

fn verify_snapshot_source(
    main_file: &FrozenSourceMainFile,
    residuals: &FrozenSourceResiduals,
    path: &Path,
    role: &str,
) -> Result<()> {
    let main_before = main_file.verify(path, role);
    let residual = residuals.verify(path, role);
    // Residual hashing can take materially longer than an lstat/fstat pair;
    // bracket it so the main file cannot drift during that work unnoticed.
    // Evaluate all three checks before combining them: no failure may skip a
    // later cleanup/postcondition check.
    let main_after = main_file.verify(path, role);
    let bracket = combine_with_postcondition(
        main_before,
        residual,
        "snapshot residual verification also failed",
    );
    combine_with_postcondition(
        bracket,
        main_after,
        "final snapshot main-file verification also failed",
    )
}

fn verify_snapshot_source_with_retained_backup(
    main_file: &FrozenSourceMainFile,
    retained_backup_destination: Option<&RetainedBackupDestinationIdentity>,
    residuals: &FrozenSourceResiduals,
    path: &Path,
    role: &str,
) -> Result<()> {
    let source = verify_snapshot_source(main_file, residuals, path, role);
    let retained = match retained_backup_destination {
        Some(retained) => retained.verify(path, role),
        None => Ok(()),
    };
    combine_with_postcondition(
        source,
        retained,
        "retained backup-destination custody verification also failed",
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenSourceResiduals {
    wal: FrozenResidualFile,
    shm: FrozenResidualFile,
}

impl FrozenSourceResiduals {
    fn capture(source: &Path) -> Result<Self> {
        ensure_named_path_absent(
            &sidecar_path(source, "-journal"),
            "snapshot source rollback journal",
        )?;
        Ok(Self {
            wal: FrozenResidualFile::capture(
                &sidecar_path(source, "-wal"),
                0,
                "snapshot source WAL residual",
            )?,
            shm: FrozenResidualFile::capture(
                &sidecar_path(source, "-shm"),
                MAX_SNAPSHOT_SHM_BYTES,
                "snapshot source shared-memory residual",
            )?,
        })
    }

    fn verify(&self, source: &Path, role: &str) -> Result<()> {
        #[cfg(test)]
        MANAGED_SOURCE_RESIDUAL_VERIFY_CALLS.with(|calls| calls.set(calls.get() + 1));
        #[cfg(test)]
        if MANAGED_SOURCE_RESIDUAL_TEST_FAILURE.with(|failure| failure.replace(false)) {
            bail!("injected managed SQLite residual verification failure");
        }
        let observed = Self::capture(source)?;
        if &observed != self {
            bail!("{role} residual bytes or durable metadata changed");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FrozenResidualFile {
    Absent,
    Present {
        metadata: DurableFileMetadata,
        sha256: [u8; 32],
    },
}

impl FrozenResidualFile {
    fn capture(path: &Path, maximum_bytes: u64, role: &str) -> Result<Self> {
        let named_metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Absent),
            Err(error) => bail!("cannot inspect {role} {:?}: {error}", path),
        };
        validate_residual_metadata(&named_metadata, path, role, maximum_bytes)?;
        let named_fingerprint = DurableFileMetadata::from_metadata(&named_metadata);

        let mut file = File::open(path).map_err(|error| {
            miette!("cannot open {role} {:?} for fingerprinting: {error}", path)
        })?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| miette!("cannot inspect opened {role} {:?}: {error}", path))?;
        validate_residual_metadata(&opened_metadata, path, role, maximum_bytes)?;
        let opened_fingerprint = DurableFileMetadata::from_metadata(&opened_metadata);
        if opened_fingerprint != named_fingerprint {
            bail!("{role} {:?} changed while it was being opened", path);
        }

        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut total = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| miette!("cannot read {role} {:?}: {error}", path))?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .ok_or_else(|| miette!("{role} {:?} byte count overflowed", path))?;
            if total > maximum_bytes {
                bail!(
                    "{role} {:?} exceeds the {maximum_bytes}-byte snapshot residual limit",
                    path
                );
            }
            hasher.update(&buffer[..read]);
        }
        if total != opened_fingerprint.len {
            bail!(
                "{role} {:?} changed length while it was being fingerprinted",
                path
            );
        }

        let closed_fingerprint = DurableFileMetadata::from_metadata(
            &file
                .metadata()
                .map_err(|error| miette!("cannot reinspect opened {role} {:?}: {error}", path))?,
        );
        let final_named_metadata = fs::symlink_metadata(path)
            .map_err(|error| miette!("cannot reinspect {role} {:?}: {error}", path))?;
        validate_residual_metadata(&final_named_metadata, path, role, maximum_bytes)?;
        let final_named_fingerprint = DurableFileMetadata::from_metadata(&final_named_metadata);
        if closed_fingerprint != opened_fingerprint || final_named_fingerprint != opened_fingerprint
        {
            bail!("{role} {:?} changed while it was being fingerprinted", path);
        }

        Ok(Self::Present {
            metadata: final_named_fingerprint,
            sha256: hasher.finalize().into(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DurableFileMetadata {
    len: u64,
    readonly: bool,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mode: u32,
    #[cfg(unix)]
    nlink: u64,
    #[cfg(unix)]
    uid: u32,
    #[cfg(unix)]
    gid: u32,
    #[cfg(unix)]
    rdev: u64,
    #[cfg(unix)]
    blocks: u64,
    #[cfg(unix)]
    block_size: u64,
    #[cfg(unix)]
    mtime: i64,
    #[cfg(unix)]
    mtime_nsec: i64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

impl DurableFileMetadata {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            nlink: metadata.nlink(),
            #[cfg(unix)]
            uid: metadata.uid(),
            #[cfg(unix)]
            gid: metadata.gid(),
            #[cfg(unix)]
            rdev: metadata.rdev(),
            #[cfg(unix)]
            blocks: metadata.blocks(),
            #[cfg(unix)]
            block_size: metadata.blksize(),
            #[cfg(unix)]
            mtime: metadata.mtime(),
            #[cfg(unix)]
            mtime_nsec: metadata.mtime_nsec(),
            #[cfg(unix)]
            ctime: metadata.ctime(),
            #[cfg(unix)]
            ctime_nsec: metadata.ctime_nsec(),
        }
    }
}

fn build_closed_source_evidence_v1(
    supplied_path: PathBuf,
    managed_snapshot_policy: ManagedSnapshotPolicy,
    main_file: FrozenSourceMainFile,
    residuals: FrozenSourceResiduals,
) -> Result<ManagedSqliteClosedSourceEvidenceV1> {
    #[cfg(not(unix))]
    {
        let _ = (supplied_path, managed_snapshot_policy, main_file, residuals);
        bail!("managed SQLite closed source evidence requires Unix raw path and file identity");
    }

    #[cfg(unix)]
    {
        ensure_absolute_path(&supplied_path, "managed SQLite closed source evidence")?;
        let FrozenSourceMainFile { file, identity } = main_file;
        let FrozenSourceResiduals { wal, shm } = residuals;
        let main_identity = ManagedSqliteClosedMainFileIdentityV1 {
            device: identity.dev,
            inode: identity.ino,
            mode: identity.mode,
            uid: identity.uid,
            gid: identity.gid,
            link_count: identity.nlink,
            length: identity.len,
            mtime_seconds: identity.mtime,
            mtime_nanoseconds: identity.mtime_nsec,
            ctime_seconds: identity.ctime,
            ctime_nanoseconds: identity.ctime_nsec,
            _thread_bound: PhantomData,
        };
        let wal_residual = build_closed_residual_evidence_v1(wal);
        let shm_residual = build_closed_residual_evidence_v1(shm);
        let identity_fingerprint = managed_closed_source_identity_fingerprint_v1(
            &supplied_path,
            managed_snapshot_policy,
            &main_identity,
            &wal_residual,
            &shm_residual,
        )?;
        // The audit deliberately retains no descriptor. It is dropped only
        // after strict SQLite close and the complete source verification bracket.
        drop(file);
        Ok(ManagedSqliteClosedSourceEvidenceV1 {
            supplied_path,
            managed_snapshot_policy,
            main_identity,
            wal_residual,
            shm_residual,
            policy_fingerprint: *managed_snapshot_policy.source_fingerprint(),
            identity_fingerprint,
            _thread_bound: PhantomData,
        })
    }
}

#[cfg(unix)]
fn build_closed_residual_evidence_v1(
    residual: FrozenResidualFile,
) -> ManagedSqliteClosedResidualV1 {
    match residual {
        FrozenResidualFile::Absent => ManagedSqliteClosedResidualV1 {
            present: false,
            metadata: None,
            sha256: None,
            _thread_bound: PhantomData,
        },
        FrozenResidualFile::Present { metadata, sha256 } => {
            let metadata = ManagedSqliteClosedResidualMetadataV1 {
                length: metadata.len,
                device: metadata.dev,
                inode: metadata.ino,
                mode: metadata.mode,
                link_count: metadata.nlink,
                uid: metadata.uid,
                gid: metadata.gid,
                rdev: metadata.rdev,
                blocks: metadata.blocks,
                block_size: metadata.block_size,
                mtime_seconds: metadata.mtime,
                mtime_nanoseconds: metadata.mtime_nsec,
                ctime_seconds: metadata.ctime,
                ctime_nanoseconds: metadata.ctime_nsec,
                _thread_bound: PhantomData,
            };
            ManagedSqliteClosedResidualV1 {
                present: true,
                metadata: Some(metadata),
                sha256: Some(sha256),
                _thread_bound: PhantomData,
            }
        }
    }
}

#[cfg(unix)]
fn managed_closed_source_identity_fingerprint_v1(
    supplied_path: &Path,
    managed_snapshot_policy: ManagedSnapshotPolicy,
    main_identity: &ManagedSqliteClosedMainFileIdentityV1,
    wal_residual: &ManagedSqliteClosedResidualV1,
    shm_residual: &ManagedSqliteClosedResidualV1,
) -> Result<[u8; 32]> {
    let source_policy_fingerprint = managed_snapshot_policy.source_fingerprint();
    let managed_snapshot_policy = managed_snapshot_policy.identity_bytes();
    let main_bytes = managed_closed_main_identity_bytes_v1(main_identity);
    let wal_bytes = managed_closed_residual_bytes_v1(wal_residual)?;
    let shm_bytes = managed_closed_residual_bytes_v1(shm_residual)?;
    let mut hasher = Sha256::new();
    hasher.update(MANAGED_CLOSED_SOURCE_IDENTITY_DOMAIN_V1);
    hasher.update([0x01]);
    hasher.update(6_u64.to_be_bytes());
    for (name, value) in [
        (
            b"source-policy-fingerprint".as_slice(),
            source_policy_fingerprint.as_slice(),
        ),
        (
            b"managed-snapshot-policy".as_slice(),
            managed_snapshot_policy,
        ),
        (
            b"supplied-absolute-unix-path".as_slice(),
            supplied_path.as_os_str().as_bytes(),
        ),
        (b"main-identity".as_slice(), main_bytes.as_slice()),
        (b"wal-residual".as_slice(), wal_bytes.as_slice()),
        (b"shm-residual".as_slice(), shm_bytes.as_slice()),
    ] {
        managed_closed_source_named_frame_v1(&mut hasher, name, value)?;
    }
    hasher.update([0xff]);
    hasher.update(1_u64.to_be_bytes());
    Ok(hasher.finalize().into())
}

#[cfg(unix)]
fn managed_closed_source_named_frame_v1(
    hasher: &mut Sha256,
    name: &[u8],
    value: &[u8],
) -> Result<()> {
    let name_length = u64::try_from(name.len())
        .map_err(|_| miette!("managed SQLite closed source frame name overflowed"))?;
    let value_length = u64::try_from(value.len())
        .map_err(|_| miette!("managed SQLite closed source frame value overflowed"))?;
    hasher.update(name_length.to_be_bytes());
    hasher.update(name);
    hasher.update(value_length.to_be_bytes());
    hasher.update(value);
    Ok(())
}

#[cfg(unix)]
fn managed_closed_main_identity_bytes_v1(
    identity: &ManagedSqliteClosedMainFileIdentityV1,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(76);
    bytes.extend_from_slice(&identity.device.to_be_bytes());
    bytes.extend_from_slice(&identity.inode.to_be_bytes());
    bytes.extend_from_slice(&identity.mode.to_be_bytes());
    bytes.extend_from_slice(&identity.uid.to_be_bytes());
    bytes.extend_from_slice(&identity.gid.to_be_bytes());
    bytes.extend_from_slice(&identity.link_count.to_be_bytes());
    bytes.extend_from_slice(&identity.length.to_be_bytes());
    bytes.extend_from_slice(&identity.mtime_seconds.to_be_bytes());
    bytes.extend_from_slice(&identity.mtime_nanoseconds.to_be_bytes());
    bytes.extend_from_slice(&identity.ctime_seconds.to_be_bytes());
    bytes.extend_from_slice(&identity.ctime_nanoseconds.to_be_bytes());
    bytes
}

#[cfg(unix)]
fn managed_closed_residual_bytes_v1(residual: &ManagedSqliteClosedResidualV1) -> Result<Vec<u8>> {
    if !residual.present {
        if residual.metadata.is_some() || residual.sha256.is_some() {
            bail!("managed SQLite absent residual carried present-only evidence");
        }
        return Ok(vec![0x00]);
    }
    let metadata = residual
        .metadata
        .as_ref()
        .ok_or_else(|| miette!("managed SQLite present residual lost its metadata"))?;
    let sha256 = residual
        .sha256
        .as_ref()
        .ok_or_else(|| miette!("managed SQLite present residual lost its content fingerprint"))?;
    let mut bytes = Vec::with_capacity(133);
    bytes.push(0x01);
    bytes.extend_from_slice(&metadata.length.to_be_bytes());
    bytes.extend_from_slice(&metadata.device.to_be_bytes());
    bytes.extend_from_slice(&metadata.inode.to_be_bytes());
    bytes.extend_from_slice(&metadata.mode.to_be_bytes());
    bytes.extend_from_slice(&metadata.link_count.to_be_bytes());
    bytes.extend_from_slice(&metadata.uid.to_be_bytes());
    bytes.extend_from_slice(&metadata.gid.to_be_bytes());
    bytes.extend_from_slice(&metadata.rdev.to_be_bytes());
    bytes.extend_from_slice(&metadata.blocks.to_be_bytes());
    bytes.extend_from_slice(&metadata.block_size.to_be_bytes());
    bytes.extend_from_slice(&metadata.mtime_seconds.to_be_bytes());
    bytes.extend_from_slice(&metadata.mtime_nanoseconds.to_be_bytes());
    bytes.extend_from_slice(&metadata.ctime_seconds.to_be_bytes());
    bytes.extend_from_slice(&metadata.ctime_nanoseconds.to_be_bytes());
    bytes.extend_from_slice(sha256);
    Ok(bytes)
}

fn validate_residual_metadata(
    metadata: &fs::Metadata,
    path: &Path,
    role: &str,
    maximum_bytes: u64,
) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("{role} {:?} is not a regular non-symlink file", path);
    }
    if metadata.len() > maximum_bytes {
        bail!(
            "{role} {:?} is {} bytes, above the {maximum_bytes}-byte snapshot residual limit",
            path,
            metadata.len()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!(
                "{role} {:?} must have exactly one link, found {}",
                path,
                metadata.nlink()
            );
        }
    }
    Ok(())
}

fn ensure_named_path_absent(path: &Path, role: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("{role} must be absent: {:?}", path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => bail!("cannot inspect {role} {:?}: {error}", path),
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn ensure_sidecars_absent(path: &Path, role: &str) -> Result<()> {
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        let sidecar = sidecar_path(path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => bail!("{role} sidecar must be absent: {:?}", sidecar),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => bail!("cannot inspect {role} sidecar {:?}: {error}", sidecar),
        }
    }
    Ok(())
}

fn combine_with_postcondition<T>(
    operation: Result<T>,
    postcondition: Result<()>,
    context: &str,
) -> Result<T> {
    match (operation, postcondition) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(operation_error), Err(postcondition_error)) => Err(miette!(
            "{operation_error}; {context}: {postcondition_error}"
        )),
    }
}

fn immutable_source_uri(path: &Path) -> Result<CString> {
    ensure_absolute_path(path, "snapshot source")?;

    let mut uri = Vec::new();
    #[cfg(unix)]
    {
        let bytes = path.as_os_str().as_bytes();
        if bytes.contains(&0) {
            bail!("snapshot source path contains a NUL byte");
        }
        // Two slashes plus the absolute path's leading slash form an empty URI
        // authority (`file:///...`) without losing arbitrary Unix path bytes.
        uri.extend_from_slice(b"file://");
        append_uri_path(&mut uri, bytes);
    }

    #[cfg(windows)]
    {
        let text = path
            .to_str()
            .ok_or_else(|| miette!("snapshot source path is not representable as SQLite UTF-8"))?;
        let normalized = text.replace('\\', "/");
        let bytes = normalized.as_bytes();
        if normalized.starts_with("//") {
            bail!("UNC snapshot source paths are not supported by this SQLite URI boundary");
        }
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || bytes[2] != b'/'
        {
            bail!("snapshot source path is not a fully qualified Windows drive path");
        }
        uri.extend_from_slice(b"file:///");
        append_uri_path(&mut uri, bytes);
    }

    #[cfg(not(any(unix, windows)))]
    {
        let text = path
            .to_str()
            .ok_or_else(|| miette!("snapshot source path is not representable as SQLite UTF-8"))?;
        uri.extend_from_slice(b"file://");
        append_uri_path(&mut uri, text.as_bytes());
    }

    uri.extend_from_slice(b"?mode=ro&immutable=1&cache=private");
    CString::new(uri).map_err(|_| miette!("snapshot source URI contains a NUL byte"))
}

fn append_uri_path(output: &mut Vec<u8>, path: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in path {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            output.push(byte);
        } else {
            output.push(b'%');
            output.push(HEX[(byte >> 4) as usize]);
            output.push(HEX[(byte & 0x0f) as usize]);
        }
    }
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    #[cfg(unix)]
    {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| miette!("snapshot destination path contains a NUL byte"))
    }
    #[cfg(not(unix))]
    {
        let text = path.to_str().ok_or_else(|| {
            miette!("snapshot destination path is not representable as SQLite UTF-8")
        })?;
        CString::new(text).map_err(|_| miette!("snapshot destination path contains a NUL byte"))
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct OwnedDestinationIdentity {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum DestinationIdentityPolicy {
    /// The descriptor has just been created with mode 0600 subject to umask.
    Creation,
    /// The descriptor has been explicitly set to the publication policy.
    Publication,
}

#[cfg(unix)]
impl OwnedDestinationIdentity {
    fn capture(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            nlink: metadata.nlink(),
        }
    }

    fn from_metadata(
        metadata: &fs::Metadata,
        path: &Path,
        role: &str,
        policy: DestinationIdentityPolicy,
    ) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;

        if !metadata.file_type().is_file() {
            bail!("{role} {:?} is not a regular file", path);
        }
        if metadata.nlink() != 1 {
            bail!(
                "{role} {:?} must have exactly one link, found {}",
                path,
                metadata.nlink()
            );
        }
        let permissions = metadata.mode() & 0o7777;
        match policy {
            DestinationIdentityPolicy::Creation if permissions & !0o600 != 0 => {
                bail!(
                    "{role} {:?} has permissions outside the create-new 0600 policy: {permissions:04o}",
                    path
                );
            }
            DestinationIdentityPolicy::Publication if permissions != 0o600 => {
                bail!(
                    "{role} {:?} must have mode 0600, found {permissions:04o}",
                    path
                );
            }
            _ => {}
        }
        Ok(Self::capture(metadata))
    }
}

#[cfg(unix)]
fn named_destination_identity(
    path: &Path,
    role: &str,
    policy: DestinationIdentityPolicy,
) -> Result<OwnedDestinationIdentity> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| miette!("cannot lstat {role} {:?}: {error}", path))?;
    OwnedDestinationIdentity::from_metadata(&metadata, path, role, policy)
}

#[derive(Debug)]
struct IncompleteDestination {
    path: PathBuf,
    file: Option<File>,
    #[cfg(unix)]
    identity: Option<OwnedDestinationIdentity>,
    /// Armed means Drop may make exactly one identity-checked cleanup attempt.
    /// Every attempt disarms first so an error cannot retry against a changed path.
    cleanup_armed: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DestinationSetupPhase {
    BeforeIdentity,
    AfterIdentity,
}

impl IncompleteDestination {
    fn create(path: &Path) -> Result<Self> {
        Self::create_impl(path, |_| Ok(()))
    }

    #[cfg(test)]
    fn create_failing_before_identity(path: &Path) -> Result<Self> {
        Self::create_impl(path, |phase| {
            if phase == DestinationSetupPhase::BeforeIdentity {
                bail!("injected destination setup failure before identity binding");
            }
            Ok(())
        })
    }

    #[cfg(test)]
    fn create_failing_after_identity(path: &Path) -> Result<Self> {
        Self::create_impl(path, |phase| {
            if phase == DestinationSetupPhase::AfterIdentity {
                bail!("injected destination setup failure after identity binding");
            }
            Ok(())
        })
    }

    fn create_impl<F>(path: &Path, mut setup_hook: F) -> Result<Self>
    where
        F: FnMut(DestinationSetupPhase) -> Result<()>,
    {
        #[cfg(not(unix))]
        {
            let _ = (path, setup_hook);
            bail!(
                "existing-only SQLite snapshot destinations require Unix lstat/fstat file identity; \
                 this target has no implemented equivalent and is refused"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
            let file = options.open(path).map_err(|error| {
                miette!("cannot create new snapshot destination {:?}: {error}", path)
            })?;

            // Arm cleanup immediately after create_new. Until the first fstat succeeds there is
            // deliberately no identity with which cleanup may justify unlinking the pathname.
            let mut destination = Self {
                path: path.to_path_buf(),
                file: Some(file),
                identity: None,
                cleanup_armed: true,
            };
            let setup = (|| {
                setup_hook(DestinationSetupPhase::BeforeIdentity)?;
                let initial_metadata = destination
                    .file
                    .as_ref()
                    .expect("armed destination guard must retain its descriptor")
                    .metadata()
                    .map_err(|error| {
                        miette!(
                            "cannot initially fstat new snapshot destination {:?}: {error}",
                            path
                        )
                    })?;
                destination.identity = Some(OwnedDestinationIdentity::capture(&initial_metadata));
                setup_hook(DestinationSetupPhase::AfterIdentity)?;
                destination.verify_bound_path(
                    "new snapshot destination immediately after create_new",
                    DestinationIdentityPolicy::Creation,
                )?;

                destination
                    .file
                    .as_ref()
                    .expect("armed destination guard must retain its descriptor")
                    .set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(|error| {
                        miette!(
                            "cannot set snapshot destination {:?} to mode 0600: {error}",
                            path
                        )
                    })?;
                // fchmod succeeded, so update the guard before the next fallible operation.
                let identity = destination
                    .identity
                    .as_mut()
                    .expect("destination identity was bound before fchmod");
                identity.mode = (identity.mode & !0o7777) | 0o600;
                clear_nonblocking_and_verify_cloexec(
                    destination
                        .file
                        .as_ref()
                        .expect("armed destination guard must retain its descriptor"),
                    path,
                    "snapshot destination",
                )?;
                destination.verify_owned_path("snapshot destination after create")?;
                Ok(())
            })();

            match setup {
                Ok(()) => Ok(destination),
                Err(setup_error) => match destination.cleanup() {
                    Ok(()) => Err(setup_error),
                    Err(cleanup_error) => Err(miette!(
                        "{setup_error}; cleanup after snapshot destination setup failure also failed: {cleanup_error}"
                    )),
                },
            }
        }
    }

    fn verify_owned_path(&self, role: &str) -> Result<()> {
        #[cfg(unix)]
        {
            self.verify_bound_path(role, DestinationIdentityPolicy::Publication)
        }

        #[cfg(not(unix))]
        {
            let _ = role;
            bail!(
                "existing-only SQLite snapshot destinations require Unix lstat/fstat file identity; \
                 this target has no implemented equivalent and is refused"
            );
        }
    }

    #[cfg(unix)]
    fn verify_bound_path(&self, role: &str, policy: DestinationIdentityPolicy) -> Result<()> {
        let identity = self.identity.as_ref().ok_or_else(|| {
            miette!(
                "{role} identity was never bound; preserving {:?}",
                self.path
            )
        })?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| miette!("{role} descriptor is already closed"))?;
        let descriptor_before = OwnedDestinationIdentity::from_metadata(
            &file
                .metadata()
                .map_err(|error| miette!("cannot fstat {role} {:?}: {error}", self.path))?,
            &self.path,
            role,
            policy,
        )?;
        let named_before = named_destination_identity(&self.path, role, policy)?;
        let descriptor_after = OwnedDestinationIdentity::from_metadata(
            &file
                .metadata()
                .map_err(|error| miette!("cannot refstat {role} {:?}: {error}", self.path))?,
            &self.path,
            role,
            policy,
        )?;
        let named_after = named_destination_identity(&self.path, role, policy)?;
        if descriptor_before != *identity
            || named_before != *identity
            || descriptor_after != *identity
            || named_after != *identity
        {
            bail!(
                "{role} no longer names the owned snapshot destination: {:?}",
                self.path
            );
        }
        Ok(())
    }

    fn sync_all(&self) -> Result<()> {
        self.file
            .as_ref()
            .ok_or_else(|| miette!("snapshot destination file is already closed"))?
            .sync_all()
            .map_err(|error| miette!("cannot sync snapshot destination {:?}: {error}", self.path))
    }

    fn file_len(&self) -> Result<u64> {
        self.file
            .as_ref()
            .ok_or_else(|| miette!("snapshot destination file is already closed"))?
            .metadata()
            .map(|metadata| metadata.len())
            .map_err(|error| {
                miette!(
                    "cannot inspect snapshot destination {:?} length: {error}",
                    self.path
                )
            })
    }

    fn duplicate_retained_backup_identity(&self) -> Result<RetainedBackupDestinationIdentity> {
        #[cfg(not(unix))]
        {
            bail!(
                "existing-only SQLite backup custody requires Unix lstat/fstat file identity; \
                 this target has no implemented equivalent and is refused"
            );
        }

        #[cfg(unix)]
        {
            self.verify_owned_path(
                "snapshot destination before duplicating its retained custody descriptor",
            )?;
            let original = self
                .file
                .as_ref()
                .ok_or_else(|| miette!("snapshot destination descriptor is already closed"))?;
            let file = original.try_clone().map_err(|error| {
                miette!(
                    "cannot duplicate snapshot destination custody descriptor {:?}: {error}",
                    self.path
                )
            })?;
            clear_nonblocking_and_verify_cloexec(
                &file,
                &self.path,
                "retained snapshot destination custody",
            )?;

            let original_before = FrozenMainFileIdentity::from_metadata(
                &original.metadata().map_err(|error| {
                    miette!(
                        "cannot fstat original snapshot destination {:?}: {error}",
                        self.path
                    )
                })?,
                &self.path,
                "original snapshot destination",
            )?;
            let duplicate_before = FrozenMainFileIdentity::from_metadata(
                &file.metadata().map_err(|error| {
                    miette!(
                        "cannot fstat duplicated snapshot destination {:?}: {error}",
                        self.path
                    )
                })?,
                &self.path,
                "duplicated snapshot destination",
            )?;
            let named = frozen_named_main_identity(
                &self.path,
                "named snapshot destination during custody transfer",
            )?;
            let duplicate_after = FrozenMainFileIdentity::from_metadata(
                &file.metadata().map_err(|error| {
                    miette!(
                        "cannot refstat duplicated snapshot destination {:?}: {error}",
                        self.path
                    )
                })?,
                &self.path,
                "duplicated snapshot destination",
            )?;
            let original_after = FrozenMainFileIdentity::from_metadata(
                &original.metadata().map_err(|error| {
                    miette!(
                        "cannot refstat original snapshot destination {:?}: {error}",
                        self.path
                    )
                })?,
                &self.path,
                "original snapshot destination",
            )?;
            if original_before != named
                || duplicate_before != named
                || duplicate_after != named
                || original_after != named
            {
                bail!(
                    "snapshot destination changed while duplicating its custody descriptor: {:?}",
                    self.path
                );
            }

            let retained = RetainedBackupDestinationIdentity {
                file,
                identity: named,
            };
            retained.verify(
                &self.path,
                "retained snapshot destination after custody transfer",
            )?;
            self.verify_owned_path(
                "snapshot destination after duplicating its retained custody descriptor",
            )?;
            Ok(retained)
        }
    }

    fn publish(&mut self) -> Result<()> {
        self.verify_owned_path("snapshot destination before final sidecar check")?;
        ensure_sidecars_absent(&self.path, "snapshot destination before publication")?;
        self.verify_owned_path("snapshot destination immediately before publication")?;
        self.cleanup_armed = false;
        drop(self.file.take());
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        if !self.cleanup_armed {
            return Ok(());
        }
        // One shot only. In particular, Drop must not retry after an error
        // against a pathname that an adversary can change between attempts.
        self.cleanup_armed = false;
        let mut failures = Vec::with_capacity(SQLITE_SIDECAR_SUFFIXES.len() + 1);

        #[cfg(unix)]
        let ownership = self.verify_bound_path(
            "incomplete snapshot destination cleanup",
            DestinationIdentityPolicy::Creation,
        );
        #[cfg(not(unix))]
        let ownership = self.verify_owned_path("incomplete snapshot destination cleanup");
        match ownership {
            Ok(()) => match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(error) => failures.push(format!(
                    "owned destination {:?} could not be removed: {error}",
                    self.path
                )),
            },
            Err(error) => failures.push(format!(
                "destination main path was preserved because ownership could not be proven: {error}"
            )),
        }

        // Absence before create does not prove ownership of a later SQLite
        // sidecar. Preserve every observed sidecar rather than deleting a
        // possibly foreign file.
        for suffix in SQLITE_SIDECAR_SUFFIXES {
            let sidecar = sidecar_path(&self.path, suffix);
            match fs::symlink_metadata(&sidecar) {
                Ok(_) => failures.push(format!(
                    "unowned destination sidecar was preserved: {:?}",
                    sidecar
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => failures.push(format!(
                    "destination sidecar ownership is unknown and it was preserved: {:?}: {error}",
                    sidecar
                )),
            }
        }

        drop(self.file.take());
        if failures.is_empty() {
            Ok(())
        } else {
            bail!("{}", failures.join("; "))
        }
    }
}

impl Drop for IncompleteDestination {
    fn drop(&mut self) {
        if self.cleanup_armed {
            let _ = self.cleanup();
        }
    }
}

fn set_journal_mode(conn: &ConnectionThreadSafe, requested: &'static str) -> Result<()> {
    let mut statement = conn
        .prepare(format!("pragma journal_mode = {requested};"))
        .into_diagnostic()?;
    if statement.next().into_diagnostic()? != State::Row {
        bail!("sqlite did not report its journal mode after requesting {requested}");
    }
    let actual = statement.read::<String, _>(0).into_diagnostic()?;
    if !actual.eq_ignore_ascii_case(requested) {
        bail!("sqlite declined journal mode {requested}; database remains in {actual} mode");
    }
    if statement.next().into_diagnostic()? != State::Done {
        bail!("sqlite returned multiple journal-mode result rows");
    }
    Ok(())
}

fn configure_connection(conn: &ConnectionThreadSafe, enable_wal: bool) -> Result<()> {
    if enable_wal {
        set_journal_mode(conn, "WAL")?;
    }
    conn.execute(format!("pragma busy_timeout = {SQLITE_BUSY_TIMEOUT_MS};"))
        .into_diagnostic()?;
    Ok(())
}

fn require_sqlite_autocommit(
    conn: &ConnectionThreadSafe,
    expected: bool,
    operation: &str,
) -> Result<()> {
    // SAFETY: `conn` owns a live SQLite handle. `sqlite3_get_autocommit` has
    // existed since SQLite 3.2.0, so this check does not raise the system
    // SQLite link floor for builds without the bundled amalgamation.
    let actual = unsafe { ffi::sqlite3_get_autocommit(conn.as_raw()) != 0 };
    if actual != expected {
        bail!("sqlite autocommit after {operation} is {actual}, expected {expected}");
    }
    Ok(())
}

fn rollback_transaction(conn: &ConnectionThreadSafe) -> Result<()> {
    conn.execute("rollback;").into_diagnostic()?;
    require_sqlite_autocommit(conn, true, "rollback")
}

fn fail_started_transaction(
    conn: &ConnectionThreadSafe,
    setup_error: miette::Report,
) -> Result<()> {
    match rollback_transaction(conn) {
        Ok(()) => Err(setup_error),
        Err(rollback_error) => bail!(
            "sqlite transaction setup failed ({setup_error}); rollback also failed ({rollback_error})"
        ),
    }
}

fn begin_write_transaction(conn: &ConnectionThreadSafe) -> Result<()> {
    require_sqlite_autocommit(conn, true, "connection checkout")?;
    conn.execute("begin immediate;").into_diagnostic()?;
    if let Err(error) = require_sqlite_autocommit(conn, false, "begin immediate") {
        return fail_started_transaction(conn, error);
    }
    Ok(())
}

fn begin_pinned_read_transaction(conn: &ConnectionThreadSafe) -> Result<()> {
    require_sqlite_autocommit(conn, true, "connection checkout")?;
    conn.execute("begin deferred;").into_diagnostic()?;

    let pin_result = (|| {
        // BEGIN DEFERRED alone does not acquire a pager snapshot. Stepping a
        // real main-schema read does; validating the cozo table's root page
        // also rejects a connection aimed at the wrong or half-created file.
        let mut statement = conn
            .prepare(
                // `sqlite_master` is the portable legacy alias for the same
                // schema btree; `sqlite_schema` requires SQLite 3.33+.
                "select rootpage from main.sqlite_master \
                 where type = 'table' and name = 'cozo';",
            )
            .into_diagnostic()?;
        if statement.next().into_diagnostic()? != State::Row {
            bail!("sqlite snapshot pin could not find the cozo table root page");
        }
        let rootpage = statement.read::<i64, _>(0).into_diagnostic()?;
        if rootpage <= 0 {
            bail!("sqlite snapshot pin found invalid cozo root page {rootpage}");
        }
        if statement.next().into_diagnostic()? != State::Done {
            bail!("sqlite snapshot pin found multiple cozo table rows");
        }
        require_sqlite_autocommit(conn, false, "snapshot pin")
    })();

    match pin_result {
        Ok(()) => Ok(()),
        Err(error) => fail_started_transaction(conn, error),
    }
}

/// The Sqlite storage engine
#[derive(Clone)]
pub struct SqliteStorage {
    lock: Arc<ShardedLock<()>>,
    name: PathBuf,
    pool: Arc<Mutex<Vec<ConnectionThreadSafe>>>,
    open_mode: SqliteOpenMode,
}

#[derive(Clone, Copy)]
enum SqliteOpenMode {
    CreateIfMissing,
    ExistingOnly,
}

impl SqliteStorage {
    fn open_connection(&self) -> Result<ConnectionThreadSafe> {
        let conn = match self.open_mode {
            SqliteOpenMode::CreateIfMissing => {
                Connection::open_thread_safe(&self.name).into_diagnostic()?
            }
            SqliteOpenMode::ExistingOnly => Connection::open_thread_safe_with_flags(
                &self.name,
                OpenFlags::new().with_read_write(),
            )
            .into_diagnostic()?,
        };
        configure_connection(&conn, false)?;
        Ok(conn)
    }

    /// Return an otherwise usable read connection without waiting on the pool.
    /// Used only on bounded-startup failure, when the deadline has generally
    /// already expired and blocking cleanup would violate the API contract.
    fn return_read_connection_if_pool_available(&self, conn: ConnectionThreadSafe) {
        match self.pool.try_lock() {
            Ok(mut pool) => pool.push(conn),
            Err(TryLockError::Poisoned(error)) => error.into_inner().push(conn),
            Err(TryLockError::WouldBlock) => drop(conn),
        }
    }

    #[cfg(test)]
    pub(crate) fn pool_lock_for_tests(&self) -> MutexGuard<'_, Vec<ConnectionThreadSafe>> {
        self.pool.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn snapshot_write_lock_for_tests(&self) -> ShardedLockWriteGuard<'_, ()> {
        self.lock.write().unwrap_or_else(|error| error.into_inner())
    }

    /// Checkpoint WAL contents into the main database and return the file to a
    /// self-contained rollback-journal mode before an offline caller moves it.
    ///
    /// This is deliberately explicit rather than a `Drop` side effect: an
    /// ordinary process may have another independently opened handle to the
    /// same database, and closing one handle must not try to change the live
    /// database's journal mode underneath the others.
    pub fn prepare_for_file_move(&self) -> Result<()> {
        let _lock = self.lock.write().unwrap_or_else(|e| e.into_inner());
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let conn = match pool.pop() {
            Some(conn) => conn,
            None => self.open_connection()?,
        };

        // `journal_mode` needs to be the only live connection owned by this
        // storage instance. Dropping the idle pool first also closes any stale
        // read transactions before the checkpoint.
        pool.clear();
        drop(pool);

        let mut checkpoint = conn
            .prepare("pragma wal_checkpoint(TRUNCATE);")
            .into_diagnostic()?;
        if checkpoint.next().into_diagnostic()? != State::Row {
            bail!("sqlite WAL checkpoint returned no status row");
        }
        let busy = checkpoint.read::<i64, _>(0).into_diagnostic()?;
        let log_frames = checkpoint.read::<i64, _>(1).into_diagnostic()?;
        let checkpointed_frames = checkpoint.read::<i64, _>(2).into_diagnostic()?;
        if busy != 0 || log_frames != checkpointed_frames {
            bail!(
                "sqlite WAL checkpoint incomplete: busy={busy}, log_frames={log_frames}, checkpointed_frames={checkpointed_frames}"
            );
        }
        if checkpoint.next().into_diagnostic()? != State::Done {
            bail!("sqlite WAL checkpoint returned multiple status rows");
        }
        drop(checkpoint);

        set_journal_mode(&conn, "DELETE")?;
        drop(conn);

        // SQLite can leave an empty shared-memory file behind even after the
        // WAL has been checkpointed and journal mode changed. With exclusive
        // ownership and every connection above closed, none of these files can
        // contain live state; remove them so renaming only the main file is
        // unambiguously lossless.
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut sidecar = self.name.as_os_str().to_os_string();
            sidecar.push(suffix);
            match std::fs::remove_file(PathBuf::from(sidecar)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).into_diagnostic(),
            }
        }
        Ok(())
    }
}

/// Create a sqlite backed database.
/// Supports concurrent readers but only a single writer.
///
/// You must provide a disk-based path: `:memory:` is not OK.
/// If you want a pure memory storage, use [`new_cozo_mem`](crate::new_cozo_mem).
pub fn new_cozo_sqlite(path: impl AsRef<Path>) -> Result<crate::Db<SqliteStorage>> {
    if path.as_ref().to_str() == Some("") {
        bail!("empty path for sqlite storage")
    }
    let conn = Connection::open_thread_safe(&path).into_diagnostic()?;
    // WAL lets readers continue while a writer publishes a bounded batch;
    // busy_timeout keeps transient cross-instance lock races inside SQLite
    // instead of surfacing as panics from prepared-statement unwraps.
    configure_connection(&conn, true)?;
    let query = r#"
        create table if not exists cozo
        (
            k BLOB primary key,
            v BLOB
        );
    "#;
    conn.execute(query).into_diagnostic()?;

    let ret = crate::Db::new(SqliteStorage {
        lock: Default::default(),
        name: PathBuf::from(path.as_ref()),
        pool: Default::default(),
        open_mode: SqliteOpenMode::CreateIfMissing,
    })?;

    ret.initialize()?;
    Ok(ret)
}

/// Open the initialized SQLite-backed database authorized by `permit` without
/// creating or repairing storage.
///
/// Unlike [`new_cozo_sqlite`], this constructor never supplies
/// `SQLITE_OPEN_CREATE`, changes journal mode, or executes schema DDL. Every
/// later connection opened after a pool miss retains that existing-only
/// policy. The database remains writable after admission; only constructor
/// initialization is read-only. See
/// [`ManagedExistingSqliteOpenPermitV1`] for the remaining pathname and
/// reconnect caveats.
pub fn new_cozo_sqlite_existing(
    permit: ManagedExistingSqliteOpenPermitV1,
) -> Result<crate::Db<SqliteStorage>> {
    let guarded_source = permit.into_guarded_source()?;
    let name = guarded_source.path.clone();
    let operation = (|| {
        if name.to_str().is_none() {
            bail!(
                "existing-only SQLite runtime path is not valid UTF-8 and sqlite 0.36 cannot open it"
            );
        }
        let conn =
            Connection::open_thread_safe_with_flags(&name, OpenFlags::new().with_read_write())
                .into_diagnostic()?;
        configure_connection(&conn, false)?;
        verify_existing_runtime_preinit_main(&guarded_source, &name)?;

        let storage = SqliteStorage {
            lock: Default::default(),
            name: name.clone(),
            pool: Default::default(),
            open_mode: SqliteOpenMode::ExistingOnly,
        };
        storage
            .pool
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(conn);

        let ret = crate::Db::new(storage)?;
        #[cfg(test)]
        if MANAGED_EXISTING_RUNTIME_INIT_FAILURE.with(|failure| failure.replace(false)) {
            bail!("injected existing-only SQLite runtime initialization failure");
        }
        ret.initialize_existing()?;
        Ok(ret)
    })();
    combine_with_postcondition(
        operation,
        verify_existing_runtime_final_main(&guarded_source, &name),
        "existing-only SQLite final main-file verification also failed",
    )
}

fn verify_existing_runtime_preinit_main(
    guarded_source: &ExistingSqliteSnapshotSource,
    path: &Path,
) -> Result<()> {
    #[cfg(test)]
    MANAGED_EXISTING_RUNTIME_PREINIT_VERIFY_CALLS.with(|calls| calls.set(calls.get() + 1));
    guarded_source
        .main_file
        .verify(path, "existing-only SQLite pre-initialization")
}

fn verify_existing_runtime_final_main(
    guarded_source: &ExistingSqliteSnapshotSource,
    path: &Path,
) -> Result<()> {
    #[cfg(test)]
    MANAGED_EXISTING_RUNTIME_FINAL_VERIFY_CALLS.with(|calls| calls.set(calls.get() + 1));
    guarded_source
        .main_file
        .verify(path, "existing-only SQLite runtime completion")
}

impl<'s> Storage<'s> for SqliteStorage {
    type Tx = SqliteTx<'s>;

    fn transact(&'s self, write: bool) -> Result<Self::Tx> {
        let conn = {
            match self.pool.lock().unwrap_or_else(|e| e.into_inner()).pop() {
                None => self.open_connection()?,
                Some(conn) => conn,
            }
        };
        let lock = if write {
            Right(self.lock.write().unwrap_or_else(|e| e.into_inner()))
        } else {
            Left(self.lock.read().unwrap_or_else(|e| e.into_inner()))
        };
        let state = if write {
            // Reserve the single SQLite writer before doing expensive CAS and
            // projection work, rather than discovering contention at commit.
            begin_write_transaction(&conn)?;
            SqliteTransactionState::ActiveWrite
        } else {
            begin_pinned_read_transaction(&conn)?;
            SqliteTransactionState::ActiveRead
        };
        Ok(SqliteTx {
            lock,
            storage: self,
            conn: Some(conn),
            stmts: [
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
            ],
            state,
            pool_return_deadline: None,
        })
    }

    fn transact_read_with_deadline(&'s self, deadline: Instant) -> Result<Self::Tx> {
        let pooled = loop {
            crate::runtime::db::check_deadline(deadline)?;
            match self.pool.try_lock() {
                Ok(mut pool) => break pool.pop(),
                Err(TryLockError::Poisoned(error)) => break error.into_inner().pop(),
                Err(TryLockError::WouldBlock) => park_before_deadline(deadline)?,
            }
        };
        let conn = match pooled {
            Some(conn) => conn,
            None => {
                // SQLite open/configuration and filesystem calls are not
                // preemptible. The surrounding checks make any overrun visible
                // immediately afterwards; the vendoring contract calls this
                // residual out explicitly instead of promising hard cancel.
                crate::runtime::db::check_deadline(deadline)?;
                let conn = self.open_connection()?;
                if let Err(error) = crate::runtime::db::check_deadline(deadline) {
                    self.return_read_connection_if_pool_available(conn);
                    return Err(error);
                }
                conn
            }
        };
        let lock = match read_sharded_before_deadline(&self.lock, deadline) {
            Ok(lock) => lock,
            Err(error) => {
                self.return_read_connection_if_pool_available(conn);
                return Err(error);
            }
        };
        if let Err(error) = crate::runtime::db::check_deadline(deadline) {
            drop(lock);
            self.return_read_connection_if_pool_available(conn);
            return Err(error);
        }
        begin_pinned_read_transaction(&conn)?;
        let tx = SqliteTx {
            lock: Left(lock),
            storage: self,
            conn: Some(conn),
            stmts: [
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
            ],
            state: SqliteTransactionState::ActiveRead,
            pool_return_deadline: Some(deadline),
        };
        if let Err(error) = crate::runtime::db::check_deadline(deadline) {
            drop(tx);
            return Err(error);
        }
        Ok(tx)
    }

    fn batch_put<'a>(
        &'a self,
        data: Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>,
    ) -> Result<()> {
        let mut tx = self.transact(true)?;
        for result in data {
            let (key, val) = result?;
            tx.put(&key, &val)?;
        }
        tx.commit()?;
        Ok(())
    }

    fn range_compact(&'_ self, _lower: &[u8], _upper: &[u8]) -> Result<()> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        while pool.pop().is_some() {}
        Ok(())
    }

    fn storage_kind(&self) -> &'static str {
        "sqlite"
    }
}

pub struct SqliteTx<'a> {
    lock: Either<ShardedLockReadGuard<'a, ()>, ShardedLockWriteGuard<'a, ()>>,
    storage: &'a SqliteStorage,
    conn: Option<ConnectionThreadSafe>,
    stmts: [Mutex<Option<Statement<'a>>>; N_CACHED_QUERIES],
    state: SqliteTransactionState,
    /// Bounded reads must not wait indefinitely merely to return their
    /// connection to this process-local pool during teardown.
    pool_return_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SqliteTransactionState {
    ActiveRead,
    ActiveWrite,
    Finished,
}

unsafe impl Sync for SqliteTx<'_> {}

const N_QUERIES: usize = 9;
const N_CACHED_QUERIES: usize = 4;
const QUERIES: [&str; N_QUERIES] = [
    "select v from cozo where k = ?;",
    "insert into cozo(k, v) values (?, ?) on conflict(k) do update set v=excluded.v;",
    "delete from cozo where k = ?;",
    "select 1 from cozo where k = ?;",
    "select k, v from cozo where k >= ? and k < ? order by k;",
    "select k, v from cozo where k >= ? and k < ? order by k limit 1;",
    "select count(*) from cozo where k >= ? and k < ?;",
    "select k, v from cozo where k >= ? and k < ? order by k limit ?;",
    "select k, v from cozo where k >= ? and k < ? order by k desc limit ?;",
];

const GET_QUERY: usize = 0;
const PUT_QUERY: usize = 1;
const DEL_QUERY: usize = 2;
const EXISTS_QUERY: usize = 3;
const RANGE_QUERY: usize = 4;
const SKIP_RANGE_QUERY: usize = 5;
const COUNT_RANGE_QUERY: usize = 6;
const LIMITED_RANGE_QUERY: usize = 7;
const LIMITED_REVERSE_RANGE_QUERY: usize = 8;

impl Drop for SqliteTx<'_> {
    fn drop(&mut self) {
        // Statements borrow the connection through an intentionally extended
        // lifetime. Finalize them before COMMIT/ROLLBACK, and before either
        // pooling or discarding the connection.
        self.finalize_cached_statements();

        if matches!(
            self.state,
            SqliteTransactionState::ActiveRead | SqliteTransactionState::ActiveWrite
        ) {
            let rollback = self
                .conn
                .as_ref()
                .ok_or_else(|| miette!("active sqlite transaction lost its connection"))
                .and_then(rollback_transaction);
            if let Err(error) = rollback {
                log::error!(
                    "discarding sqlite connection after transaction rollback failed: {error}"
                );
                // A connection whose transaction state is unknown must never
                // re-enter the pool. Cached statements are already finalized,
                // so dropping it cannot retain a SQLite handle through them.
                drop(self.conn.take());
                return;
            }
            self.state = SqliteTransactionState::Finished;
        }

        let Some(conn) = self.conn.take() else {
            return;
        };
        let Some(deadline) = self.pool_return_deadline else {
            self.storage
                .pool
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(conn);
            return;
        };
        let mut conn = Some(conn);
        loop {
            match self.storage.pool.try_lock() {
                Ok(mut pool) => {
                    if let Some(conn) = conn.take() {
                        pool.push(conn);
                    }
                    return;
                }
                Err(TryLockError::Poisoned(error)) => {
                    if let Some(conn) = conn.take() {
                        error.into_inner().push(conn);
                    }
                    return;
                }
                Err(TryLockError::WouldBlock) => {
                    if park_before_deadline(deadline).is_err() {
                        // Closing a spare read connection is safe now that its
                        // cached statements have been finalized. Prefer that
                        // to pinning joined teardown behind the pool mutex.
                        return;
                    }
                }
            }
        }
    }
}

impl<'s> SqliteTx<'s> {
    fn active_connection(&self) -> Result<&ConnectionThreadSafe> {
        if self.state == SqliteTransactionState::Finished {
            bail!("sqlite transaction is already finished");
        }
        self.conn
            .as_ref()
            .ok_or_else(|| miette!("active sqlite transaction lost its connection"))
    }

    fn writable_connection(&self) -> Result<&ConnectionThreadSafe> {
        match self.state {
            SqliteTransactionState::ActiveWrite => self.active_connection(),
            SqliteTransactionState::ActiveRead => {
                bail!("cannot write through a read-only sqlite transaction")
            }
            SqliteTransactionState::Finished => bail!("sqlite transaction is already finished"),
        }
    }

    fn finalize_cached_statements(&mut self) {
        for cached in &mut self.stmts {
            let statement = cached
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            drop(statement);
        }
    }

    fn ensure_stmt(&self, idx: usize) -> Result<()> {
        let conn = self.active_connection()?;
        let mut stmt = self.stmts[idx]
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if stmt.is_none() {
            let query = QUERIES[idx];
            let prepared = conn.prepare(query).into_diagnostic()?;

            // Casting away the lifetime!
            // This is OK because we are abiding by the contract of the underlying C pointer,
            // as required by Sqlite's implementation
            let prepared = unsafe { std::mem::transmute(prepared) };

            *stmt = Some(prepared)
        }
        Ok(())
    }

    fn prepare_range_statement<'a>(
        &'a self,
        query: &str,
        lower: &[u8],
        upper: &[u8],
    ) -> Result<Statement<'a>> {
        let mut statement = self.active_connection()?.prepare(query).into_diagnostic()?;
        statement.bind((1, lower)).into_diagnostic()?;
        statement.bind((2, upper)).into_diagnostic()?;
        Ok(statement)
    }
}

fn error_iterator<'a, T: 'a>(error: miette::Report) -> Box<dyn Iterator<Item = Result<T>> + 'a> {
    Box::new(std::iter::once(Err(error)))
}

impl<'s> StoreTx<'s> for SqliteTx<'s> {
    fn get(&self, key: &[u8], _for_update: bool) -> Result<Option<Vec<u8>>> {
        self.ensure_stmt(GET_QUERY)?;
        let mut statement = self.stmts[GET_QUERY]
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let statement = statement
            .as_mut()
            .ok_or_else(|| miette!("sqlite get statement cache is unexpectedly empty"))?;
        statement.reset().into_diagnostic()?;

        statement.bind((1, key)).into_diagnostic()?;
        Ok(match statement.next().into_diagnostic()? {
            State::Row => {
                let res = statement.read::<Vec<u8>, _>(0).into_diagnostic()?;
                Some(res)
            }
            State::Done => None,
        })
    }

    fn put(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        self.par_put(key, val)
    }

    fn supports_par_put(&self) -> bool {
        true
    }

    fn par_put(&self, key: &[u8], val: &[u8]) -> Result<()> {
        self.writable_connection()?;
        self.ensure_stmt(PUT_QUERY)?;
        let mut statement = self.stmts[PUT_QUERY]
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let statement = statement
            .as_mut()
            .ok_or_else(|| miette!("sqlite put statement cache is unexpectedly empty"))?;
        statement.reset().into_diagnostic()?;

        statement.bind((1, key)).into_diagnostic()?;
        statement.bind((2, val)).into_diagnostic()?;
        while statement.next().into_diagnostic()? != State::Done {}
        Ok(())
    }

    fn del(&mut self, key: &[u8]) -> Result<()> {
        self.par_del(key)
    }

    fn par_del(&self, key: &[u8]) -> Result<()> {
        self.writable_connection()?;
        self.ensure_stmt(DEL_QUERY)?;
        let mut statement = self.stmts[DEL_QUERY]
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let statement = statement
            .as_mut()
            .ok_or_else(|| miette!("sqlite delete statement cache is unexpectedly empty"))?;
        statement.reset().into_diagnostic()?;

        statement.bind((1, key)).into_diagnostic()?;
        while statement.next().into_diagnostic()? != State::Done {}

        Ok(())
    }

    fn del_range_from_persisted(&mut self, lower: &[u8], upper: &[u8]) -> Result<()> {
        let query = r#"
                delete from cozo where k >= ? and k < ?;
            "#;
        let mut statement = self
            .writable_connection()?
            .prepare(query)
            .into_diagnostic()?;

        statement.bind((1, lower)).into_diagnostic()?;
        statement.bind((2, upper)).into_diagnostic()?;
        while statement.next().into_diagnostic()? != State::Done {}
        Ok(())
    }

    fn exists(&self, key: &[u8], _for_update: bool) -> Result<bool> {
        self.ensure_stmt(EXISTS_QUERY)?;
        let mut statement = self.stmts[EXISTS_QUERY]
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let statement = statement
            .as_mut()
            .ok_or_else(|| miette!("sqlite exists statement cache is unexpectedly empty"))?;
        statement.reset().into_diagnostic()?;

        statement.bind((1, key)).into_diagnostic()?;
        Ok(match statement.next().into_diagnostic()? {
            State::Row => true,
            State::Done => false,
        })
    }

    fn commit(&mut self) -> Result<()> {
        if self.state == SqliteTransactionState::Finished {
            bail!("sqlite transaction is already finished");
        }
        self.finalize_cached_statements();
        let conn = self
            .conn
            .as_ref()
            .ok_or_else(|| miette!("active sqlite transaction lost its connection"))?;
        conn.execute("commit;").into_diagnostic()?;
        require_sqlite_autocommit(conn, true, "commit")?;
        self.state = SqliteTransactionState::Finished;
        Ok(())
    }

    fn range_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        // Range scans cannot use cached prepared statements, as several of them
        // can be used at the same time.
        let statement = match self.prepare_range_statement(QUERIES[RANGE_QUERY], lower, upper) {
            Ok(statement) => statement,
            Err(error) => return error_iterator(error),
        };
        Box::new(TupleIter(statement))
    }

    fn range_scan_tuple_limited<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        limit: usize,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        let mut statement =
            match self.prepare_range_statement(QUERIES[LIMITED_RANGE_QUERY], lower, upper) {
                Ok(statement) => statement,
                Err(error) => return error_iterator(error),
            };
        if let Err(error) = statement
            .bind((3, i64::try_from(limit).unwrap_or(i64::MAX)))
            .into_diagnostic()
        {
            return error_iterator(error);
        }
        Box::new(TupleIter(statement))
    }

    fn range_scan_tuple_rev<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        let statement = match self.prepare_range_statement(
            "select k, v from cozo where k >= ? and k < ? order by k desc;",
            lower,
            upper,
        ) {
            Ok(statement) => statement,
            Err(error) => return error_iterator(error),
        };
        Box::new(TupleIter(statement))
    }

    fn range_scan_tuple_rev_limited<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        limit: usize,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a>
    where
        's: 'a,
    {
        let mut statement = match self.prepare_range_statement(
            QUERIES[LIMITED_REVERSE_RANGE_QUERY],
            lower,
            upper,
        ) {
            Ok(statement) => statement,
            Err(error) => return error_iterator(error),
        };
        if let Err(error) = statement
            .bind((3, i64::try_from(limit).unwrap_or(i64::MAX)))
            .into_diagnostic()
        {
            return error_iterator(error);
        }
        Box::new(TupleIter(statement))
    }

    fn range_skip_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        valid_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        let statement = match self
            .active_connection()
            .and_then(|conn| conn.prepare(QUERIES[SKIP_RANGE_QUERY]).into_diagnostic())
        {
            Ok(statement) => statement,
            Err(error) => return error_iterator(error),
        };
        Box::new(SkipIter {
            stmt: statement,
            valid_at,
            next_bound: lower.to_vec(),
            upper_bound: upper.to_vec(),
        })
    }

    fn range_bitemporal_scan_tuple<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
        vt_at: Option<ValidityTs>,
        tt_at: ValidityTs,
    ) -> Box<dyn Iterator<Item = Result<Tuple>> + 'a> {
        // Step-6 seek override: ONE prepared statement for the whole walk,
        // reset + rebound per real seek; sequential probes ride the open
        // cursor (`HybridProbe`). The generic default re-prepares the range
        // query per probe, which dominates.
        let statement = match self
            .active_connection()
            .and_then(|conn| conn.prepare(QUERIES[RANGE_QUERY]).into_diagnostic())
        {
            Ok(statement) => statement,
            Err(error) => return error_iterator(error),
        };
        let mut probe = crate::data::bitemporal::HybridProbe::new(SqliteSeekCursor {
            stmt: statement,
            upper_bound: upper.to_vec(),
        });
        Box::new(crate::data::bitemporal::BitemporalIter::new(
            move |bound: &[u8], far: bool| probe.probe(bound, far),
            lower.to_vec(),
            vt_at,
            tt_at,
        ))
    }

    fn range_scan<'a>(
        &'a self,
        lower: &[u8],
        upper: &[u8],
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        let statement = match self.prepare_range_statement(QUERIES[RANGE_QUERY], lower, upper) {
            Ok(statement) => statement,
            Err(error) => return error_iterator(error),
        };
        Box::new(RawIter(statement))
    }

    fn range_count<'a>(&'a self, lower: &[u8], upper: &[u8]) -> Result<usize>
    where
        's: 'a,
    {
        let mut statement =
            self.prepare_range_statement(QUERIES[COUNT_RANGE_QUERY], lower, upper)?;
        if statement.next().into_diagnostic()? != State::Row {
            bail!("range count query returned no rows");
        }
        let count = statement.read::<i64, _>(0).into_diagnostic()?;
        let count = usize::try_from(count).into_diagnostic()?;
        if statement.next().into_diagnostic()? != State::Done {
            bail!("range count query returned multiple rows");
        }
        Ok(count)
    }

    fn total_scan<'a>(&'a self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + 'a>
    where
        's: 'a,
    {
        let statement = match self.active_connection().and_then(|conn| {
            conn.prepare("select k, v from cozo order by k;")
                .into_diagnostic()
        }) {
            Ok(statement) => statement,
            Err(error) => return error_iterator(error),
        };
        Box::new(RawIter(statement))
    }
}

struct TupleIter<'l>(Statement<'l>);

impl<'l> Iterator for TupleIter<'l> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            Ok(State::Done) => None,
            Ok(State::Row) => Some((|| {
                let k = self.0.read::<Vec<u8>, _>(0).into_diagnostic()?;
                let v = self.0.read::<Vec<u8>, _>(1).into_diagnostic()?;
                try_decode_tuple_from_kv(&k, &v, None)
            })()),
            Err(err) => Some(Err(miette!(err))),
        }
    }
}

struct RawIter<'l>(Statement<'l>);

impl<'l> Iterator for RawIter<'l> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0.next() {
            Ok(State::Done) => None,
            Ok(State::Row) => Some((|| {
                let k = self.0.read::<Vec<u8>, _>(0).into_diagnostic()?;
                let v = self.0.read::<Vec<u8>, _>(1).into_diagnostic()?;
                Ok((k, v))
            })()),
            Err(err) => Some(Err(miette!(err))),
        }
    }
}

/// Pinned-statement cursor for the bitemporal walk (bitemporality step 6).
/// `seek` = reset + rebind the lower bound; `step` = continue the open
/// cursor. The SQL (`k >= ? and k < ?`) enforces the upper bound.
struct SqliteSeekCursor<'l> {
    stmt: Statement<'l>,
    upper_bound: Vec<u8>,
}

impl<'l> SqliteSeekCursor<'l> {
    fn read_row(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        match self.stmt.next().into_diagnostic()? {
            State::Done => Ok(None),
            State::Row => {
                let k = self.stmt.read::<Vec<u8>, _>(0).into_diagnostic()?;
                let v = self.stmt.read::<Vec<u8>, _>(1).into_diagnostic()?;
                Ok(Some((k, v)))
            }
        }
    }
}

impl<'l> crate::data::bitemporal::SeekCursor for SqliteSeekCursor<'l> {
    fn seek(&mut self, bound: &[u8]) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        if bound >= self.upper_bound.as_slice() {
            return Ok(None);
        }
        self.stmt.reset().into_diagnostic()?;
        self.stmt.bind((1, bound)).into_diagnostic()?;
        self.stmt
            .bind((2, &self.upper_bound as &[u8]))
            .into_diagnostic()?;
        self.read_row()
    }

    fn step(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.read_row()
    }
}

struct SkipIter<'l> {
    stmt: Statement<'l>,
    valid_at: ValidityTs,
    next_bound: Vec<u8>,
    upper_bound: Vec<u8>,
}

impl<'l> SkipIter<'l> {
    fn next_inner(&mut self) -> Result<Option<Tuple>> {
        loop {
            self.stmt.reset().into_diagnostic()?;
            self.stmt
                .bind((1, &self.next_bound as &[u8]))
                .into_diagnostic()?;
            self.stmt
                .bind((2, &self.upper_bound as &[u8]))
                .into_diagnostic()?;

            match self.stmt.next().into_diagnostic()? {
                State::Done => return Ok(None),
                State::Row => {
                    let k = self.stmt.read::<Vec<u8>, _>(0).into_diagnostic()?;
                    let (ret, nxt_bound) = match check_key_for_validity(&k, self.valid_at, None) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            self.next_bound.clone_from(&self.upper_bound);
                            return Err(error);
                        }
                    };
                    self.next_bound = nxt_bound;
                    if let Some(mut tup) = ret {
                        let v = self.stmt.read::<Vec<u8>, _>(1).into_diagnostic()?;
                        try_extend_tuple_from_v(&mut tup, &k, &v)?;
                        return Ok(Some(tup));
                    }
                }
            }
        }
    }
}

impl<'l> Iterator for SkipIter<'l> {
    type Item = Result<Tuple>;

    fn next(&mut self) -> Option<Self::Item> {
        swap_option_result(self.next_inner())
    }
}

#[cfg(test)]
mod transaction_tests {
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::{Storage, StoreTx, new_cozo_sqlite};

    const SNAPSHOT_KEY: &[u8] = b"\xffmnestic-sqlite-snapshot-regression";

    fn write_value(storage: &super::SqliteStorage, value: &[u8]) {
        let mut writer = storage.transact(true).unwrap();
        writer.put(SNAPSHOT_KEY, value).unwrap();
        writer.commit().unwrap();
    }

    fn assert_cross_instance_snapshot_is_pinned(bounded: bool) {
        let directory = tempdir().unwrap();
        let path = directory.path().join("snapshot.db");
        let first = new_cozo_sqlite(&path).unwrap();
        let second = new_cozo_sqlite(&path).unwrap();
        write_value(&second.db, b"before");

        let mut reader = if bounded {
            first
                .db
                .transact_read_with_deadline(Instant::now() + Duration::from_secs(5))
                .unwrap()
        } else {
            first.db.transact(false).unwrap()
        };

        // The startup rootpage probe must already have pinned the snapshot:
        // this commit precedes the reader's first user-data query.
        write_value(&second.db, b"after-startup");
        assert_eq!(reader.get(SNAPSHOT_KEY, false).unwrap().unwrap(), b"before");

        // A second independent commit must not appear on a repeated read,
        // either. No sleeps or process-local lock sharing are involved.
        write_value(&second.db, b"after-first-read");
        assert_eq!(reader.get(SNAPSHOT_KEY, false).unwrap().unwrap(), b"before");
        reader.commit().unwrap();

        let mut fresh_reader = first.db.transact(false).unwrap();
        assert_eq!(
            fresh_reader.get(SNAPSHOT_KEY, false).unwrap().unwrap(),
            b"after-first-read"
        );
        fresh_reader.commit().unwrap();
    }

    #[test]
    fn ordinary_read_is_pinned_across_independent_wal_commits() {
        assert_cross_instance_snapshot_is_pinned(false);
    }

    #[test]
    fn bounded_read_is_pinned_across_independent_wal_commits() {
        assert_cross_instance_snapshot_is_pinned(true);
    }

    #[test]
    fn finished_and_read_only_transactions_reject_further_operations() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.db");
        let database = new_cozo_sqlite(path).unwrap();
        let mut reader = database.db.transact(false).unwrap();

        assert!(reader.put(SNAPSHOT_KEY, b"not allowed").is_err());
        reader.commit().unwrap();
        assert!(reader.get(SNAPSHOT_KEY, false).is_err());
        assert!(reader.commit().is_err());
    }

    #[test]
    fn failed_drop_rollback_discards_connection_instead_of_pooling_it() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollback.db");
        let database = new_cozo_sqlite(path).unwrap();
        let transaction = database.db.transact(false).unwrap();
        assert!(database.db.pool_lock_for_tests().is_empty());

        // Simulate SQLite having implicitly ended the transaction while our
        // explicit state still requires rollback. Drop's ROLLBACK now fails
        // with "no transaction is active", so this handle must be discarded.
        transaction
            .conn
            .as_ref()
            .unwrap()
            .execute("rollback;")
            .unwrap();
        drop(transaction);
        assert!(database.db.pool_lock_for_tests().is_empty());

        // The storage remains usable by opening a fresh connection.
        let mut fresh = database.db.transact(false).unwrap();
        fresh.commit().unwrap();
    }
}

#[cfg(test)]
mod existing_snapshot_tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::fs;
    #[cfg(unix)]
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    #[cfg(unix)]
    use std::os::unix::io::AsRawFd;
    use std::panic::AssertUnwindSafe;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use ::sqlite::{Connection, OpenFlags, State};
    use sha2::{Digest, Sha256};
    use sqlite3_sys as ffi;
    use tempfile::tempdir;

    use crate::data::tuple::TupleT;
    use crate::data::value::DataValue;
    use crate::runtime::catalog_codec::{
        CatalogCodec, ManagedCatalogEncodingV1, ManagedCatalogFixtureV1, encode_test_catalog_record,
    };
    use crate::runtime::relation::{AccessLevel, RelationHandle, RelationId};

    use super::{
        ExistingSqliteSnapshotSource, IncompleteDestination, MANAGED_BEGIN_READ_SQL,
        MANAGED_CANONICAL_COMMITMENT_DOMAIN_V1, MANAGED_CATALOG_BLOB_POINTER_REQUESTS,
        MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT, MANAGED_CATALOG_CUMULATIVE_RAW_LIMIT,
        MANAGED_CATALOG_POLICY_DESCRIPTION_V1, MANAGED_CATALOG_POLICY_FINGERPRINT_DOMAIN_V1,
        MANAGED_CATALOG_POLICY_FINGERPRINT_V1, MANAGED_CATALOG_QUERY,
        MANAGED_CATALOG_QUERY_ROW_LIMIT, MANAGED_CATALOG_RELATION_COUNT, MANAGED_CATALOG_ROW_COUNT,
        MANAGED_CATALOG_VALUE_LIMIT, MANAGED_COMMITMENT_ROW_MARKER_V1,
        MANAGED_COMMITMENT_TERMINATOR_V1, MANAGED_INDEX_LIST_CHECK, MANAGED_INDEX_XINFO_CHECK,
        MANAGED_MAIN_ONLY_CHECK, MANAGED_QUERY_ONLY_CHECK, MANAGED_QUERY_ONLY_SQL,
        MANAGED_RAW_COMMITMENT_DOMAIN_V1, MANAGED_ROLLBACK_READ_SQL, MANAGED_SCHEMA_OBJECT_CHECK,
        MANAGED_SQLITE_ATTACHED_LIMIT, MANAGED_SQLITE_CATALOG_LENGTH_LIMIT,
        MANAGED_SQLITE_COLUMN_LIMIT, MANAGED_SQLITE_COMPOUND_SELECT_LIMIT,
        MANAGED_SQLITE_DEFENSIVE_VALUE, MANAGED_SQLITE_ENABLE_TRIGGER_VALUE,
        MANAGED_SQLITE_EXPR_DEPTH_LIMIT, MANAGED_SQLITE_FUNCTION_ARG_LIMIT,
        MANAGED_SQLITE_LIKE_PATTERN_LIMIT, MANAGED_SQLITE_QUERY_ONLY_VALUE,
        MANAGED_SQLITE_SCHEMA_LENGTH_LIMIT, MANAGED_SQLITE_SQL_LENGTH_LIMIT,
        MANAGED_SQLITE_TRIGGER_DEPTH_LIMIT, MANAGED_SQLITE_TRUSTED_SCHEMA_VALUE,
        MANAGED_SQLITE_VARIABLE_LIMIT, MANAGED_SQLITE_VDBE_OP_LIMIT,
        MANAGED_SQLITE_WORKER_THREADS_LIMIT, MANAGED_TABLE_XINFO_CHECK, MAX_ENCODED_KEY_BYTES,
        MIN_SNAPSHOT_SQLITE_VERSION, ManagedCatalogFencePolicy, ManagedExistingSqliteOpenPermitV1,
        ManagedSnapshotPolicy, ManagedSqliteSnapshotReader, ManagedStatement, SQLITE_MAIN_SCHEMA,
        SQLITE_SCHEMA_PROBE, SQLITE_SIDECAR_SUFFIXES, Storage, checked_catalog_total,
        ensure_snapshot_sqlite_runtime, managed_boolean_check, managed_catalog_blob_cells,
        new_cozo_sqlite, new_cozo_sqlite_existing, sidecar_path,
    };

    const _: fn() = || {
        trait AmbiguousIfSend<A> {
            fn marker() {}
        }
        struct IfSend;
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        impl<T: ?Sized + Send> AmbiguousIfSend<IfSend> for T {}

        let _ = <ManagedSqliteSnapshotReader as AmbiguousIfSend<_>>::marker;
    };

    const _: fn() = || {
        trait AmbiguousIfSync<A> {
            fn marker() {}
        }
        struct IfSync;
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<IfSync> for T {}

        let _ = <ManagedSqliteSnapshotReader as AmbiguousIfSync<_>>::marker;
    };

    const _: fn() = || {
        trait AmbiguousIfClone<A> {
            fn marker() {}
        }
        struct IfClone;
        impl<T: ?Sized> AmbiguousIfClone<()> for T {}
        impl<T: Clone> AmbiguousIfClone<IfClone> for T {}

        let _ = <ManagedSqliteSnapshotReader as AmbiguousIfClone<_>>::marker;
    };

    macro_rules! assert_not_send_sync_clone {
        ($type:ty) => {
            const _: fn() = || {
                trait AmbiguousIfSend<A> {
                    fn marker() {}
                }
                struct IfSend;
                impl<T: ?Sized> AmbiguousIfSend<()> for T {}
                impl<T: ?Sized + Send> AmbiguousIfSend<IfSend> for T {}
                let _ = <$type as AmbiguousIfSend<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfSync<A> {
                    fn marker() {}
                }
                struct IfSync;
                impl<T: ?Sized> AmbiguousIfSync<()> for T {}
                impl<T: ?Sized + Sync> AmbiguousIfSync<IfSync> for T {}
                let _ = <$type as AmbiguousIfSync<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfClone<A> {
                    fn marker() {}
                }
                struct IfClone;
                impl<T: ?Sized> AmbiguousIfClone<()> for T {}
                impl<T: Clone> AmbiguousIfClone<IfClone> for T {}
                let _ = <$type as AmbiguousIfClone<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfCopy<A> {
                    fn marker() {}
                }
                struct IfCopy;
                impl<T: ?Sized> AmbiguousIfCopy<()> for T {}
                impl<T: Copy> AmbiguousIfCopy<IfCopy> for T {}
                let _ = <$type as AmbiguousIfCopy<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfDebug<A> {
                    fn marker() {}
                }
                struct IfDebug;
                impl<T: ?Sized> AmbiguousIfDebug<()> for T {}
                impl<T: ?Sized + std::fmt::Debug> AmbiguousIfDebug<IfDebug> for T {}
                let _ = <$type as AmbiguousIfDebug<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfDisplay<A> {
                    fn marker() {}
                }
                struct IfDisplay;
                impl<T: ?Sized> AmbiguousIfDisplay<()> for T {}
                impl<T: ?Sized + std::fmt::Display> AmbiguousIfDisplay<IfDisplay> for T {}
                let _ = <$type as AmbiguousIfDisplay<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfDefault<A> {
                    fn marker() {}
                }
                struct IfDefault;
                impl<T: ?Sized> AmbiguousIfDefault<()> for T {}
                impl<T: Default> AmbiguousIfDefault<IfDefault> for T {}
                let _ = <$type as AmbiguousIfDefault<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfSerialize<A> {
                    fn marker() {}
                }
                struct IfSerialize;
                impl<T: ?Sized> AmbiguousIfSerialize<()> for T {}
                impl<T: ?Sized + serde::Serialize> AmbiguousIfSerialize<IfSerialize> for T {}
                let _ = <$type as AmbiguousIfSerialize<_>>::marker;
            };
            const _: fn() = || {
                trait AmbiguousIfDeserialize<A> {
                    fn marker() {}
                }
                struct IfDeserialize;
                impl<T: ?Sized> AmbiguousIfDeserialize<()> for T {}
                impl<T: serde::Deserialize<'static>> AmbiguousIfDeserialize<IfDeserialize> for T {}
                let _ = <$type as AmbiguousIfDeserialize<_>>::marker;
            };
        };
    }

    assert_not_send_sync_clone!(super::ManagedSqliteRelationAssertionPlannerV1<'static>);
    assert_not_send_sync_clone!(super::ManagedSqliteAssertionTranscriptV1);
    assert_not_send_sync_clone!(super::ManagedSqliteRelationAssertionOutcomeV1);
    assert_not_send_sync_clone!(super::ManagedSqliteRelationAssertionEvidenceV1);
    assert_not_send_sync_clone!(super::ManagedSqliteRecordVisitEvidenceV1);
    assert_not_send_sync_clone!(super::ManagedSqliteClosedMainFileIdentityV1);
    assert_not_send_sync_clone!(super::ManagedSqliteClosedResidualMetadataV1);
    assert_not_send_sync_clone!(super::ManagedSqliteClosedResidualV1);
    assert_not_send_sync_clone!(super::ManagedSqliteClosedSourceEvidenceV1);
    assert_not_send_sync_clone!(super::ManagedSqliteClosedAuditV1);
    assert_not_send_sync_clone!(super::ManagedSqliteClosedRecordVisitV1<()>);
    assert_not_send_sync_clone!(super::ManagedSqliteCatalogAssertionPlannerV1<'static>);
    assert_not_send_sync_clone!(super::ManagedSqliteCatalogFenceAssertionTranscriptV1);
    assert_not_send_sync_clone!(super::ManagedSqliteCatalogFenceAssertionOutcomeV1);
    assert_not_send_sync_clone!(super::ManagedSqliteCatalogFenceAssertionEvidenceV1);
    assert_not_send_sync_clone!(super::ManagedSqliteClosedCatalogFenceV1);
    assert_not_send_sync_clone!(super::ManagedExistingSqliteOpenPermitV1);

    unsafe extern "C" fn mark_sqlite_connection_destroyed(context: *mut std::ffi::c_void) {
        let marker = unsafe { Box::from_raw(context.cast::<Arc<AtomicBool>>()) };
        marker.store(true, Ordering::SeqCst);
    }

    unsafe extern "C" fn deny_sqlite_transaction_authorizer(
        _context: *mut std::ffi::c_void,
        action: std::ffi::c_int,
        _first: *const std::ffi::c_char,
        _second: *const std::ffi::c_char,
        _database: *const std::ffi::c_char,
        _trigger: *const std::ffi::c_char,
    ) -> std::ffi::c_int {
        if action == ffi::SQLITE_TRANSACTION {
            ffi::SQLITE_DENY
        } else {
            ffi::SQLITE_OK
        }
    }

    unsafe extern "C" fn count_sqlite_rollback_authorizer(
        context: *mut std::ffi::c_void,
        action: std::ffi::c_int,
        first: *const std::ffi::c_char,
        _second: *const std::ffi::c_char,
        _database: *const std::ffi::c_char,
        _trigger: *const std::ffi::c_char,
    ) -> std::ffi::c_int {
        if action == ffi::SQLITE_TRANSACTION
            && !first.is_null()
            && unsafe { std::ffi::CStr::from_ptr(first) }.to_bytes() == b"ROLLBACK"
        {
            let calls = unsafe { &*context.cast::<Cell<usize>>() };
            calls.set(calls.get() + 1);
        }
        ffi::SQLITE_OK
    }

    const CLEANUP_PROBE_FUNCTION_NAME: &[u8] = b"mneme_cleanup_probe\0";
    const CLEANUP_PROBE_STATEMENT: &[u8] = b"SELECT 1;\0";

    #[derive(Debug, Eq, PartialEq)]
    struct StableMetadata {
        len: u64,
        readonly: bool,
        modified: Option<SystemTime>,
        #[cfg(unix)]
        dev: u64,
        #[cfg(unix)]
        ino: u64,
        #[cfg(unix)]
        mode: u32,
        #[cfg(unix)]
        nlink: u64,
        #[cfg(unix)]
        uid: u32,
        #[cfg(unix)]
        gid: u32,
        #[cfg(unix)]
        rdev: u64,
        #[cfg(unix)]
        blocks: u64,
        #[cfg(unix)]
        block_size: u64,
        #[cfg(unix)]
        mtime: i64,
        #[cfg(unix)]
        mtime_nsec: i64,
        #[cfg(unix)]
        ctime: i64,
        #[cfg(unix)]
        ctime_nsec: i64,
    }

    fn stable_metadata(path: &Path) -> StableMetadata {
        let metadata = fs::metadata(path).unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        StableMetadata {
            len: metadata.len(),
            readonly: metadata.permissions().readonly(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            dev: metadata.dev(),
            #[cfg(unix)]
            ino: metadata.ino(),
            #[cfg(unix)]
            mode: metadata.mode(),
            #[cfg(unix)]
            nlink: metadata.nlink(),
            #[cfg(unix)]
            uid: metadata.uid(),
            #[cfg(unix)]
            gid: metadata.gid(),
            #[cfg(unix)]
            rdev: metadata.rdev(),
            #[cfg(unix)]
            blocks: metadata.blocks(),
            #[cfg(unix)]
            block_size: metadata.blksize(),
            #[cfg(unix)]
            mtime: metadata.mtime(),
            #[cfg(unix)]
            mtime_nsec: metadata.mtime_nsec(),
            #[cfg(unix)]
            ctime: metadata.ctime(),
            #[cfg(unix)]
            ctime_nsec: metadata.ctime_nsec(),
        }
    }

    fn fingerprint(path: &Path) -> (Vec<u8>, StableMetadata) {
        (fs::read(path).unwrap(), stable_metadata(path))
    }

    fn directory_byte_census(path: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut census: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let file_type = entry.file_type().unwrap();
                assert!(
                    file_type.is_file(),
                    "test directory contains a non-file entry"
                );
                (entry.file_name(), fs::read(entry.path()).unwrap())
            })
            .collect();
        census.sort_by(|left, right| left.0.cmp(&right.0));
        census
    }

    fn test_path(directory: &tempfile::TempDir, name: &str) -> PathBuf {
        fs::canonicalize(directory.path()).unwrap().join(name)
    }

    fn existing_open_permit(path: &Path) -> miette::Result<ManagedExistingSqliteOpenPermitV1> {
        let fence = ExistingSqliteSnapshotSource::open(path)?
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |_| Ok(()))?;
        Ok(fence.into_existing_sqlite_open_permit_v1())
    }

    fn create_source(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "CREATE TABLE payload (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                 INSERT INTO payload (id, value) VALUES (1, 'copied');",
            )
            .unwrap();
        drop(connection);
        assert_no_sidecars(path);
    }

    fn create_managed_cozo_source(path: &Path) {
        let database = new_cozo_sqlite(path).unwrap();
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        assert_no_sidecars(path);
    }

    fn create_populated_managed_cozo_source(path: &Path) {
        let database = new_cozo_sqlite(path).unwrap();
        for index in 0..super::MANAGED_CATALOG_RELATION_COUNT {
            let schema = if index == 0 {
                "{id: Int => value: String}"
            } else {
                "{id: Int}"
            };
            database
                .run_script(
                    &format!(":create catalog_{index:02} {schema}"),
                    Default::default(),
                    crate::ScriptMutability::Mutable,
                )
                .unwrap();
        }
        database
            .run_script(
                "?[id, value] <- [[1, 'one'], [2, 'two'], [3, 'three']] \
                 :put catalog_00 {id => value}",
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        assert_no_sidecars(path);
    }

    fn create_clean_wal_header_managed_cozo_source(path: &Path) {
        create_populated_managed_cozo_source(path);
        // This is the ordinary runtime open/close path: it switches the main
        // header back to WAL mode and does not call prepare_for_file_move.
        drop(new_cozo_sqlite(path).unwrap());
        assert_no_sidecars(path);
        let header = fs::read(path).unwrap();
        assert_eq!((header[18], header[19]), (2, 2));
    }

    fn create_sidecar_free_wal_headerless_cozo_source(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "PRAGMA journal_mode=WAL;
                 PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE cozo
                 (
                     k BLOB primary key,
                     v BLOB
                 );
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .unwrap();
        drop(connection);
        assert_no_sidecars(path);
        let header = fs::read(path).unwrap();
        assert_eq!((header[18], header[19]), (2, 2));
    }

    fn create_relation_assertion_source(path: &Path) {
        let database = new_cozo_sqlite(path).unwrap();
        for index in 0..super::MANAGED_CATALOG_RELATION_COUNT {
            let schema = match index {
                0 => "{key: String => value: String}",
                1 => "{key: String => value: Any}",
                2 => "{key: String => value: String, extra: String}",
                3 | 4 => "{key: String => value: String}",
                _ => "{id: Int}",
            };
            database
                .run_script(
                    &format!(":create assertion_{index:02} {schema}"),
                    Default::default(),
                    crate::ScriptMutability::Mutable,
                )
                .unwrap();
        }
        let maximum_key = "x".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT);
        let maximum_value = "y".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT);
        database
            .run_script(
                &format!(
                    "?[key, value] <- [\
                       ['exact', 'value'], \
                       ['choice-zero', 'old'], \
                       ['choice-one', 'new'], \
                       ['point-three', 'three'], \
                       ['point-four', 'four'], \
                       ['point-five', 'five'], \
                       ['point-six', 'six'], \
                       ['{maximum_key}', '{maximum_value}']] \
                     :put assertion_00 {{key => value}}"
                ),
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database
            .run_script(
                "?[key, value] <- [['wrong-type', 7]] \
                 :put assertion_01 {key => value}",
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database
            .run_script(
                "?[key, value, extra] <- [['extra-value', 'value', 'trailing']] \
                 :put assertion_02 {key => value, extra}",
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database
            .run_script(
                "?[key, value] <- [['sentinel', 'ATTACKER_DATABASE_SENTINEL']] \
                 :put assertion_03 {key => value}",
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        let oversized_database_value = "q".repeat(1_100);
        database
            .run_script(
                &format!(
                    "?[key, value] <- [['oversized', '{oversized_database_value}']] \
                     :put assertion_04 {{key => value}}"
                ),
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        assert_no_sidecars(path);
    }

    fn create_catalog_fence_count_source(path: &Path, rows: usize) {
        create_relation_assertion_source(path);
        let database = new_cozo_sqlite(path).unwrap();
        let input = (0..rows)
            .map(|id| format!("[{id}]"))
            .collect::<Vec<_>>()
            .join(",");
        database
            .run_script(
                &format!("?[id] <- [{input}] :put assertion_05 {{id}}"),
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        assert_no_sidecars(path);
    }

    type CatalogFenceTranscriptBoundaryInput = (&'static str, &'static str, [&'static str; 2]);

    fn catalog_fence_transcript_boundary_text(
        index: usize,
        fill: char,
        byte_len: usize,
    ) -> &'static str {
        assert!(fill.is_ascii());
        let mut value = format!("{index:02}");
        assert_eq!(value.len(), 2);
        value.extend(std::iter::repeat_n(fill, byte_len - value.len()));
        assert_eq!(value.len(), byte_len);
        Box::leak(value.into_boxed_str())
    }

    fn catalog_fence_transcript_boundary_inputs(
        last_tag_len: usize,
    ) -> Vec<CatalogFenceTranscriptBoundaryInput> {
        (0..8)
            .map(|index| {
                let tag_len = if index == 7 { last_tag_len } else { 256 };
                (
                    catalog_fence_transcript_boundary_text(index, 't', tag_len),
                    catalog_fence_transcript_boundary_text(index, 'k', 256),
                    [
                        catalog_fence_transcript_boundary_text(index, 'v', 256),
                        catalog_fence_transcript_boundary_text(index, 'w', 256),
                    ],
                )
            })
            .collect()
    }

    fn create_catalog_fence_transcript_boundary_source(
        path: &Path,
        inputs: &[CatalogFenceTranscriptBoundaryInput],
    ) {
        let database = new_cozo_sqlite(path).unwrap();
        for index in 0..super::MANAGED_CATALOG_RELATION_COUNT {
            let schema = if index == 0 {
                "{key: String => value: String}"
            } else {
                "{id: Int}"
            };
            database
                .run_script(
                    &format!(":create assertion_{index:02} {schema}"),
                    Default::default(),
                    crate::ScriptMutability::Mutable,
                )
                .unwrap();
        }
        let rows = inputs
            .iter()
            .map(|(_, key, alternatives)| format!("['{key}', '{}']", alternatives[0]))
            .collect::<Vec<_>>()
            .join(",");
        database
            .run_script(
                &format!("?[key, value] <- [{rows}] :put assertion_00 {{key => value}}"),
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        assert_no_sidecars(path);
    }

    fn catalog_fence_transcript_boundary_entries(
        relation_id: u64,
        inputs: &[CatalogFenceTranscriptBoundaryInput],
    ) -> Vec<Vec<Vec<u8>>> {
        inputs
            .iter()
            .map(|(tag, key, alternatives)| {
                vec![
                    vec![2],
                    tag.as_bytes().to_vec(),
                    relation_id.to_be_bytes().to_vec(),
                    key.as_bytes().to_vec(),
                    alternatives[0].as_bytes().to_vec(),
                    alternatives[1].as_bytes().to_vec(),
                    vec![0],
                ]
            })
            .collect()
    }

    fn catalog_fence_transcript_framed_bytes(entries: &[Vec<Vec<u8>>]) -> usize {
        9 + entries
            .iter()
            .map(|entry| 1 + 8 + entry.iter().map(|field| 8 + field.len()).sum::<usize>())
            .sum::<usize>()
    }

    fn assertion_relation_id(
        planner: &super::ManagedSqliteRelationAssertionPlannerV1<'_>,
        name: &str,
    ) -> u64 {
        planner
            .catalog()
            .catalog()
            .entries()
            .iter()
            .find(|entry| entry.relation().name() == name)
            .unwrap()
            .relation()
            .id()
    }

    fn record_visit_relation_ids(
        planner: &mut super::ManagedSqliteRelationAssertionPlannerV1<'_>,
    ) -> miette::Result<[u64; 16]> {
        let mut ids = [0_u64; 16];
        for (index, slot) in ids.iter_mut().enumerate() {
            let name = format!("assertion_{index:02}");
            *slot = planner
                .catalog()
                .catalog()
                .entries()
                .iter()
                .find(|entry| entry.relation().name() == name)
                .map(|entry| entry.relation().id())
                .ok_or_else(|| miette::miette!("record-visit fixture relation was missing"))?;
        }
        Ok(ids)
    }

    fn independently_hash_record_visit_transcript(
        path: &Path,
        relation_ids: &[u64; 16],
        row_counts: &[u64; 16],
        total_rows: u64,
        cumulative_callbacks: u64,
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_RECORD_VISIT_TRANSCRIPT_DOMAIN_V1);
        hasher.update([0x00]);
        hasher.update(16_u16.to_be_bytes());
        for (index, (&relation_id, &row_count)) in
            relation_ids.iter().zip(row_counts.iter()).enumerate()
        {
            let ordinal = u16::try_from(index + 1).unwrap();
            hasher.update(ordinal.to_be_bytes());
            hasher.update(relation_id.to_be_bytes());
            hasher.update(row_count.to_be_bytes());
        }
        hasher.update(total_rows.to_be_bytes());

        let connection = Connection::open(path).unwrap();
        let mut observed_total = 0_u64;
        for (index, (&relation_id, &expected_rows)) in
            relation_ids.iter().zip(row_counts.iter()).enumerate()
        {
            let ordinal = u16::try_from(index + 1).unwrap();
            hasher.update([0x01]);
            hasher.update(ordinal.to_be_bytes());
            hasher.update(relation_id.to_be_bytes());
            hasher.update(expected_rows.to_be_bytes());
            let lower = relation_id.to_be_bytes();
            let upper = relation_id.checked_add(1).unwrap().to_be_bytes();
            let mut statement = connection
                .prepare(
                    "SELECT k, v FROM cozo INDEXED BY sqlite_autoindex_cozo_1 \
                     WHERE k >= ?1 AND k < ?2 ORDER BY k;",
                )
                .unwrap();
            statement.bind((1, lower.as_slice())).unwrap();
            statement.bind((2, upper.as_slice())).unwrap();
            let mut rows = 0_u64;
            while statement.next().unwrap() == State::Row {
                let key = statement.read::<Vec<u8>, _>(0).unwrap();
                let value = statement.read::<Vec<u8>, _>(1).unwrap();
                rows += 1;
                observed_total += 1;
                hasher.update([0x02]);
                hasher.update(ordinal.to_be_bytes());
                hasher.update(u64::try_from(key.len()).unwrap().to_be_bytes());
                hasher.update(&key);
                hasher.update(u64::try_from(value.len()).unwrap().to_be_bytes());
                hasher.update(&value);
            }
            assert_eq!(rows, expected_rows);
            hasher.update([0x03]);
            hasher.update(ordinal.to_be_bytes());
            hasher.update(rows.to_be_bytes());
        }
        drop(connection);
        assert_eq!(observed_total, total_rows);
        hasher.update([0xff]);
        hasher.update(16_u16.to_be_bytes());
        hasher.update(observed_total.to_be_bytes());
        hasher.update(cumulative_callbacks.to_be_bytes());
        hasher.finalize().into()
    }

    struct RecordVisitDropSpy(Arc<AtomicBool>);

    impl Drop for RecordVisitDropSpy {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct RecordVisitControlSink {
        connection: *mut ffi::sqlite3,
        statement: *mut ffi::sqlite3_stmt,
        configured: bool,
        dropped: Arc<AtomicBool>,
        statement_finalize_code: Arc<AtomicI32>,
    }

    impl Drop for RecordVisitControlSink {
        fn drop(&mut self) {
            if !self.statement.is_null() {
                let code = unsafe { ffi::sqlite3_finalize(self.statement) };
                self.statement = std::ptr::null_mut();
                self.statement_finalize_code.store(code, Ordering::SeqCst);
            }
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn catalog_fence_relation_id(
        planner: &super::ManagedSqliteCatalogAssertionPlannerV1<'_>,
        name: &str,
    ) -> u64 {
        planner
            .catalog()
            .catalog()
            .entries()
            .iter()
            .find(|entry| entry.relation().name() == name)
            .unwrap()
            .relation()
            .id()
    }

    fn prepared_statement_count(connection: &super::SnapshotConnection) -> usize {
        let mut count = 0_usize;
        let mut statement = std::ptr::null_mut();
        loop {
            statement = unsafe { ffi::sqlite3_next_stmt(connection.as_raw(), statement) };
            if statement.is_null() {
                return count;
            }
            count += 1;
        }
    }

    fn install_connection_destruction_probe(
        reader: &ManagedSqliteSnapshotReader,
    ) -> Arc<AtomicBool> {
        let destroyed = Arc::new(AtomicBool::new(false));
        let destructor_context = Box::into_raw(Box::new(Arc::clone(&destroyed))).cast();
        let registration = unsafe {
            ffi::sqlite3_create_function_v2(
                reader.connection.as_ref().unwrap().as_raw(),
                CLEANUP_PROBE_FUNCTION_NAME.as_ptr().cast(),
                0,
                ffi::SQLITE_UTF8,
                destructor_context,
                None,
                None,
                None,
                Some(mark_sqlite_connection_destroyed),
            )
        };
        // SQLite invokes the destructor even when registration fails, so
        // ownership transferred regardless. This fixture requires success.
        assert_eq!(registration, ffi::SQLITE_OK);
        destroyed
    }

    fn closed_audit_error(
        result: miette::Result<super::ManagedSqliteClosedAuditV1>,
        context: &str,
    ) -> miette::Report {
        match result {
            Ok(_) => panic!("{context}"),
            Err(error) => error,
        }
    }

    fn assert_relation_assertion_failure<F>(path: &Path, expected: &str, plan: F)
    where
        F: for<'reader> FnOnce(
            &mut super::ManagedSqliteRelationAssertionPlannerV1<'reader>,
        ) -> miette::Result<()>,
    {
        let mut reader = ExistingSqliteSnapshotSource::open(path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let error = match reader.run_relation_assertions_v1(plan) {
            Ok(_) => panic!("hostile relation assertion unexpectedly passed"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains(expected),
            "expected {expected:?} in bounded diagnostic {message:?}"
        );
        assert!(message.len() < 256, "unbounded diagnostic: {message}");
        assert_eq!(
            prepared_statement_count(reader.connection.as_ref().unwrap()),
            0,
            "assertion statement was not explicitly finalized"
        );
        super::managed_integer_query(
            reader.connection.as_ref().unwrap(),
            super::MANAGED_PAGE_COUNT_QUERY,
            "post-assertion callback cleanup probe",
        )
        .unwrap();
        assert!(reader.run_relation_assertions_v1(|_| Ok(())).is_err());
        assert!(reader.close_and_verify().is_err());
        assert_no_sidecars(path);
    }

    struct TestIndexImposterReset {
        connection: *mut ffi::sqlite3,
        armed: bool,
    }

    impl TestIndexImposterReset {
        fn leave_create_mode(&self) {
            let code = unsafe {
                ffi::sqlite3_test_control(
                    ffi::SQLITE_TESTCTRL_IMPOSTER,
                    self.connection,
                    SQLITE_MAIN_SCHEMA.as_ptr().cast::<std::ffi::c_char>(),
                    0_i32,
                    0_i32,
                )
            };
            assert_eq!(code, ffi::SQLITE_OK);
        }

        fn erase(mut self) {
            let code = self.erase_inner();
            self.armed = false;
            assert_eq!(code, ffi::SQLITE_OK);
        }

        fn erase_inner(&self) -> i32 {
            unsafe {
                ffi::sqlite3_test_control(
                    ffi::SQLITE_TESTCTRL_IMPOSTER,
                    self.connection,
                    SQLITE_MAIN_SCHEMA.as_ptr().cast::<std::ffi::c_char>(),
                    0_i32,
                    1_i32,
                )
            }
        }
    }

    impl Drop for TestIndexImposterReset {
        fn drop(&mut self) {
            if self.armed {
                let _ = self.erase_inner();
            }
        }
    }

    fn test_single_i64(connection: &Connection, sql: &str) -> i64 {
        let mut statement = connection.prepare(sql).unwrap();
        assert_eq!(statement.next().unwrap(), State::Row);
        let value = statement.read::<i64, _>(0).unwrap();
        assert_eq!(statement.next().unwrap(), State::Done);
        value
    }

    /// Delete one real positive-relation primary-index cell without touching
    /// the table b-tree. SQLite's connection-local test-control imposter uses
    /// SQLite's own b-tree writer, avoiding raw page-byte forgery.
    fn corrupt_primary_index_by_imposter_delete(path: &Path) -> bool {
        for option in [
            b"OMIT_TEST_CONTROL\0".as_slice(),
            b"UNTESTABLE\0".as_slice(),
        ] {
            if unsafe { ffi::sqlite3_compileoption_used(option.as_ptr().cast()) } != 0 {
                eprintln!(
                    "skipping SQLite imposter corruption fixture: linked runtime is untestable"
                );
                return false;
            }
        }

        let connection = Connection::open(path).unwrap();
        let mut selected = connection
            .prepare(
                "SELECT k, rowid FROM cozo NOT INDEXED \
                 WHERE k >= x'0000000000000001' ORDER BY rowid LIMIT 1;",
            )
            .unwrap();
        assert_eq!(selected.next().unwrap(), State::Row);
        let key = selected.read::<Vec<u8>, _>(0).unwrap();
        let rowid = selected.read::<i64, _>(1).unwrap();
        assert!(RelationId::try_raw_decode_prefix(&key).unwrap().0 > 0);
        assert_eq!(selected.next().unwrap(), State::Done);
        drop(selected);

        let rootpage = test_single_i64(
            &connection,
            "SELECT rootpage FROM sqlite_schema \
             WHERE type='index' AND name='sqlite_autoindex_cozo_1';",
        );
        let rootpage = i32::try_from(rootpage).unwrap();
        assert!(rootpage > 1);
        let code = unsafe {
            ffi::sqlite3_test_control(
                ffi::SQLITE_TESTCTRL_IMPOSTER,
                connection.as_raw(),
                SQLITE_MAIN_SCHEMA.as_ptr().cast::<std::ffi::c_char>(),
                1_i32,
                rootpage,
            )
        };
        assert_eq!(code, ffi::SQLITE_OK);
        let reset = TestIndexImposterReset {
            connection: connection.as_raw(),
            armed: true,
        };
        connection
            .execute(
                "CREATE TABLE mneme_corrupt_primary(\
                    k BLOB COLLATE BINARY, rid INTEGER, PRIMARY KEY(k,rid)\
                 ) WITHOUT ROWID;",
            )
            .unwrap();
        reset.leave_create_mode();

        assert_eq!(
            test_single_i64(
                &connection,
                "SELECT count(*) FROM sqlite_schema \
                 WHERE name='mneme_corrupt_primary';",
            ),
            0
        );
        assert_eq!(
            test_single_i64(
                &connection,
                "SELECT \
                   (SELECT count(*) FROM mneme_corrupt_primary) = \
                     (SELECT count(*) FROM cozo NOT INDEXED) \
                 AND NOT EXISTS (\
                   SELECT 1 FROM cozo AS t NOT INDEXED \
                   WHERE NOT EXISTS (\
                     SELECT 1 FROM mneme_corrupt_primary AS i \
                     WHERE i.k=t.k AND i.rid=t.rowid)) \
                 AND NOT EXISTS (\
                   SELECT 1 FROM mneme_corrupt_primary AS i \
                   WHERE NOT EXISTS (\
                     SELECT 1 FROM cozo AS t NOT INDEXED \
                     WHERE t.k=i.k AND t.rowid=i.rid));",
            ),
            1,
            "imposter schema did not exactly map the primary-index image"
        );

        connection.execute("BEGIN IMMEDIATE;").unwrap();
        let delete_result = (|| -> std::result::Result<(), String> {
            let mut delete = connection
                .prepare("DELETE FROM mneme_corrupt_primary WHERE k=? AND rid=?;")
                .map_err(|error| error.to_string())?;
            delete
                .bind((1, key.as_slice()))
                .map_err(|error| error.to_string())?;
            delete.bind((2, rowid)).map_err(|error| error.to_string())?;
            while delete.next().map_err(|error| error.to_string())? != State::Done {}
            drop(delete);
            if unsafe { ffi::sqlite3_changes64(connection.as_raw()) } != 1 {
                return Err("imposter delete changed other than exactly one index cell".to_owned());
            }
            connection
                .execute("COMMIT;")
                .map_err(|error| error.to_string())?;
            Ok(())
        })();
        if let Err(error) = delete_result {
            let _ = connection.execute("ROLLBACK;");
            panic!("SQLite imposter corruption fixture failed: {error}");
        }

        reset.erase();
        assert_eq!(
            test_single_i64(
                &connection,
                "SELECT count(*) FROM sqlite_schema \
                 WHERE name='mneme_corrupt_primary';",
            ),
            0
        );
        drop(connection);
        assert_no_sidecars(path);
        true
    }

    fn relation_rows(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
        let connection = Connection::open(path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT k, v FROM cozo INDEXED BY sqlite_autoindex_cozo_1 \
                 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' \
                 ORDER BY k;",
            )
            .unwrap();
        let mut rows = Vec::new();
        while statement.next().unwrap() == State::Row {
            let key = statement.read::<Vec<u8>, _>(0).unwrap();
            let value = statement.read::<Vec<u8>, _>(1).unwrap();
            let tuple = crate::try_decode_tuple_from_key(&key, 2).unwrap();
            if matches!(tuple.as_slice(), [DataValue::Str(_)]) {
                rows.push((key, value));
            }
        }
        drop(statement);
        drop(connection);
        rows
    }

    fn replace_raw_row(path: &Path, old_key: &[u8], new_key: &[u8], value: &[u8]) {
        let connection = Connection::open(path).unwrap();
        let mut delete = connection.prepare("DELETE FROM cozo WHERE k = ?;").unwrap();
        delete.bind((1, old_key)).unwrap();
        while delete.next().unwrap() != State::Done {}
        drop(delete);
        let mut insert = connection
            .prepare("INSERT INTO cozo (k, v) VALUES (?, ?);")
            .unwrap();
        insert.bind((1, new_key)).unwrap();
        insert.bind((2, value)).unwrap();
        while insert.next().unwrap() != State::Done {}
        drop(insert);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn update_raw_value(path: &Path, key: &[u8], value: &[u8]) {
        let connection = Connection::open(path).unwrap();
        let mut update = connection
            .prepare("UPDATE cozo SET v = ? WHERE k = ?;")
            .unwrap();
        update.bind((1, value)).unwrap();
        update.bind((2, key)).unwrap();
        while update.next().unwrap() != State::Done {}
        drop(update);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn insert_raw_row(path: &Path, key: &[u8], value: &[u8]) {
        let connection = Connection::open(path).unwrap();
        let mut insert = connection
            .prepare("INSERT INTO cozo (k, v) VALUES (?, ?);")
            .unwrap();
        insert.bind((1, key)).unwrap();
        insert.bind((2, value)).unwrap();
        while insert.next().unwrap() != State::Done {}
        drop(insert);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn insert_raw_row_first(path: &Path, key: &[u8], value: &[u8]) {
        let connection = Connection::open(path).unwrap();
        let mut insert = connection
            .prepare("INSERT INTO cozo (rowid, k, v) VALUES (-1, ?, ?);")
            .unwrap();
        insert.bind((1, key)).unwrap();
        insert.bind((2, value)).unwrap();
        while insert.next().unwrap() != State::Done {}
        drop(insert);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn insert_raw_text_value_first(path: &Path, key: &[u8], value: &str) {
        let connection = Connection::open(path).unwrap();
        let mut insert = connection
            .prepare("INSERT INTO cozo (rowid, k, v) VALUES (-1, ?, ?);")
            .unwrap();
        insert.bind((1, key)).unwrap();
        insert.bind((2, value)).unwrap();
        while insert.next().unwrap() != State::Done {}
        drop(insert);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn insert_raw_oversize_value_first(path: &Path, key: &[u8]) {
        let connection = Connection::open(path).unwrap();
        let mut insert = connection
            .prepare(
                "INSERT INTO cozo (rowid, k, v) \
                 VALUES (-1, ?, zeroblob(1048577));",
            )
            .unwrap();
        insert.bind((1, key)).unwrap();
        while insert.next().unwrap() != State::Done {}
        drop(insert);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn delete_raw_row(path: &Path, key: &[u8]) {
        let connection = Connection::open(path).unwrap();
        let mut delete = connection.prepare("DELETE FROM cozo WHERE k = ?;").unwrap();
        delete.bind((1, key)).unwrap();
        while delete.next().unwrap() != State::Done {}
        drop(delete);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn prepare_test_statement(connection: &Connection, sql: &'static [u8]) -> ManagedStatement {
        assert_eq!(sql.last(), Some(&0));
        let mut raw = std::ptr::null_mut();
        let code = unsafe {
            ffi::sqlite3_prepare_v2(
                connection.as_raw(),
                sql.as_ptr().cast(),
                -1,
                &mut raw,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(code, ffi::SQLITE_OK);
        assert!(!raw.is_null());
        ManagedStatement { raw }
    }

    fn primary_index_catalog_error(reader: &mut ManagedSqliteSnapshotReader) -> miette::Report {
        match reader.inspect_primary_index_catalog_v1() {
            Ok(_) => panic!("managed primary-index catalog unexpectedly passed"),
            Err(error) => error,
        }
    }

    fn catalog_commitments(path: &Path) -> ([u8; 32], [u8; 32]) {
        let mut reader = ExistingSqliteSnapshotSource::open(path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let observation = reader.inspect_primary_index_catalog_v1().unwrap();
        let commitments = (
            *observation.raw_commitment(),
            *observation.canonical_commitment(),
        );
        reader.close_and_verify().unwrap();
        commitments
    }

    fn update_policy_fingerprint_field(
        hasher: &mut Sha256,
        field_count: &mut u64,
        label: &[u8],
        value: &[u8],
    ) {
        let label_len = u64::try_from(label.len()).expect("test label length must fit u64");
        let value_len = u64::try_from(value.len()).expect("test value length must fit u64");
        hasher.update([0x01]);
        hasher.update(label_len.to_be_bytes());
        hasher.update(label);
        hasher.update(value_len.to_be_bytes());
        hasher.update(value);
        *field_count += 1;
    }

    fn derive_catalog_policy_fingerprint_v1() -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(MANAGED_CATALOG_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;

        for (label, sql) in [
            (b"sqlite-schema-probe".as_slice(), SQLITE_SCHEMA_PROBE),
            (b"query-only-set".as_slice(), MANAGED_QUERY_ONLY_SQL),
            (b"query-only-check".as_slice(), MANAGED_QUERY_ONLY_CHECK),
            (b"main-only-check".as_slice(), MANAGED_MAIN_ONLY_CHECK),
            (
                b"schema-object-check".as_slice(),
                MANAGED_SCHEMA_OBJECT_CHECK,
            ),
            (b"table-xinfo-check".as_slice(), MANAGED_TABLE_XINFO_CHECK),
            (b"index-list-check".as_slice(), MANAGED_INDEX_LIST_CHECK),
            (b"index-xinfo-check".as_slice(), MANAGED_INDEX_XINFO_CHECK),
            (b"begin-read".as_slice(), MANAGED_BEGIN_READ_SQL),
            (b"rollback-read".as_slice(), MANAGED_ROLLBACK_READ_SQL),
            (b"catalog-query".as_slice(), MANAGED_CATALOG_QUERY),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut field_count, label, sql);
        }

        for (label, value) in [
            (
                b"schema-length-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_SCHEMA_LENGTH_LIMIT).unwrap(),
            ),
            (
                b"sql-length-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_SQL_LENGTH_LIMIT).unwrap(),
            ),
            (
                b"column-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_COLUMN_LIMIT).unwrap(),
            ),
            (
                b"expression-depth-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_EXPR_DEPTH_LIMIT).unwrap(),
            ),
            (
                b"compound-select-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_COMPOUND_SELECT_LIMIT).unwrap(),
            ),
            (
                b"vdbe-op-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_VDBE_OP_LIMIT).unwrap(),
            ),
            (
                b"function-argument-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_FUNCTION_ARG_LIMIT).unwrap(),
            ),
            (
                b"like-pattern-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_LIKE_PATTERN_LIMIT).unwrap(),
            ),
            (
                b"variable-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_VARIABLE_LIMIT).unwrap(),
            ),
            (
                b"attached-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_ATTACHED_LIMIT).unwrap(),
            ),
            (
                b"trigger-depth-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_TRIGGER_DEPTH_LIMIT).unwrap(),
            ),
            (
                b"worker-threads-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_WORKER_THREADS_LIMIT).unwrap(),
            ),
            (
                b"dbconfig-defensive-value".as_slice(),
                u64::try_from(MANAGED_SQLITE_DEFENSIVE_VALUE).unwrap(),
            ),
            (
                b"dbconfig-trusted-schema-value".as_slice(),
                u64::try_from(MANAGED_SQLITE_TRUSTED_SCHEMA_VALUE).unwrap(),
            ),
            (
                b"dbconfig-enable-trigger-value".as_slice(),
                u64::try_from(MANAGED_SQLITE_ENABLE_TRIGGER_VALUE).unwrap(),
            ),
            (
                b"query-only-value".as_slice(),
                u64::try_from(MANAGED_SQLITE_QUERY_ONLY_VALUE).unwrap(),
            ),
            (
                b"catalog-sqlite-length-limit".as_slice(),
                u64::try_from(MANAGED_SQLITE_CATALOG_LENGTH_LIMIT).unwrap(),
            ),
            (
                b"encoded-key-limit".as_slice(),
                u64::try_from(MAX_ENCODED_KEY_BYTES).unwrap(),
            ),
            (
                b"catalog-value-limit".as_slice(),
                u64::try_from(MANAGED_CATALOG_VALUE_LIMIT).unwrap(),
            ),
            (
                b"catalog-cumulative-raw-limit".as_slice(),
                u64::try_from(MANAGED_CATALOG_CUMULATIVE_RAW_LIMIT).unwrap(),
            ),
            (
                b"catalog-cumulative-canonical-limit".as_slice(),
                u64::try_from(MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT).unwrap(),
            ),
            (
                b"catalog-relation-count".as_slice(),
                u64::try_from(MANAGED_CATALOG_RELATION_COUNT).unwrap(),
            ),
            (
                b"catalog-row-count".as_slice(),
                u64::try_from(MANAGED_CATALOG_ROW_COUNT).unwrap(),
            ),
            (
                b"catalog-query-row-limit".as_slice(),
                u64::try_from(MANAGED_CATALOG_QUERY_ROW_LIMIT).unwrap(),
            ),
        ] {
            update_policy_fingerprint_field(
                &mut hasher,
                &mut field_count,
                label,
                &value.to_be_bytes(),
            );
        }

        for (label, value) in [
            (
                b"raw-commitment-domain".as_slice(),
                MANAGED_RAW_COMMITMENT_DOMAIN_V1,
            ),
            (
                b"canonical-commitment-domain".as_slice(),
                MANAGED_CANONICAL_COMMITMENT_DOMAIN_V1,
            ),
            (
                b"commitment-row-marker".as_slice(),
                MANAGED_COMMITMENT_ROW_MARKER_V1.as_slice(),
            ),
            (
                b"commitment-terminator".as_slice(),
                MANAGED_COMMITMENT_TERMINATOR_V1.as_slice(),
            ),
            (
                b"policy-projection-description".as_slice(),
                MANAGED_CATALOG_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut field_count, label, value);
        }

        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        hasher.finalize().into()
    }

    fn derive_physical_policy_fingerprint_v1() -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;

        for (label, sql) in [
            (b"mmap-disable".as_slice(), super::MANAGED_MMAP_DISABLE_SQL),
            (b"cache-size-set".as_slice(), super::MANAGED_CACHE_SIZE_SQL),
            (
                b"cell-size-check-set".as_slice(),
                super::MANAGED_CELL_SIZE_CHECK_SQL,
            ),
            (b"mmap-check".as_slice(), super::MANAGED_MMAP_CHECK),
            (
                b"cache-size-check".as_slice(),
                super::MANAGED_CACHE_SIZE_CHECK,
            ),
            (
                b"cell-size-check".as_slice(),
                super::MANAGED_CELL_SIZE_CHECK,
            ),
            (b"begin-read".as_slice(), MANAGED_BEGIN_READ_SQL),
            (b"snapshot-pin".as_slice(), SQLITE_SCHEMA_PROBE),
            (b"query-only-check".as_slice(), MANAGED_QUERY_ONLY_CHECK),
            (b"main-only-check".as_slice(), MANAGED_MAIN_ONLY_CHECK),
            (
                b"schema-object-check".as_slice(),
                MANAGED_SCHEMA_OBJECT_CHECK,
            ),
            (b"table-xinfo-check".as_slice(), MANAGED_TABLE_XINFO_CHECK),
            (b"index-list-check".as_slice(), MANAGED_INDEX_LIST_CHECK),
            (b"index-xinfo-check".as_slice(), MANAGED_INDEX_XINFO_CHECK),
            (
                b"page-count-query".as_slice(),
                super::MANAGED_PAGE_COUNT_QUERY,
            ),
            (
                b"page-size-query".as_slice(),
                super::MANAGED_PAGE_SIZE_QUERY,
            ),
            (
                b"integrity-check".as_slice(),
                super::MANAGED_INTEGRITY_CHECK_QUERY,
            ),
            (
                b"table-census".as_slice(),
                super::MANAGED_TABLE_CENSUS_QUERY,
            ),
            (b"point-query".as_slice(), super::MANAGED_POINT_QUERY),
            (
                b"covering-index-query".as_slice(),
                super::MANAGED_COVERING_INDEX_QUERY,
            ),
            (
                b"covering-index-plan-query".as_slice(),
                super::MANAGED_COVERING_INDEX_PLAN_QUERY,
            ),
            (b"rollback-read".as_slice(), MANAGED_ROLLBACK_READ_SQL),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut field_count, label, sql);
        }

        for (label, value) in [
            (
                b"minimum-sqlite-version".as_slice(),
                u64::try_from(MIN_SNAPSHOT_SQLITE_VERSION).unwrap(),
            ),
            (
                b"maximum-file-bytes".as_slice(),
                super::MANAGED_PHYSICAL_MAX_FILE_BYTES,
            ),
            (
                b"maximum-page-count".as_slice(),
                super::MANAGED_PHYSICAL_MAX_PAGE_COUNT,
            ),
            (
                b"cache-size-kib".as_slice(),
                u64::try_from(super::MANAGED_PHYSICAL_CACHE_SIZE_KIB).unwrap(),
            ),
            (
                b"progress-interval".as_slice(),
                u64::try_from(super::MANAGED_PHYSICAL_PROGRESS_INTERVAL).unwrap(),
            ),
            (
                b"progress-callback-limit".as_slice(),
                super::MANAGED_PHYSICAL_PROGRESS_CALLBACK_LIMIT,
            ),
            (
                b"encoded-key-limit".as_slice(),
                u64::try_from(MAX_ENCODED_KEY_BYTES).unwrap(),
            ),
            (
                b"positive-row-value-limit".as_slice(),
                u64::try_from(super::MANAGED_ROW_VALUE_LIMIT).unwrap(),
            ),
            (
                b"id-zero-value-limit".as_slice(),
                u64::try_from(MANAGED_CATALOG_VALUE_LIMIT).unwrap(),
            ),
            (
                b"id-zero-row-count".as_slice(),
                u64::try_from(MANAGED_CATALOG_ROW_COUNT).unwrap(),
            ),
            (
                b"runtime-version-byte-limit".as_slice(),
                u64::try_from(super::MANAGED_SQLITE_RUNTIME_VERSION_LIMIT).unwrap(),
            ),
            (
                b"runtime-source-id-byte-limit".as_slice(),
                u64::try_from(super::MANAGED_SQLITE_RUNTIME_SOURCE_ID_LIMIT).unwrap(),
            ),
        ] {
            update_policy_fingerprint_field(
                &mut hasher,
                &mut field_count,
                label,
                &value.to_be_bytes(),
            );
        }

        for (label, value) in [
            (b"main-schema".as_slice(), SQLITE_MAIN_SCHEMA),
            (b"required-target-endian".as_slice(), b"little".as_slice()),
            (
                b"covering-index-plan-detail".as_slice(),
                super::MANAGED_COVERING_INDEX_PLAN_DETAIL_V1,
            ),
            (
                b"runtime-identity-domain".as_slice(),
                super::MANAGED_SQLITE_RUNTIME_IDENTITY_FINGERPRINT_DOMAIN_V1,
            ),
            (
                b"catalog-policy-fingerprint".as_slice(),
                MANAGED_CATALOG_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-relation-id-policy-fingerprint".as_slice(),
                super::STORED_RELATION_ID_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-memcmp-key-policy-fingerprint".as_slice(),
                super::STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-msgpack-exact-codec-policy-fingerprint".as_slice(),
                super::STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-msgpack-row-policy-fingerprint".as_slice(),
                super::STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-msgpack-relation-catalog-policy-fingerprint".as_slice(),
                super::STORED_MSGPACK_RELATION_CATALOG_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-row-datavalue-codec-policy-fingerprint".as_slice(),
                super::STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"table-commitment-domain".as_slice(),
                super::MANAGED_PHYSICAL_TABLE_COMMITMENT_DOMAIN_V1,
            ),
            (
                b"index-commitment-domain".as_slice(),
                super::MANAGED_PHYSICAL_INDEX_COMMITMENT_DOMAIN_V1,
            ),
            (
                b"commitment-row-marker".as_slice(),
                MANAGED_COMMITMENT_ROW_MARKER_V1.as_slice(),
            ),
            (
                b"commitment-terminator".as_slice(),
                MANAGED_COMMITMENT_TERMINATOR_V1.as_slice(),
            ),
            (
                b"policy-description".as_slice(),
                super::MANAGED_PHYSICAL_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut field_count, label, value);
        }

        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        hasher.finalize().into()
    }

    fn derive_assertion_policy_fingerprint_v1() -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_ASSERTION_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;

        update_policy_fingerprint_field(
            &mut hasher,
            &mut field_count,
            b"point-query",
            super::MANAGED_POINT_QUERY,
        );
        for (label, value) in [
            (
                b"assertion-limit".as_slice(),
                u64::try_from(super::MANAGED_ASSERTION_LIMIT).unwrap(),
            ),
            (
                b"point-limit".as_slice(),
                u64::try_from(super::MANAGED_ASSERTION_POINT_LIMIT).unwrap(),
            ),
            (
                b"string-byte-limit".as_slice(),
                u64::try_from(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT).unwrap(),
            ),
            (
                b"point-value-byte-limit".as_slice(),
                u64::try_from(super::MANAGED_ASSERTION_POINT_VALUE_BYTE_LIMIT).unwrap(),
            ),
            (
                b"transcript-byte-limit".as_slice(),
                super::MANAGED_ASSERTION_TRANSCRIPT_BYTE_LIMIT,
            ),
            (
                b"progress-interval".as_slice(),
                u64::try_from(super::MANAGED_ASSERTION_PROGRESS_INTERVAL).unwrap(),
            ),
            (
                b"progress-callback-limit".as_slice(),
                super::MANAGED_ASSERTION_PROGRESS_CALLBACK_LIMIT,
            ),
            (b"point-column-count".as_slice(), 5),
            (b"point-parameter-count".as_slice(), 1),
            (b"relation-id-exclusive-end".as_slice(), 1_u64 << 48),
        ] {
            update_policy_fingerprint_field(
                &mut hasher,
                &mut field_count,
                label,
                &value.to_be_bytes(),
            );
        }

        for (label, value) in [
            (
                b"transcript-domain".as_slice(),
                super::MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1,
            ),
            (b"entry-marker".as_slice(), &[0x01][..]),
            (b"terminator-marker".as_slice(), &[0xff][..]),
            (b"assertion-kind-markers".as_slice(), &[0x00, 0x01, 0x02][..]),
            (
                b"transcript-framing".as_slice(),
                b"entry-marker || repeated(u64be(field-length) || field) || u64be(field-count); terminator-marker || u64be(entry-count)".as_slice(),
            ),
            (
                b"count-field-order".as_slice(),
                b"kind,relation-id-u64be,expected-count-u64be".as_slice(),
            ),
            (
                b"exact-pair-field-order".as_slice(),
                b"kind,relation-id-u64be,key,value".as_slice(),
            ),
            (
                b"one-of-field-order".as_slice(),
                b"kind,outcome-tag,relation-id-u64be,key,alternative-0,alternative-1,selected-index-u8".as_slice(),
            ),
            (
                b"prepare-contract".as_slice(),
                b"prepare-v3 persistent; exact NUL-terminated SQL with no tail; readonly; five columns; one parameter".as_slice(),
            ),
            (
                b"completion-contract".as_slice(),
                b"one handler for assertion phase; explicit sqlite3_finalize SQLITE_OK; handler unregistered; autocommit off and main READ before planner and after completion".as_slice(),
            ),
            (
                b"stored-relation-id-policy-fingerprint".as_slice(),
                super::STORED_RELATION_ID_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-memcmp-key-policy-fingerprint".as_slice(),
                super::STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-msgpack-exact-codec-policy-fingerprint".as_slice(),
                super::STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-msgpack-row-policy-fingerprint".as_slice(),
                super::STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-row-datavalue-codec-policy-fingerprint".as_slice(),
                super::STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"policy-description".as_slice(),
                super::MANAGED_ASSERTION_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut field_count, label, value);
        }

        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        hasher.finalize().into()
    }

    fn derive_record_visit_policy_fingerprint_v1() -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut fields = 0_u64;
        update_policy_fingerprint_field(
            &mut hasher,
            &mut fields,
            b"range-query",
            super::MANAGED_RECORD_VISIT_RANGE_QUERY_V1,
        );
        for (label, value) in [
            (
                b"relation-count".as_slice(),
                super::MANAGED_RECORD_VISIT_RELATION_COUNT_V1 as u64,
            ),
            (
                b"total-row-limit".as_slice(),
                super::MANAGED_RECORD_VISIT_TOTAL_ROW_LIMIT_V1,
            ),
            (
                b"encoded-key-limit".as_slice(),
                super::MAX_ENCODED_KEY_BYTES as u64,
            ),
            (
                b"row-value-limit".as_slice(),
                super::MANAGED_ROW_VALUE_LIMIT as u64,
            ),
            (
                b"progress-interval".as_slice(),
                super::MANAGED_PHYSICAL_PROGRESS_INTERVAL as u64,
            ),
            (
                b"cumulative-progress-limit".as_slice(),
                super::MANAGED_PHYSICAL_PROGRESS_CALLBACK_LIMIT,
            ),
            (b"range-result-arity".as_slice(), 4),
            (b"range-bind-arity".as_slice(), 3),
            (b"selection-marker".as_slice(), 0),
            (b"relation-marker".as_slice(), 1),
            (b"row-marker".as_slice(), 2),
            (b"relation-end-marker".as_slice(), 3),
            (b"terminator-marker".as_slice(), 0xff),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut fields, label, &value.to_be_bytes());
        }
        for (label, value) in [
            (
                b"transcript-domain".as_slice(),
                super::MANAGED_RECORD_VISIT_TRANSCRIPT_DOMAIN_V1,
            ),
            (
                b"selection-framing".as_slice(),
                b"00 || relation-count-u16be || repeated(ordinal-u16be,relation-id-u64be,row-count-u64be) || declared-total-u64be".as_slice(),
            ),
            (
                b"relation-framing".as_slice(),
                b"01 || ordinal-u16be || relation-id-u64be || expected-rows-u64be".as_slice(),
            ),
            (
                b"row-framing".as_slice(),
                b"02 || ordinal-u16be || key-length-u64be || raw-key || value-length-u64be || raw-value".as_slice(),
            ),
            (
                b"end-framing".as_slice(),
                b"03 || ordinal-u16be || rows-u64be; ff || relation-count-u16be || total-rows-u64be || cumulative-progress-callbacks-u64be".as_slice(),
            ),
            (
                b"range-bounds".as_slice(),
                b"RelationId raw prefix through checked next sentinel; expected+1 limit".as_slice(),
            ),
            (
                b"bind-lifetime".as_slice(),
                b"SQLITE_TRANSIENT".as_slice(),
            ),
            (
                b"prepare-contract".as_slice(),
                b"prepare-v3 persistent; exact NUL-terminated SQL with no tail; readonly; four columns; three parameters".as_slice(),
            ),
            (
                b"completion-contract".as_slice(),
                b"reset and clear before and after each relation; exact DONE/count; SORT=0; FULLSCAN_STEP=0; strict finalize; READ recheck".as_slice(),
            ),
            (
                b"stored-relation-id-policy-fingerprint".as_slice(),
                super::STORED_RELATION_ID_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-memcmp-key-policy-fingerprint".as_slice(),
                super::STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-msgpack-exact-codec-policy-fingerprint".as_slice(),
                super::STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-msgpack-row-policy-fingerprint".as_slice(),
                super::STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"stored-row-datavalue-codec-policy-fingerprint".as_slice(),
                super::STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"policy-description".as_slice(),
                super::MANAGED_RECORD_VISIT_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut fields, label, value);
        }
        hasher.update([0xff]);
        hasher.update(fields.to_be_bytes());
        hasher.finalize().into()
    }

    fn derive_catalog_fence_assertion_policy_fingerprint_v1() -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut fields = 0_u64;
        update_policy_fingerprint_field(
            &mut hasher,
            &mut fields,
            b"range-query",
            super::MANAGED_CATALOG_FENCE_RANGE_QUERY_V1,
        );
        update_policy_fingerprint_field(
            &mut hasher,
            &mut fields,
            b"point-query",
            super::MANAGED_POINT_QUERY,
        );
        for (label, value) in [
            (
                b"progress-interval".as_slice(),
                super::MANAGED_CATALOG_FENCE_PROGRESS_INTERVAL_V1 as u64,
            ),
            (
                b"progress-callback-limit".as_slice(),
                super::MANAGED_CATALOG_FENCE_PROGRESS_CALLBACK_LIMIT_V1,
            ),
            (
                b"assertion-limit".as_slice(),
                super::MANAGED_CATALOG_FENCE_ASSERTION_LIMIT_V1 as u64,
            ),
            (
                b"range-limit".as_slice(),
                super::MANAGED_CATALOG_FENCE_RANGE_LIMIT_V1 as u64,
            ),
            (
                b"point-limit".as_slice(),
                super::MANAGED_CATALOG_FENCE_POINT_LIMIT_V1 as u64,
            ),
            (
                b"expected-max".as_slice(),
                super::MANAGED_CATALOG_FENCE_EXPECTED_MAX_V1,
            ),
            (
                b"string-limit".as_slice(),
                super::MANAGED_CATALOG_FENCE_STRING_LIMIT_V1 as u64,
            ),
            (
                b"point-value-limit".as_slice(),
                super::MANAGED_CATALOG_FENCE_POINT_VALUE_LIMIT_V1 as u64,
            ),
            (
                b"transcript-byte-limit".as_slice(),
                super::MANAGED_CATALOG_FENCE_TRANSCRIPT_BYTE_LIMIT_V1,
            ),
            (
                b"main-file-byte-limit".as_slice(),
                super::MANAGED_PHYSICAL_MAX_FILE_BYTES,
            ),
            (
                b"encoded-key-byte-limit".as_slice(),
                super::MAX_ENCODED_KEY_BYTES as u64,
            ),
            (b"range-result-arity".as_slice(), 1),
            (b"range-bind-arity".as_slice(), 3),
            (b"point-result-arity".as_slice(), 5),
            (b"point-bind-arity".as_slice(), 1),
            (b"outcome-count-marker".as_slice(), 0),
            (b"outcome-pair-marker".as_slice(), 1),
            (b"outcome-one-of-marker".as_slice(), 2),
            (b"transcript-entry-marker".as_slice(), 1),
            (b"transcript-terminator".as_slice(), 0xff),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut fields, label, &value.to_be_bytes());
        }
        for (label, value) in [
            (
                b"relation-id-codec".as_slice(),
                super::STORED_RELATION_ID_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"memcmp-key-codec".as_slice(),
                super::STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"msgpack-exact-codec".as_slice(),
                super::STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"msgpack-row-profile".as_slice(),
                super::STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"row-datavalue-codec".as_slice(),
                super::STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (b"transient-bind".as_slice(), b"SQLITE_TRANSIENT".as_slice()),
            (
                b"range-upper".as_slice(),
                b"RelationId::next checked sentinel".as_slice(),
            ),
            (
                b"range-completion".as_slice(),
                b"exact DONE; stmtstatus SORT/FULLSCAN resetFlag=1".as_slice(),
            ),
            (
                b"point-completion".as_slice(),
                b"exact one ROW then DONE; reset clear finalize".as_slice(),
            ),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut fields, label, value);
        }
        update_policy_fingerprint_field(
            &mut hasher,
            &mut fields,
            b"transcript-domain",
            super::MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1,
        );
        update_policy_fingerprint_field(
            &mut hasher,
            &mut fields,
            b"policy-description",
            super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_DESCRIPTION_V1.as_bytes(),
        );
        hasher.update([0xff]);
        hasher.update(fields.to_be_bytes());
        hasher.finalize().into()
    }

    fn derive_closed_catalog_fence_policy_fingerprint_v1() -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut fields = 0_u64;
        for (label, value) in [
            (
                b"selector".as_slice(),
                b"mnestic.managed-catalog-fence.v1".as_slice(),
            ),
            (
                b"snapshot-policy".as_slice(),
                super::MANAGED_SNAPSHOT_POLICY_V1_IDENTITY_BYTES,
            ),
            (
                b"catalog-policy".as_slice(),
                super::MANAGED_CATALOG_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"source-policy".as_slice(),
                super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"assertion-policy".as_slice(),
                super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1.as_slice(),
            ),
            (
                b"policy-description".as_slice(),
                super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut fields, label, value);
        }
        hasher.update([0xff]);
        hasher.update(fields.to_be_bytes());
        hasher.finalize().into()
    }

    fn derive_closed_source_policy_fingerprint_v1() -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_DOMAIN_V1);
        let mut field_count = 0_u64;
        for (label, value) in [
            (
                b"identity-domain".as_slice(),
                super::MANAGED_CLOSED_SOURCE_IDENTITY_DOMAIN_V1,
            ),
            (
                b"managed-snapshot-policy-v1".as_slice(),
                super::MANAGED_SNAPSHOT_POLICY_V1_IDENTITY_BYTES,
            ),
            (
                b"policy-description".as_slice(),
                super::MANAGED_CLOSED_SOURCE_POLICY_DESCRIPTION_V1.as_bytes(),
            ),
        ] {
            update_policy_fingerprint_field(&mut hasher, &mut field_count, label, value);
        }
        hasher.update([0xff]);
        hasher.update(field_count.to_be_bytes());
        hasher.finalize().into()
    }

    fn create_schema_source(path: &Path, schema: &str) {
        let connection = Connection::open(path).unwrap();
        connection.execute(schema).unwrap();
        drop(connection);
        assert_no_sidecars(path);
    }

    fn create_wal_source(path: &Path, persist_sidecars: bool, truncate_wal: bool) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        if persist_sidecars {
            let mut enabled = 1_i32;
            let code = unsafe {
                ffi::sqlite3_file_control(
                    connection.as_raw(),
                    SQLITE_MAIN_SCHEMA.as_ptr().cast(),
                    ffi::SQLITE_FCNTL_PERSIST_WAL,
                    (&mut enabled as *mut i32).cast(),
                )
            };
            assert_eq!(code, ffi::SQLITE_OK);
        }
        connection
            .execute(
                "CREATE TABLE payload (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                 INSERT INTO payload (id, value) VALUES (1, 'copied');",
            )
            .unwrap();
        if truncate_wal {
            connection
                .execute("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
        }
        drop(connection);
    }

    fn create_committed_uncheckpointed_wal_source(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "PRAGMA journal_mode=WAL;\
                 PRAGMA wal_autocheckpoint=0;\
                 CREATE TABLE payload (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                 INSERT INTO payload (id, value) VALUES (1, 'copied');",
            )
            .unwrap();
        connection
    }

    fn create_sidecar_free_wal_header_source(path: &Path) {
        create_wal_source(path, true, true);
        let wal = sidecar_path(path, "-wal");
        let shm = sidecar_path(path, "-shm");
        assert!(wal.is_file());
        assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
        assert!(shm.is_file());

        // These are fixture-owned residuals created above. Production snapshot
        // code never removes a source sidecar.
        fs::remove_file(wal).unwrap();
        fs::remove_file(shm).unwrap();
        assert_no_sidecars(path);
    }

    fn create_hot_rollback_journal_source(path: &Path) -> Connection {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "PRAGMA journal_mode=DELETE;\
                 CREATE TABLE payload (id INTEGER PRIMARY KEY, value TEXT NOT NULL);\
                 INSERT INTO payload (id, value) VALUES (1, 'before');\
                 BEGIN IMMEDIATE;\
                 UPDATE payload SET value = 'uncommitted' WHERE id = 1;",
            )
            .unwrap();
        connection
    }

    fn assert_no_sidecars(path: &Path) {
        for suffix in SQLITE_SIDECAR_SUFFIXES {
            assert!(!sidecar_path(path, suffix).exists());
        }
    }

    fn assert_payload_copied(path: &Path) {
        assert_no_sidecars(path);
        let uri = super::immutable_source_uri(path).unwrap();
        let connection = Connection::open_with_flags(
            uri.to_str().unwrap(),
            OpenFlags::new().with_read_only().with_uri(),
        )
        .expect("copied database should open through the immutable read-only URI");
        let mut statement = connection
            .prepare("SELECT value FROM payload WHERE id = 1;")
            .unwrap();
        assert_eq!(statement.next().unwrap(), State::Row);
        assert_eq!(statement.read::<String, _>(0).unwrap(), "copied");
        assert_eq!(statement.next().unwrap(), State::Done);
        drop(statement);
        drop(connection);
        assert_no_sidecars(path);
    }

    fn assert_payload_visible_through_wal(path: &Path) {
        let connection = Connection::open_with_flags(path, OpenFlags::new().with_read_only())
            .expect("WAL fixture should open read-only with its real sidecars");
        let mut statement = connection
            .prepare("SELECT value FROM payload WHERE id = 1;")
            .unwrap();
        assert_eq!(statement.next().unwrap(), State::Row);
        assert_eq!(statement.read::<String, _>(0).unwrap(), "copied");
        assert_eq!(statement.next().unwrap(), State::Done);
    }

    fn assert_payload_absent_from_main_image(source: &Path, main_image: &Path) {
        fs::copy(source, main_image).unwrap();
        let uri = super::immutable_source_uri(main_image).unwrap();
        let connection = Connection::open_with_flags(
            uri.to_str().unwrap(),
            OpenFlags::new().with_read_only().with_uri(),
        )
        .unwrap();
        match connection.prepare("SELECT value FROM payload WHERE id = 1;") {
            Err(_) => {}
            Ok(mut statement) => assert_ne!(statement.next().unwrap(), State::Row),
        };
    }

    #[test]
    fn missing_source_is_not_created() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "missing.db");

        assert!(existing_open_permit(&source).is_err());
        assert!(!source.exists());
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn clean_wal_header_store_reopens_only_through_consumed_fence_permit() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "accepted-wal.db");
        create_clean_wal_header_managed_cozo_source(&source);

        let database =
            crate::DbInstance::open_existing_sqlite(existing_open_permit(&source).unwrap())
                .unwrap();
        let rows = database
            .run_default("?[id, value] := *catalog_00{id, value}")
            .unwrap();
        let schema = database.run_default("::columns catalog_00").unwrap();

        assert_eq!(rows.rows.len(), 3);
        assert!(schema.rows.iter().any(|row| {
            row.first()
                .is_some_and(|value| value == &DataValue::from("id"))
        }));
        assert!(schema.rows.iter().any(|row| {
            row.first()
                .is_some_and(|value| value == &DataValue::from("value"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_runtime_path_is_refused_without_family_or_lossy_alias_mutation() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let utf8_source = test_path(&directory, "utf8-source.db");
        create_clean_wal_header_managed_cozo_source(&utf8_source);

        let mut raw_name = b"runtime-".to_vec();
        raw_name.push(0x80);
        raw_name.extend_from_slice(b".db");
        let source = fs::canonicalize(directory.path())
            .unwrap()
            .join(std::ffi::OsString::from_vec(raw_name));
        #[cfg(target_os = "macos")]
        let before_rename = directory_byte_census(directory.path());
        if let Err(error) = fs::rename(&utf8_source, &source) {
            #[cfg(target_os = "macos")]
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(libc::EILSEQ)
            {
                assert_eq!(directory_byte_census(directory.path()), before_rename);
                eprintln!("filesystem refused the non-UTF-8 fixture without mutation; runtime-path coverage requires a filesystem accepting raw names");
                return;
            }
            panic!("failed to create the non-UTF-8 runtime source path: {error}");
        }
        let lossy_alias = test_path(&directory, "runtime-�.db");
        fs::write(&lossy_alias, b"lossy alias sentinel").unwrap();
        let source_before = fs::read(&source).unwrap();
        let family_before = directory_byte_census(directory.path());

        let permit = existing_open_permit(&source).unwrap();
        let error = match new_cozo_sqlite_existing(permit) {
            Ok(_) => panic!("sqlite 0.36 accepted a non-UTF-8 runtime path"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("UTF-8"));
        assert_eq!(fs::read(&source).unwrap(), source_before);
        assert_eq!(fs::read(&lossy_alias).unwrap(), b"lossy alias sentinel");
        assert_eq!(directory_byte_census(directory.path()), family_before);
    }

    #[cfg(unix)]
    #[test]
    fn existing_runtime_final_main_check_runs_after_initialization_failure() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "init-failure.db");
        create_clean_wal_header_managed_cozo_source(&source);
        let permit = existing_open_permit(&source).unwrap();
        super::MANAGED_EXISTING_RUNTIME_INIT_FAILURE.with(|failure| failure.set(true));
        super::MANAGED_EXISTING_RUNTIME_PREINIT_VERIFY_CALLS.with(|calls| calls.set(0));
        super::MANAGED_EXISTING_RUNTIME_FINAL_VERIFY_CALLS.with(|calls| calls.set(0));

        let error = match new_cozo_sqlite_existing(permit) {
            Ok(_) => panic!("injected runtime initialization failure unexpectedly passed"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("injected existing-only SQLite runtime initialization failure")
        );
        assert_eq!(
            super::MANAGED_EXISTING_RUNTIME_PREINIT_VERIFY_CALLS.with(|calls| calls.get()),
            1,
            "runtime initialization began without the immediate held-main verification"
        );
        assert_eq!(
            super::MANAGED_EXISTING_RUNTIME_FINAL_VERIFY_CALLS.with(|calls| calls.get()),
            1,
            "initialization failure skipped the final held-main verification"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_existing_only_reconnect_does_not_recreate_removed_main() {
        assert_existing_only_reconnect_does_not_recreate(false);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_existing_only_reconnect_does_not_recreate_removed_main() {
        assert_existing_only_reconnect_does_not_recreate(true);
    }

    #[cfg(unix)]
    fn assert_existing_only_reconnect_does_not_recreate(bounded: bool) {
        let directory = tempdir().unwrap();
        let source = test_path(
            &directory,
            if bounded {
                "bounded-reconnect.db"
            } else {
                "ordinary-reconnect.db"
            },
        );
        create_clean_wal_header_managed_cozo_source(&source);
        let database = new_cozo_sqlite_existing(existing_open_permit(&source).unwrap()).unwrap();
        database.db.pool_lock_for_tests().clear();
        fs::remove_file(&source).unwrap();
        let before = directory_byte_census(directory.path());

        if bounded {
            assert!(
                database
                    .db
                    .transact_read_with_deadline(Instant::now() + Duration::from_secs(1))
                    .is_err()
            );
        } else {
            assert!(database.db.transact(false).is_err());
        }

        assert!(!source.exists());
        assert_eq!(directory_byte_census(directory.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn wal_headerless_cozo_is_refused_before_permit_without_family_mutation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "wal-headerless.db");
        create_sidecar_free_wal_headerless_cozo_source(&source);
        let main_before = fs::read(&source).unwrap();
        let family_before = directory_byte_census(directory.path());

        assert!(existing_open_permit(&source).is_err());

        assert_eq!(fs::read(&source).unwrap(), main_before);
        assert_eq!(directory_byte_census(directory.path()), family_before);
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_source_is_rejected_without_mutation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let alias = test_path(&directory, "alias.db");
        create_source(&source);
        fs::hard_link(&source, &alias).unwrap();
        let source_before = fingerprint(&source);
        let alias_before = fingerprint(&alias);

        let error = ExistingSqliteSnapshotSource::open(&source)
            .err()
            .expect("hardlinked source must be refused");
        assert!(error.to_string().contains("exactly one link"));

        assert_eq!(fingerprint(&source), source_before);
        assert_eq!(fingerprint(&alias), alias_before);
        assert_no_sidecars(&source);
        assert_no_sidecars(&alias);
    }

    #[cfg(unix)]
    #[test]
    fn renamed_source_is_detected_before_destination_creation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let moved = test_path(&directory, "moved.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        fs::rename(&source, &moved).unwrap();
        let moved_before = fingerprint(&moved);

        assert!(snapshot.backup_to_new(&destination).is_err());
        assert_eq!(fingerprint(&moved), moved_before);
        assert!(!source.exists());
        assert!(!destination.exists());
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn renamed_and_replaced_source_is_detected_before_destination_creation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let moved = test_path(&directory, "moved.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        fs::rename(&source, &moved).unwrap();
        create_source(&source);
        let moved_before = fingerprint(&moved);
        let replacement_before = fingerprint(&source);

        assert!(snapshot.backup_to_new(&destination).is_err());
        assert_eq!(fingerprint(&moved), moved_before);
        assert_eq!(fingerprint(&source), replacement_before);
        assert!(!destination.exists());
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn in_place_source_drift_is_detected_before_destination_creation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"drift")
            .unwrap();
        let drifted = fingerprint(&source);

        assert!(snapshot.backup_to_new(&destination).is_err());
        assert_eq!(fingerprint(&source), drifted);
        assert!(!destination.exists());
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn post_backup_source_drift_fails_and_removes_unpublished_destination() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let result = snapshot.backup_to_new_with_after_native_backup_hook(
            &destination,
            || -> miette::Result<()> {
                OpenOptions::new()
                    .append(true)
                    .open(&source)
                    .map_err(|error| miette::miette!("cannot inject source drift: {error}"))?
                    .write_all(b"post-backup drift")
                    .map_err(|error| miette::miette!("cannot inject source drift: {error}"))?;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!destination.exists());
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn destination_setup_failure_before_identity_is_preserved_and_reported() {
        let directory = tempdir().unwrap();
        let destination = test_path(&directory, "destination.db");

        let error = IncompleteDestination::create_failing_before_identity(&destination)
            .expect_err("injected setup failure must fail");

        let message = error.to_string();
        assert!(message.contains("injected destination setup failure before identity binding"));
        assert!(message.contains("identity was never bound"));
        assert!(message.contains("preserv"));
        assert!(destination.is_file());
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn destination_setup_failure_after_identity_leaves_no_residue() {
        let directory = tempdir().unwrap();
        let destination = test_path(&directory, "destination.db");

        let error = IncompleteDestination::create_failing_after_identity(&destination)
            .expect_err("injected setup failure must fail");

        assert!(
            error
                .to_string()
                .contains("injected destination setup failure after identity binding")
        );
        assert!(!destination.exists());
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn renamed_destination_foreign_replacement_and_sidecar_are_preserved() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        let moved_owned = test_path(&directory, "moved-owned.db");
        let foreign_sidecar = sidecar_path(&destination, "-wal");
        let foreign_main_bytes = b"foreign destination sentinel";
        let foreign_sidecar_bytes = b"foreign sidecar sentinel";
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let result = snapshot.backup_to_new_with_after_native_backup_hook(
            &destination,
            || -> miette::Result<()> {
                fs::rename(&destination, &moved_owned).map_err(|error| {
                    miette::miette!("cannot move owned destination in hook: {error}")
                })?;
                fs::write(&destination, foreign_main_bytes).map_err(|error| {
                    miette::miette!("cannot create foreign destination in hook: {error}")
                })?;
                fs::write(&foreign_sidecar, foreign_sidecar_bytes).map_err(|error| {
                    miette::miette!("cannot create foreign sidecar in hook: {error}")
                })?;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&destination).unwrap(), foreign_main_bytes);
        assert_eq!(fs::read(&foreign_sidecar).unwrap(), foreign_sidecar_bytes);
        assert_payload_copied(&moved_owned);
    }

    #[cfg(unix)]
    #[test]
    fn hardlinked_destination_is_preserved_on_failed_publication() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        let alias = test_path(&directory, "destination-alias.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let result = snapshot.backup_to_new_with_after_native_backup_hook(
            &destination,
            || -> miette::Result<()> {
                fs::hard_link(&destination, &alias).map_err(|error| {
                    miette::miette!("cannot hardlink destination in hook: {error}")
                })?;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::metadata(&destination).unwrap().nlink(), 2);
        assert_eq!(fs::read(&destination).unwrap(), fs::read(&alias).unwrap());
        assert_payload_copied(&destination);
        assert_no_sidecars(&alias);
    }

    #[cfg(unix)]
    #[test]
    fn foreign_destination_sidecar_is_preserved_while_owned_main_is_removed() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        let foreign_sidecar = sidecar_path(&destination, "-wal");
        let sentinel = b"foreign sidecar sentinel";
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let result = snapshot.backup_to_new_with_after_native_backup_hook(
            &destination,
            || -> miette::Result<()> {
                fs::write(&foreign_sidecar, sentinel).map_err(|error| {
                    miette::miette!("cannot create foreign sidecar in hook: {error}")
                })?;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(fs::read(&foreign_sidecar).unwrap(), sentinel);
        assert!(!sidecar_path(&destination, "-shm").exists());
        assert!(!sidecar_path(&destination, "-journal").exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_cleanup_is_one_shot_and_drop_does_not_retry() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempdir().unwrap();
        let destination = test_path(&directory, "destination.db");
        let moved_owned = test_path(&directory, "moved-owned.db");
        let foreign_sidecar = sidecar_path(&destination, "-journal");
        let sentinel = b"foreign destination sentinel";
        let sidecar_sentinel = b"foreign sidecar sentinel";
        let mut created = IncompleteDestination::create(&destination).unwrap();
        let owned_metadata = fs::metadata(&destination).unwrap();

        fs::rename(&destination, &moved_owned).unwrap();
        fs::write(&destination, sentinel).unwrap();
        fs::write(&foreign_sidecar, sidecar_sentinel).unwrap();
        let error = created
            .cleanup()
            .expect_err("cleanup must refuse a replaced destination");
        assert!(error.to_string().contains("preserved"));
        assert_eq!(fs::read(&destination).unwrap(), sentinel);
        assert_eq!(fs::read(&foreign_sidecar).unwrap(), sidecar_sentinel);

        // Restore the owned inode after the failed attempt. Drop must not make
        // a second path-based cleanup attempt against this new pathname state.
        fs::remove_file(&destination).unwrap();
        fs::rename(&moved_owned, &destination).unwrap();
        drop(created);

        let restored_metadata = fs::metadata(&destination).unwrap();
        assert_eq!(restored_metadata.dev(), owned_metadata.dev());
        assert_eq!(restored_metadata.ino(), owned_metadata.ino());
        assert_eq!(fs::read(&foreign_sidecar).unwrap(), sidecar_sentinel);
    }

    #[test]
    fn linked_sqlite_runtime_meets_snapshot_floor() {
        let linked = unsafe { ffi::sqlite3_libversion_number() };
        assert!(linked >= MIN_SNAPSHOT_SQLITE_VERSION);
        ensure_snapshot_sqlite_runtime().unwrap();
    }

    #[test]
    fn managed_reader_accepts_genuine_cozo_without_mutating_source() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed.db");
        create_managed_cozo_source(&source);
        let before = fingerprint(&source);

        let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();
        let reader = guarded
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.close_and_verify().unwrap();

        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_primary_index_catalog_is_pinned_borrowed_and_cached() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-catalog.db");
        create_populated_managed_cozo_source(&source);
        let before = fingerprint(&source);

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let first = reader
            .inspect_primary_index_catalog_v1()
            .expect("genuine catalog must be syntactically visible");
        assert_eq!(first.catalog().len(), super::MANAGED_CATALOG_RELATION_COUNT);
        assert_eq!(first.relation_counter(), 26);
        assert_eq!(first.storage_version(), 0);
        assert_ne!(first.raw_commitment(), first.canonical_commitment());
        assert_eq!(
            first.policy_fingerprint(),
            &MANAGED_CATALOG_POLICY_FINGERPRINT_V1
        );
        let first_address = std::ptr::from_ref(first);
        assert!(reader.transaction_active);
        assert_eq!(
            unsafe { ffi::sqlite3_get_autocommit(reader.connection.as_ref().unwrap().as_raw()) },
            0
        );

        let second_address = std::ptr::from_ref(
            reader
                .inspect_primary_index_catalog_v1()
                .expect("repeated observation must use the cache"),
        );
        assert_eq!(first_address, second_address);
        reader.close_and_verify().unwrap();
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_physical_census_accepts_genuine_populated_store_and_caches_composite() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-physical.db");
        create_populated_managed_cozo_source(&source);
        let before = fingerprint(&source);

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let (catalog_address, physical_address) = {
            let observation = reader.inspect_physical_census_v1().unwrap();
            assert_eq!(
                observation.catalog().catalog().len(),
                super::MANAGED_CATALOG_RELATION_COUNT
            );
            let physical = observation.physical();
            assert_eq!(physical.table_row_count(), 31);
            assert_eq!(physical.index_row_count(), 31);
            assert!(physical.main_file_bytes() > 0);
            assert!(physical.main_page_count() > 0);
            assert!(
                physical.progress_callbacks_used()
                    < super::MANAGED_PHYSICAL_PROGRESS_CALLBACK_LIMIT
            );
            assert_ne!(physical.table_ordered_commitment(), &[0_u8; 32]);
            assert_ne!(physical.index_ordered_commitment(), &[0_u8; 32]);
            assert_ne!(
                physical.table_ordered_commitment(),
                physical.index_ordered_commitment()
            );
            assert_eq!(
                physical.policy_fingerprint(),
                &super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1
            );
            assert_eq!(physical.linked_sqlite_version_number(), unsafe {
                ffi::sqlite3_libversion_number()
            });
            assert_eq!(
                physical.linked_sqlite_version(),
                unsafe { std::ffi::CStr::from_ptr(ffi::sqlite3_libversion()) }.to_bytes()
            );
            assert_eq!(
                physical.linked_sqlite_source_id(),
                unsafe { std::ffi::CStr::from_ptr(ffi::sqlite3_sourceid()) }.to_bytes()
            );
            assert_ne!(
                physical.linked_sqlite_runtime_identity_fingerprint(),
                &[0_u8; 32]
            );
            assert_eq!(
                physical.stored_relation_id_policy_fingerprint(),
                &super::STORED_RELATION_ID_POLICY_FINGERPRINT_V1
            );
            assert_eq!(
                physical.stored_memcmp_key_policy_fingerprint(),
                &super::STORED_MEMCMP_KEY_POLICY_FINGERPRINT_V1
            );
            assert_eq!(
                physical.stored_msgpack_exact_codec_policy_fingerprint(),
                &super::STORED_MSGPACK_EXACT_CODEC_POLICY_FINGERPRINT_V1
            );
            assert_eq!(
                physical.stored_msgpack_row_policy_fingerprint(),
                &super::STORED_MSGPACK_ROW_POLICY_FINGERPRINT_V1
            );
            assert_eq!(
                physical.stored_msgpack_relation_catalog_policy_fingerprint(),
                &super::STORED_MSGPACK_RELATION_CATALOG_POLICY_FINGERPRINT_V1
            );
            assert_eq!(
                physical.stored_row_datavalue_codec_policy_fingerprint(),
                &super::STORED_ROW_DATAVALUE_CODEC_POLICY_FINGERPRINT_V1
            );
            let counts: BTreeMap<_, _> = physical
                .relation_row_counts()
                .iter()
                .map(|entry| (entry.relation_id(), entry.row_count()))
                .collect();
            assert_eq!(counts.get(&0), Some(&28));
            let populated_id = observation.catalog().catalog().entries()[0].relation().id();
            assert_eq!(counts.get(&populated_id), Some(&3));
            (
                std::ptr::from_ref(observation.catalog()),
                std::ptr::from_ref(physical),
            )
        };
        let repeated = reader.inspect_physical_census_v1().unwrap();
        assert_eq!(std::ptr::from_ref(repeated.catalog()), catalog_address);
        assert_eq!(std::ptr::from_ref(repeated.physical()), physical_address);
        assert_eq!(
            unsafe {
                ffi::sqlite3_txn_state(
                    reader.connection.as_ref().unwrap().as_raw(),
                    SQLITE_MAIN_SCHEMA.as_ptr().cast(),
                )
            },
            ffi::SQLITE_TXN_READ
        );
        reader.close_and_verify().unwrap();
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_physical_census_budget_interrupts_poisons_and_unregisters_callback() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-budget.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();

        let error =
            match reader.inspect_physical_census_with_budget_v1(super::ManagedProgressBudget {
                progress_interval: 1,
                max_callbacks: 1,
                phase: super::ManagedProgressPhase::Census,
            }) {
                Ok(_) => panic!("one progress callback did not interrupt the physical pass"),
                Err(error) => error,
            };
        assert!(error.to_string().contains("work budget"));
        assert!(reader.inspect_physical_census_v1().is_err());
        // A normal fixed statement would immediately inherit SQLITE_INTERRUPT
        // here if the stack-backed callback had remained registered.
        super::managed_integer_query(
            reader.connection.as_ref().unwrap(),
            super::MANAGED_PAGE_COUNT_QUERY,
            "post-interrupt callback cleanup probe",
        )
        .unwrap();
        assert!(reader.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_catalog_callbacks_are_charged_to_the_same_physical_budget() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-shared-budget.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();

        reader
            .inspect_primary_index_catalog_with_budget_v1(super::ManagedProgressBudget {
                progress_interval: 1,
                max_callbacks: 1 << 20,
                phase: super::ManagedProgressPhase::Census,
            })
            .unwrap();
        let catalog_callbacks = reader.census_progress_callbacks_used;
        assert!(catalog_callbacks > 0);

        let error =
            match reader.inspect_physical_census_with_budget_v1(super::ManagedProgressBudget {
                progress_interval: 1,
                max_callbacks: catalog_callbacks + 1,
                phase: super::ManagedProgressPhase::Census,
            }) {
                Ok(_) => panic!("physical work received a fresh budget after catalog inspection"),
                Err(error) => error,
            };
        assert!(error.to_string().contains("work budget"));
        super::managed_integer_query(
            reader.connection.as_ref().unwrap(),
            super::MANAGED_PAGE_COUNT_QUERY,
            "post-shared-budget callback cleanup probe",
        )
        .unwrap();
        assert!(reader.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_physical_census_caught_panic_never_yields_success_or_dangles_callback() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-panic.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        super::MANAGED_PHYSICAL_TEST_PANIC_PHASE.with(|phase| phase.set(2));
        let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = reader.inspect_physical_census_v1();
        }));
        assert!(caught.is_err());
        super::managed_integer_query(
            reader.connection.as_ref().unwrap(),
            super::MANAGED_PAGE_COUNT_QUERY,
            "post-panic callback cleanup probe",
        )
        .unwrap();
        assert!(reader.inspect_physical_census_v1().is_err());
        assert!(reader.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_physical_census_rejects_lost_transaction_and_poison_sticks() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-lost-transaction.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.inspect_primary_index_catalog_v1().unwrap();
        super::managed_exec_fixed(
            reader.connection.as_ref().unwrap(),
            MANAGED_ROLLBACK_READ_SQL,
            "test rollback",
        )
        .unwrap();
        let error = match reader.inspect_physical_census_v1() {
            Ok(_) => panic!("lost pinned transaction passed the physical census"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("pinned READ transaction"));
        assert!(reader.inspect_physical_census_v1().is_err());
        assert!(reader.close_and_verify().is_err());
    }

    #[test]
    fn managed_physical_census_rechecks_connection_shape_inside_transaction() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-inside-tx-recheck.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let connection = reader.connection.as_ref().unwrap();
        unsafe {
            ffi::sqlite3_limit(connection.as_raw(), ffi::SQLITE_LIMIT_ATTACHED, 1);
        }
        super::managed_exec_fixed(
            connection,
            super::MANAGED_TEST_QUERY_ONLY_OFF_SQL,
            "test query-only override",
        )
        .unwrap();
        let attach = unsafe {
            ffi::sqlite3_exec(
                connection.as_raw(),
                super::MANAGED_TEST_ATTACH_SQL.as_ptr().cast(),
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(attach, ffi::SQLITE_OK);
        super::managed_exec_fixed(connection, MANAGED_QUERY_ONLY_SQL, "restored query-only")
            .unwrap();

        let error = match reader.inspect_physical_census_v1() {
            Ok(_) => panic!("inside-transaction attachment passed the recheck"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("main-only attachment set"));
        assert!(reader.close_and_verify().is_err());
    }

    #[test]
    fn managed_physical_caps_and_page_geometry_are_exact() {
        super::managed_admit_file_bytes(super::MANAGED_PHYSICAL_MAX_FILE_BYTES).unwrap();
        assert!(
            super::managed_admit_file_bytes(super::MANAGED_PHYSICAL_MAX_FILE_BYTES + 1).is_err()
        );
        super::managed_validate_page_geometry(
            super::MANAGED_PHYSICAL_MAX_PAGE_COUNT * 4_096,
            super::MANAGED_PHYSICAL_MAX_PAGE_COUNT,
            4_096,
        )
        .unwrap();
        assert!(
            super::managed_validate_page_geometry(
                (super::MANAGED_PHYSICAL_MAX_PAGE_COUNT + 1) * 512,
                super::MANAGED_PHYSICAL_MAX_PAGE_COUNT + 1,
                512,
            )
            .is_err()
        );
        assert!(super::managed_validate_page_geometry(4_096, 1, 1_024).is_err());
        assert!(super::managed_validate_page_geometry(4_097, 1, 4_096).is_err());
        assert_eq!(super::MANAGED_PHYSICAL_MAX_FILE_BYTES, 1_u64 << 40);
        assert_eq!(super::MANAGED_PHYSICAL_MAX_PAGE_COUNT, 1_u64 << 24);
        assert_eq!(super::MANAGED_PHYSICAL_CACHE_SIZE_KIB, 65_536);
    }

    #[test]
    fn managed_integrity_result_requires_exact_one_text_ok_then_done() {
        let connection = Connection::open(":memory:").unwrap();
        let accepted = prepare_test_statement(&connection, b"SELECT CAST('ok' AS TEXT);\0");
        super::managed_accept_exact_integrity_result(&accepted).unwrap();
        drop(accepted);

        for sql in [
            b"SELECT x'6f6b';\0".as_slice(),
            b"SELECT 'no';\0".as_slice(),
            b"SELECT 'not ok';\0".as_slice(),
            b"SELECT 'ok' UNION ALL SELECT 'ok';\0".as_slice(),
        ] {
            let rejected = prepare_test_statement(&connection, sql);
            let message = super::managed_accept_exact_integrity_result(&rejected)
                .expect_err("non-exact integrity envelope must reject")
                .to_string();
            assert!(message.contains("integrity check"));
            assert!(message.len() < 192);
        }
    }

    #[test]
    fn managed_physical_census_rejects_a_real_missing_primary_index_cell() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-corrupt-index.db");
        create_populated_managed_cozo_source(&source);
        if !corrupt_primary_index_by_imposter_delete(&source) {
            return;
        }

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader
            .inspect_primary_index_catalog_v1()
            .expect("positive-row index damage must leave G2a's id-zero view intact");
        let error = match reader.inspect_physical_census_v1() {
            Ok(_) => panic!("a missing real primary-index cell passed the physical census"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("integrity check"));
        assert!(reader.inspect_physical_census_v1().is_err());
        assert!(reader.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_point_statement_reuses_and_detects_zero_or_two_rows_with_cleanup() {
        let connection = Connection::open(":memory:").unwrap();
        connection
            .execute("CREATE TABLE dupe (k BLOB NOT NULL, v BLOB NOT NULL);")
            .unwrap();
        let statement = prepare_test_statement(
            &connection,
            b"SELECT rowid, length(k), length(v), k, v FROM dupe WHERE k = ?1;\0",
        );
        let missing = super::managed_verify_point_image(&statement, 1, b"k", b"v")
            .expect_err("zero-row point result must reject");
        assert!(missing.to_string().contains("no row"));
        assert_eq!(unsafe { ffi::sqlite3_stmt_busy(statement.raw) }, 0);

        connection
            .execute("INSERT INTO dupe VALUES (x'6b',x'76'),(x'6b',x'76');")
            .unwrap();
        let duplicate = super::managed_verify_point_image(&statement, 1, b"k", b"v")
            .expect_err("two-row point result must reject");
        assert!(duplicate.to_string().contains("multiple rows"));
        assert_eq!(unsafe { ffi::sqlite3_stmt_busy(statement.raw) }, 0);

        connection
            .execute("DELETE FROM dupe WHERE rowid = 2;")
            .unwrap();
        super::managed_verify_point_image(&statement, 1, b"k", b"v").unwrap();
        assert_eq!(unsafe { ffi::sqlite3_stmt_busy(statement.raw) }, 0);
    }

    #[test]
    fn managed_assertion_point_visitor_and_string_decoder_fail_closed() {
        let relation = RelationId::new(42);
        let key = vec![DataValue::from("key")].encode_as_key(relation);
        let value_blob = |values: Vec<DataValue>| {
            let mut bytes = relation.raw_encode().to_vec();
            bytes.extend(rmp_serde::to_vec(&values).unwrap());
            bytes
        };
        let valid = value_blob(vec![DataValue::from("value")]);
        assert_eq!(
            super::managed_select_assertion_string_pair(&key, &valid, "key", &["other", "value"])
                .unwrap(),
            1
        );
        assert!(
            super::managed_select_assertion_string_pair(&key, &valid, "wrong", &["value"])
                .unwrap_err()
                .to_string()
                .contains("wrong key")
        );
        let wrong_type = value_blob(vec![DataValue::from(7_i64)]);
        assert!(
            super::managed_select_assertion_string_pair(&key, &wrong_type, "key", &["value"])
                .unwrap_err()
                .to_string()
                .contains("wrong tuple shape")
        );
        let extra = value_blob(vec![DataValue::from("value"), DataValue::from("extra")]);
        assert!(
            super::managed_select_assertion_string_pair(&key, &extra, "key", &["value"])
                .unwrap_err()
                .to_string()
                .contains("wrong tuple shape")
        );
        let mut trailing = valid.clone();
        trailing.push(0xc0);
        assert!(
            super::managed_select_assertion_string_pair(&key, &trailing, "key", &["value"])
                .unwrap_err()
                .to_string()
                .contains("exact decoder")
        );
        let mut malformed = relation.raw_encode().to_vec();
        malformed.push(0xc1);
        assert!(
            super::managed_select_assertion_string_pair(&key, &malformed, "key", &["value"])
                .unwrap_err()
                .to_string()
                .contains("exact decoder")
        );
        assert!(
            super::managed_select_assertion_string_pair(&key, &valid, "key", &[])
                .unwrap_err()
                .to_string()
                .contains("alternatives were invalid")
        );

        let connection = Connection::open(":memory:").unwrap();
        connection
            .execute(
                "CREATE TABLE hostile_point (k BLOB NOT NULL, v); \
                 INSERT INTO hostile_point VALUES (x'6b', 'ATTACKER_TEXT');",
            )
            .unwrap();
        let statement = prepare_test_statement(
            &connection,
            b"SELECT rowid,length(k),length(v),k,v FROM hostile_point WHERE k=?1;\0",
        );
        super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.set(0));
        let error = super::managed_visit_forced_primary_index_point(
            &statement,
            b"k",
            None,
            super::ManagedPointValueLengthV1::AtMost(1_024),
            |_, _| Ok(()),
        )
        .expect_err("non-BLOB point value passed");
        assert!(error.to_string().contains("invalid cell types"));
        assert!(!error.to_string().contains("ATTACKER_TEXT"));
        assert_eq!(
            super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.get()),
            0
        );
        assert_eq!(unsafe { ffi::sqlite3_stmt_busy(statement.raw) }, 0);

        connection.execute("DELETE FROM hostile_point;").unwrap();
        connection
            .execute("INSERT INTO hostile_point VALUES (x'6b', zeroblob(1025));")
            .unwrap();
        super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.set(0));
        let error = super::managed_visit_forced_primary_index_point(
            &statement,
            b"k",
            None,
            super::ManagedPointValueLengthV1::AtMost(1_024),
            |_, _| Ok(()),
        )
        .expect_err("oversized point value passed");
        assert!(
            error
                .to_string()
                .contains("value exceeded its byte boundary")
        );
        assert_eq!(
            super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.get()),
            0
        );
        assert_eq!(unsafe { ffi::sqlite3_stmt_busy(statement.raw) }, 0);

        connection.execute("DELETE FROM hostile_point;").unwrap();
        connection
            .execute("INSERT INTO hostile_point VALUES (x'6b', x'76');")
            .unwrap();
        let wrong_key = prepare_test_statement(
            &connection,
            b"SELECT rowid,length(k),length(v),k,v FROM hostile_point WHERE ?1 IS NOT NULL;\0",
        );
        super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.set(0));
        let error = super::managed_visit_forced_primary_index_point(
            &wrong_key,
            b"x",
            None,
            super::ManagedPointValueLengthV1::AtMost(1_024),
            |_, _| Ok(()),
        )
        .expect_err("wrong returned key passed");
        assert!(error.to_string().contains("differed from its table row"));
        assert_eq!(
            super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.get()),
            1,
            "wrong key must reject before the value pointer"
        );
        assert_eq!(unsafe { ffi::sqlite3_stmt_busy(wrong_key.raw) }, 0);

        let no_parameter = prepare_test_statement(
            &connection,
            b"SELECT 1,1,1,CAST(x'6b' AS BLOB),CAST(x'76' AS BLOB);\0",
        );
        let error = super::managed_visit_forced_primary_index_point(
            &no_parameter,
            b"k",
            None,
            super::ManagedPointValueLengthV1::AtMost(1_024),
            |_, _| Ok(()),
        )
        .expect_err("bind failure passed");
        assert!(error.to_string().contains("binding failed"));
        assert_eq!(unsafe { ffi::sqlite3_stmt_busy(no_parameter.raw) }, 0);
    }

    #[test]
    fn managed_relation_assertions_succeed_from_every_prior_phase_with_both_one_of_outcomes() {
        for initial_phase in ["fresh", "catalog-observed", "physically-audited"] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("assertions-{initial_phase}.db"));
            create_relation_assertion_source(&source);
            let before = fingerprint(&source);
            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            match initial_phase {
                "catalog-observed" => {
                    reader.inspect_primary_index_catalog_v1().unwrap();
                }
                "physically-audited" => {
                    reader.inspect_physical_census_v1().unwrap();
                }
                "fresh" => {}
                _ => unreachable!(),
            }
            let cached_catalog_address = reader.catalog.as_ref().map(std::ptr::from_ref);
            let census_callbacks = reader.census_progress_callbacks_used;
            super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.set(0));
            super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.set(0));
            let maximum_key: &'static str = Box::leak(
                "x".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT)
                    .into_boxed_str(),
            );
            let maximum_value: &'static str = Box::leak(
                "y".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT)
                    .into_boxed_str(),
            );
            let maximum_other: &'static str = Box::leak(
                "z".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT)
                    .into_boxed_str(),
            );
            let maximum_tag: &'static str = Box::leak(
                "t".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT)
                    .into_boxed_str(),
            );

            let evidence = reader
                .run_relation_assertions_v1(|planner| {
                    let catalog: &super::ManagedSqlitePrimaryIndexCatalogV1 = planner.catalog();
                    let physical: &super::ManagedSqlitePhysicalCensusV1 = planner.physical();
                    assert_eq!(
                        catalog.catalog().len(),
                        super::MANAGED_CATALOG_RELATION_COUNT
                    );
                    assert_eq!(physical.relation_row_counts().len(), 27);
                    let relation_id = assertion_relation_id(planner, "assertion_00");
                    planner.require_exact_row_count(relation_id, 8)?;
                    planner.require_string_pair(relation_id, "exact", "value")?;
                    planner.require_string_pair_one_of(
                        "choice-zero-result",
                        relation_id,
                        "choice-zero",
                        ["old", "new"],
                    )?;
                    planner.require_string_pair_one_of(
                        "choice-one-result",
                        relation_id,
                        "choice-one",
                        ["old", "new"],
                    )?;
                    planner.require_string_pair_one_of(
                        maximum_tag,
                        relation_id,
                        maximum_key,
                        [maximum_other, maximum_value],
                    )?;
                    Ok(())
                })
                .unwrap();

            assert_eq!(evidence.assertion_count, 5);
            assert_eq!(evidence.point_read_count, 4);
            assert_eq!(evidence.outcomes.len(), 5);
            assert_eq!(
                evidence.policy_fingerprint,
                super::MANAGED_ASSERTION_POLICY_FINGERPRINT_V1
            );
            assert_ne!(evidence.transcript.commitment, [0_u8; 32]);
            match &evidence.outcomes[0].kind {
                super::ManagedRelationAssertionOutcomeKindV1::ExactRowCount {
                    expected, ..
                } => assert_eq!(*expected, 8),
                _ => panic!("first assertion outcome had the wrong kind"),
            }
            match &evidence.outcomes[1].kind {
                super::ManagedRelationAssertionOutcomeKindV1::StringPair { key, value, .. } => {
                    assert_eq!((*key, *value), ("exact", "value"));
                }
                _ => panic!("second assertion outcome had the wrong kind"),
            }
            for (index, expected_tag, expected_selected) in [
                (2_usize, "choice-zero-result", 0_u8),
                (3_usize, "choice-one-result", 1_u8),
            ] {
                match &evidence.outcomes[index].kind {
                    super::ManagedRelationAssertionOutcomeKindV1::StringPairOneOf {
                        outcome_tag,
                        alternatives,
                        selected_index,
                        ..
                    } => {
                        assert_eq!(*outcome_tag, expected_tag);
                        assert_eq!(*alternatives, ["old", "new"]);
                        assert_eq!(*selected_index, expected_selected);
                    }
                    _ => panic!("one-of assertion outcome had the wrong kind"),
                }
            }
            assert_eq!(
                super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.get()),
                1
            );
            assert_eq!(
                super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.get()),
                1
            );
            if initial_phase == "physically-audited" {
                assert_eq!(reader.census_progress_callbacks_used, census_callbacks);
            } else if initial_phase == "fresh" {
                assert_eq!(census_callbacks, 0);
                assert_eq!(
                    reader.census_progress_callbacks_used,
                    reader
                        .physical_census
                        .as_ref()
                        .unwrap()
                        .progress_callbacks_used()
                );
            }
            if let Some(address) = cached_catalog_address {
                assert_eq!(
                    reader.catalog.as_ref().map(std::ptr::from_ref),
                    Some(address),
                    "the cached catalog was rescanned or replaced"
                );
            }
            assert!(reader.state == super::ManagedReaderState::Audited);
            assert_eq!(
                unsafe {
                    ffi::sqlite3_txn_state(
                        reader.connection.as_ref().unwrap().as_raw(),
                        SQLITE_MAIN_SCHEMA.as_ptr().cast(),
                    )
                },
                ffi::SQLITE_TXN_READ
            );
            assert_eq!(
                prepared_statement_count(reader.connection.as_ref().unwrap()),
                0
            );
            reader.close_and_verify().unwrap();
            assert_eq!(fingerprint(&source), before);
            assert_no_sidecars(&source);
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_closed_audit_succeeds_from_every_prior_phase_and_exposes_only_exact_outcomes() {
        for initial_phase in ["fresh", "catalog-observed", "physically-audited"] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("closed-audit-{initial_phase}.db"));
            create_relation_assertion_source(&source);
            let before = fingerprint(&source);
            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            match initial_phase {
                "catalog-observed" => {
                    reader.inspect_primary_index_catalog_v1().unwrap();
                }
                "physically-audited" => {
                    reader.inspect_physical_census_v1().unwrap();
                }
                "fresh" => {}
                _ => unreachable!(),
            }
            let prior_callbacks = reader
                .physical_census
                .as_ref()
                .map(|physical| physical.progress_callbacks_used());

            let audit = reader
                .close_with_relation_assertions_v1(|planner| {
                    let relation_id = assertion_relation_id(planner, "assertion_00");
                    planner.require_exact_row_count(relation_id, 8)?;
                    planner.require_string_pair(relation_id, "exact", "value")?;
                    planner.require_string_pair_one_of(
                        "choice-zero-result",
                        relation_id,
                        "choice-zero",
                        ["old", "new"],
                    )?;
                    planner.require_string_pair_one_of(
                        "choice-one-result",
                        relation_id,
                        "choice-one",
                        ["old", "new"],
                    )?;
                    Ok(())
                })
                .unwrap();

            assert_eq!(audit.policy(), ManagedSnapshotPolicy::V1);
            assert_eq!(
                audit.catalog().catalog().len(),
                MANAGED_CATALOG_RELATION_COUNT
            );
            assert_eq!(audit.physical().relation_row_counts().len(), 27);
            if let Some(callbacks) = prior_callbacks {
                assert_eq!(audit.physical().progress_callbacks_used(), callbacks);
            }
            let relation_id = audit
                .catalog()
                .catalog()
                .entries()
                .iter()
                .find(|entry| entry.relation().name() == "assertion_00")
                .unwrap()
                .relation()
                .id();
            let assertions = audit.assertions();
            assert_eq!(assertions.assertion_count(), 4);
            assert_eq!(assertions.point_read_count(), 3);
            assert_eq!(assertions.outcomes().len(), 4);
            assert_ne!(assertions.transcript_commitment(), &[0_u8; 32]);
            assert_eq!(
                assertions.transcript_commitment(),
                assertions.transcript().commitment()
            );
            assert_eq!(
                assertions.policy_fingerprint(),
                &super::MANAGED_ASSERTION_POLICY_FINGERPRINT_V1
            );
            assert!(assertions.outcomes()[0].matches_exact_row_count(relation_id, 8));
            assert!(!assertions.outcomes()[0].matches_exact_row_count(relation_id, 7));
            assert!(!assertions.outcomes()[0].matches_exact_row_count(relation_id + 1, 8));
            assert!(assertions.outcomes()[1].matches_string_pair(relation_id, "exact", "value"));
            assert!(!assertions.outcomes()[1].matches_string_pair(
                relation_id + 1,
                "exact",
                "value"
            ));
            assert!(!assertions.outcomes()[1].matches_string_pair(
                relation_id,
                "wrong-key",
                "value"
            ));
            assert!(!assertions.outcomes()[1].matches_string_pair(relation_id, "exact", "other"));
            assert_eq!(
                assertions.outcomes()[2].selected_index_if_string_pair_one_of(
                    "choice-zero-result",
                    relation_id,
                    "choice-zero",
                    ["old", "new"],
                ),
                Some(0)
            );
            assert_eq!(
                assertions.outcomes()[2].selected_index_if_string_pair_one_of(
                    "choice-zero-result",
                    relation_id + 1,
                    "choice-zero",
                    ["old", "new"],
                ),
                None
            );
            assert_eq!(
                assertions.outcomes()[2].selected_index_if_string_pair_one_of(
                    "choice-zero-result",
                    relation_id,
                    "wrong-key",
                    ["old", "new"],
                ),
                None
            );
            assert_eq!(
                assertions.outcomes()[3].selected_index_if_string_pair_one_of(
                    "choice-one-result",
                    relation_id,
                    "choice-one",
                    ["old", "new"],
                ),
                Some(1)
            );
            assert_eq!(
                assertions.outcomes()[2].selected_index_if_string_pair_one_of(
                    "choice-zero-result",
                    relation_id,
                    "choice-zero",
                    ["new", "old"],
                ),
                None
            );
            assert_eq!(
                assertions.outcomes()[2].selected_index_if_string_pair_one_of(
                    "wrong-tag",
                    relation_id,
                    "choice-zero",
                    ["old", "new"],
                ),
                None
            );

            let source_evidence = audit.source();
            assert_eq!(source_evidence.supplied_path(), source);
            assert_eq!(
                source_evidence.managed_snapshot_policy(),
                ManagedSnapshotPolicy::V1
            );
            assert_eq!(
                source_evidence.policy_fingerprint(),
                &super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
            );
            assert_ne!(source_evidence.identity_fingerprint(), &[0_u8; 32]);
            assert_eq!(source_evidence.main_identity().length(), before.1.len);
            assert!(!source_evidence.wal_residual().is_present());
            assert!(source_evidence.wal_residual().metadata().is_none());
            assert!(source_evidence.wal_residual().sha256().is_none());
            assert!(!source_evidence.shm_residual().is_present());
            assert_eq!(fingerprint(&source), before);
            assert_no_sidecars(&source);
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_closed_source_evidence_preserves_present_empty_and_full_unix_metadata() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "closed-source-residuals.db");
        create_relation_assertion_source(&source);
        let connection = Connection::open(&source).unwrap();
        connection
            .execute("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        let mut persist = 1_i32;
        assert_eq!(
            unsafe {
                ffi::sqlite3_file_control(
                    connection.as_raw(),
                    SQLITE_MAIN_SCHEMA.as_ptr().cast(),
                    ffi::SQLITE_FCNTL_PERSIST_WAL,
                    (&mut persist as *mut i32).cast(),
                )
            },
            ffi::SQLITE_OK
        );
        connection
            .execute("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(connection);
        let wal_path = sidecar_path(&source, "-wal");
        let shm_path = sidecar_path(&source, "-shm");
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), 0);
        assert!(shm_path.is_file());
        let main_before = stable_metadata(&source);
        let wal_before = stable_metadata(&wal_path);
        let shm_before = stable_metadata(&shm_path);

        let reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let audit = reader
            .close_with_relation_assertions_v1(|_| Ok(()))
            .unwrap();
        let main = audit.source().main_identity();
        assert_eq!(main.device(), main_before.dev);
        assert_eq!(main.inode(), main_before.ino);
        assert_eq!(main.mode(), main_before.mode);
        assert_eq!(main.uid(), main_before.uid);
        assert_eq!(main.gid(), main_before.gid);
        assert_eq!(main.link_count(), main_before.nlink);
        assert_eq!(main.length(), main_before.len);
        assert_eq!(main.mtime_seconds(), main_before.mtime);
        assert_eq!(main.mtime_nanoseconds(), main_before.mtime_nsec);
        assert_eq!(main.ctime_seconds(), main_before.ctime);
        assert_eq!(main.ctime_nanoseconds(), main_before.ctime_nsec);

        for (residual, expected) in [
            (audit.source().wal_residual(), &wal_before),
            (audit.source().shm_residual(), &shm_before),
        ] {
            assert!(residual.is_present());
            assert!(residual.sha256().is_some());
            let metadata = residual.metadata().unwrap();
            assert_eq!(metadata.length(), expected.len);
            assert_eq!(metadata.device(), expected.dev);
            assert_eq!(metadata.inode(), expected.ino);
            assert_eq!(metadata.mode(), expected.mode);
            assert_eq!(metadata.link_count(), expected.nlink);
            assert_eq!(metadata.uid(), expected.uid);
            assert_eq!(metadata.gid(), expected.gid);
            assert_eq!(metadata.rdev(), expected.rdev);
            assert_eq!(metadata.blocks(), expected.blocks);
            assert_eq!(metadata.block_size(), expected.block_size);
            assert_eq!(metadata.mtime_seconds(), expected.mtime);
            assert_eq!(metadata.mtime_nanoseconds(), expected.mtime_nsec);
            assert_eq!(metadata.ctime_seconds(), expected.ctime);
            assert_eq!(metadata.ctime_nanoseconds(), expected.ctime_nsec);
        }
        assert_eq!(
            audit.source().wal_residual().sha256(),
            Some(&[
                227, 176, 196, 66, 152, 252, 28, 20, 154, 251, 244, 200, 153, 111, 185, 36, 39,
                174, 65, 228, 100, 155, 147, 76, 164, 149, 153, 27, 120, 82, 184, 85,
            ])
        );
        assert_eq!(stable_metadata(&source), main_before);
        assert_eq!(stable_metadata(&wal_path), wal_before);
        assert_eq!(stable_metadata(&shm_path), shm_before);
    }

    #[cfg(unix)]
    #[test]
    fn managed_closed_audit_preserves_real_non_utf8_supplied_path_bytes() {
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let staging = test_path(&directory, "non-utf8-staging.db");
        create_relation_assertion_source(&staging);
        let mut supplied_bytes = fs::canonicalize(directory.path())
            .unwrap()
            .as_os_str()
            .as_bytes()
            .to_vec();
        supplied_bytes.push(b'/');
        supplied_bytes.extend_from_slice(b"closed-source-\x80.db");
        let source = PathBuf::from(std::ffi::OsString::from_vec(supplied_bytes.clone()));
        #[cfg(target_os = "macos")]
        let before_rename = directory_byte_census(directory.path());
        if let Err(error) = fs::rename(&staging, &source) {
            #[cfg(target_os = "macos")]
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(libc::EILSEQ)
            {
                assert_eq!(directory_byte_census(directory.path()), before_rename);
                eprintln!(
                    "filesystem refused the non-UTF8 fixture without mutation; closed-audit coverage requires a filesystem accepting raw names"
                );
                return;
            }
            panic!("failed to create the non-UTF8 source path: {error}");
        }
        let before = fingerprint(&source);

        let reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let audit = reader
            .close_with_relation_assertions_v1(|_| Ok(()))
            .unwrap();
        assert_eq!(
            audit.source().supplied_path().as_os_str().as_bytes(),
            supplied_bytes
        );
        assert_ne!(audit.source().identity_fingerprint(), &[0_u8; 32]);
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_relation_assertion_progress_is_separate_bounded_and_unregistered() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "assertion-progress.db");
        create_relation_assertion_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.inspect_physical_census_v1().unwrap();
        let census_callbacks = reader.census_progress_callbacks_used;
        super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.set(0));
        super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.set(0));
        let evidence = reader
            .run_relation_assertions_with_budget_v1(
                super::ManagedProgressBudget {
                    progress_interval: 1,
                    max_callbacks: 1 << 20,
                    phase: super::ManagedProgressPhase::RelationAssertions,
                },
                |planner| {
                    let relation_id = assertion_relation_id(planner, "assertion_00");
                    planner.require_string_pair(relation_id, "exact", "value")?;
                    planner.require_string_pair_one_of(
                        "progress-choice",
                        relation_id,
                        "choice-one",
                        ["old", "new"],
                    )
                },
            )
            .unwrap();
        assert!(evidence.progress_callbacks_used > 0);
        assert!(evidence.progress_callbacks_used < 1 << 20);
        assert_eq!(reader.census_progress_callbacks_used, census_callbacks);
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.get()),
            1
        );
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.get()),
            1
        );
        super::managed_integer_query(
            reader.connection.as_ref().unwrap(),
            super::MANAGED_PAGE_COUNT_QUERY,
            "post-success assertion callback cleanup probe",
        )
        .unwrap();
        reader.close_and_verify().unwrap();

        let mut interrupted = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.set(0));
        super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.set(0));
        let error = match interrupted.run_relation_assertions_with_budget_v1(
            super::ManagedProgressBudget {
                progress_interval: 1,
                max_callbacks: 1,
                phase: super::ManagedProgressPhase::RelationAssertions,
            },
            |planner| {
                let relation_id = assertion_relation_id(planner, "assertion_00");
                planner.require_string_pair(relation_id, "exact", "value")
            },
        ) {
            Ok(_) => panic!("one assertion callback did not interrupt the phase"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("relation assertions exhausted their fixed work budget")
        );
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.get()),
            1
        );
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.get()),
            1
        );
        super::managed_integer_query(
            interrupted.connection.as_ref().unwrap(),
            super::MANAGED_PAGE_COUNT_QUERY,
            "post-interrupt assertion callback cleanup probe",
        )
        .unwrap();
        assert!(interrupted.close_and_verify().is_err());

        let mut invalid = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let error = match invalid.run_relation_assertions_with_budget_v1(
            super::ManagedProgressBudget {
                progress_interval: 0,
                max_callbacks: 1,
                phase: super::ManagedProgressPhase::RelationAssertions,
            },
            |_| Ok(()),
        ) {
            Ok(_) => panic!("invalid assertion progress budget passed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("relation assertion progress budget was invalid")
        );
        assert!(invalid.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_relation_assertion_failures_are_sticky_and_always_finalize() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "assertion-sticky.db");
        create_relation_assertion_source(&source);

        assert_relation_assertion_failure(&source, "planner was poisoned", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            let first = planner
                .require_exact_row_count(relation_id, 9)
                .expect_err("wrong row count passed");
            assert!(first.to_string().contains("row-count assertion failed"));
            let second = planner
                .require_string_pair(relation_id, "exact", "value")
                .expect_err("caught first error did not poison the planner");
            assert!(second.to_string().contains("planner is poisoned"));
            Ok(())
        });

        assert_relation_assertion_failure(&source, "planner was poisoned", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.transcript.framed_bytes = super::MANAGED_ASSERTION_TRANSCRIPT_BYTE_LIMIT;
            super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.set(0));
            let first = planner
                .require_string_pair(relation_id, "exact", "value")
                .expect_err("transcript byte overflow passed");
            assert!(first.to_string().contains("transcript byte cap"));
            assert_eq!(
                super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.get()),
                0,
                "transcript overflow reached SQL/BLOB access"
            );
            assert!(planner.require_exact_row_count(relation_id, 8).is_err());
            Ok(())
        });

        assert_relation_assertion_failure(&source, "planner was poisoned", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.transcript.entry_count = u64::MAX;
            let first = planner
                .require_exact_row_count(relation_id, 8)
                .expect_err("transcript entry overflow passed");
            assert!(first.to_string().contains("entry count overflowed"));
            assert!(planner.require_exact_row_count(relation_id, 8).is_err());
            Ok(())
        });

        assert_relation_assertion_failure(&source, "planner was poisoned", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.assertions_used = usize::MAX;
            let first = planner
                .require_exact_row_count(relation_id, 8)
                .expect_err("assertion counter overflow passed");
            assert!(first.to_string().contains("assertion cap"));
            assert!(planner.require_exact_row_count(relation_id, 8).is_err());
            Ok(())
        });

        assert_relation_assertion_failure(&source, "callback refused assertions", |_| {
            miette::bail!("callback refused assertions")
        });

        assert_relation_assertion_failure(
            &source,
            "transcript count was inconsistent",
            |planner| {
                let relation_id = assertion_relation_id(planner, "assertion_00");
                planner.require_exact_row_count(relation_id, 8)?;
                planner.transcript.entry_count = 0;
                Ok(())
            },
        );

        super::MANAGED_ASSERTION_TEST_FINALIZE_CODE.with(|code| code.set(Some(ffi::SQLITE_BUSY)));
        assert_relation_assertion_failure(&source, "finalization failed", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_exact_row_count(relation_id, 8)
        });
        super::MANAGED_ASSERTION_TEST_FINALIZE_CODE.with(|code| code.set(None));
    }

    #[test]
    fn managed_relation_assertion_panic_unwinds_statement_and_handler() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "assertion-panic.db");
        create_relation_assertion_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.set(0));
        super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.set(0));
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = reader.run_relation_assertions_v1(|planner| {
                let _ = assertion_relation_id(planner, "assertion_00");
                panic!("assertion callback panic probe")
            });
        }));
        assert!(panic.is_err());
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.get()),
            1
        );
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.get()),
            1
        );
        assert_eq!(
            prepared_statement_count(reader.connection.as_ref().unwrap()),
            0
        );
        super::managed_integer_query(
            reader.connection.as_ref().unwrap(),
            super::MANAGED_PAGE_COUNT_QUERY,
            "post-panic assertion callback cleanup probe",
        )
        .unwrap();
        assert!(reader.run_relation_assertions_v1(|_| Ok(())).is_err());
        assert!(reader.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_relation_assertion_vocabulary_rejects_hostile_inputs_and_caps() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "assertion-hostile.db");
        create_relation_assertion_source(&source);

        assert_relation_assertion_failure(&source, "relation id was not admitted", |planner| {
            planner.require_exact_row_count(0, 28)
        });
        assert_relation_assertion_failure(&source, "relation id was invalid", |planner| {
            planner.require_exact_row_count(1_u64 << 48, 0)
        });
        assert_relation_assertion_failure(&source, "relation id was not admitted", |planner| {
            planner.require_exact_row_count((1_u64 << 48) - 1, 0)
        });
        assert_relation_assertion_failure(&source, "closed string boundary", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair(relation_id, "", "value")
        });
        let oversized: &'static str = Box::leak(
            "z".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT + 1)
                .into_boxed_str(),
        );
        assert_relation_assertion_failure(&source, "closed string boundary", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair(relation_id, oversized, "value")
        });
        assert_relation_assertion_failure(&source, "alternatives were not distinct", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair_one_of(
                "same-alternatives",
                relation_id,
                "choice-zero",
                ["old", "old"],
            )
        });
        assert_relation_assertion_failure(&source, "row-count assertion failed", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_exact_row_count(relation_id, 7)
        });
        assert_relation_assertion_failure(&source, "found no row", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair(relation_id, "missing", "value")
        });
        assert_relation_assertion_failure(&source, "returned another value", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair(relation_id, "exact", "ATTACKER_EXPECTATION")
        });
        assert_relation_assertion_failure(&source, "returned another value", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair_one_of(
                "unknown-choice",
                relation_id,
                "exact",
                ["other-a", "other-b"],
            )
        });
        assert_relation_assertion_failure(&source, "wrong tuple shape", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_01");
            planner.require_string_pair(relation_id, "wrong-type", "seven")
        });
        assert_relation_assertion_failure(&source, "wrong tuple shape", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_02");
            planner.require_string_pair(relation_id, "extra-value", "value")
        });
        assert_relation_assertion_failure(&source, "planner was poisoned", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_03");
            let error = planner
                .require_string_pair(relation_id, "sentinel", "expected")
                .expect_err("attacker database value passed");
            let message = error.to_string();
            assert!(message.contains("returned another value"));
            assert!(!message.contains("ATTACKER_DATABASE_SENTINEL"));
            Ok(())
        });
        assert_relation_assertion_failure(&source, "planner was poisoned", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_04");
            super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.set(0));
            let error = planner
                .require_string_pair(relation_id, "oversized", "expected")
                .expect_err("oversized database value passed the assertion fuse");
            assert!(
                error
                    .to_string()
                    .contains("value exceeded its byte boundary")
            );
            assert_eq!(
                super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.get()),
                0,
                "assertion value fuse requested a BLOB pointer"
            );
            Ok(())
        });
        assert_relation_assertion_failure(&source, "repeated a count relation id", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_exact_row_count(relation_id, 8)?;
            planner.require_exact_row_count(relation_id, 8)
        });
        assert_relation_assertion_failure(&source, "repeated a point key", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair(relation_id, "exact", "value")?;
            planner.require_string_pair_one_of(
                "duplicate-point",
                relation_id,
                "exact",
                ["value", "other"],
            )
        });
        assert_relation_assertion_failure(&source, "repeated an outcome tag", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            planner.require_string_pair_one_of(
                "duplicate-tag",
                relation_id,
                "choice-zero",
                ["old", "new"],
            )?;
            planner.require_string_pair_one_of(
                "duplicate-tag",
                relation_id,
                "choice-one",
                ["old", "new"],
            )
        });
        assert_relation_assertion_failure(&source, "assertion cap was exceeded", |planner| {
            let rows: Vec<_> = planner
                .physical()
                .relation_row_counts()
                .iter()
                .filter(|row| row.relation_id() != 0)
                .map(|row| (row.relation_id(), row.row_count()))
                .collect();
            for (relation_id, count) in rows.iter().take(super::MANAGED_ASSERTION_LIMIT) {
                planner.require_exact_row_count(*relation_id, *count)?;
            }
            let (relation_id, count) = rows[super::MANAGED_ASSERTION_LIMIT];
            planner.require_exact_row_count(relation_id, count)
        });
        let maximum_key: &'static str = Box::leak(
            "x".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT)
                .into_boxed_str(),
        );
        let maximum_value: &'static str = Box::leak(
            "y".repeat(super::MANAGED_ASSERTION_STRING_BYTE_LIMIT)
                .into_boxed_str(),
        );
        assert_relation_assertion_failure(&source, "point-read cap was exceeded", |planner| {
            let relation_id = assertion_relation_id(planner, "assertion_00");
            for (key, value) in [
                ("exact", "value"),
                ("choice-zero", "old"),
                ("choice-one", "new"),
                ("point-three", "three"),
                ("point-four", "four"),
                ("point-five", "five"),
                ("point-six", "six"),
                (maximum_key, maximum_value),
            ] {
                planner.require_string_pair(relation_id, key, value)?;
            }
            planner.require_string_pair(relation_id, "ninth-point", "value")
        });
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_physical_row_gates_reject_before_disallowed_blob_pointer_requests() {
        for variant in [
            "nonblob-value",
            "oversize-key",
            "malformed-key",
            "unknown-id",
            "oversize-value",
            "malformed-value",
        ] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("managed-row-{variant}.db"));
            create_populated_managed_cozo_source(&source);
            let (_, catalog_value) = relation_rows(&source).remove(0);
            let handle = RelationHandle::decode(&catalog_value).unwrap();
            let admitted_key = vec![DataValue::from(999_i64)].encode_as_key(handle.id);
            let key = match variant {
                "oversize-key" => {
                    let mut key = handle.id.raw_encode().to_vec();
                    key.resize(MAX_ENCODED_KEY_BYTES + 1, 0);
                    key
                }
                "malformed-key" => {
                    let mut key = handle.id.raw_encode().to_vec();
                    key.push(0xff);
                    key
                }
                "unknown-id" => {
                    vec![DataValue::from(999_i64)].encode_as_key(RelationId::new(10_000))
                }
                _ => admitted_key,
            };
            match variant {
                "nonblob-value" => {
                    insert_raw_text_value_first(&source, &key, "ATTACKER_ROW_SENTINEL")
                }
                "oversize-value" => insert_raw_oversize_value_first(&source, &key),
                "malformed-value" => insert_raw_row_first(&source, &key, &[0xc1]),
                _ => insert_raw_row_first(&source, &key, &[]),
            }

            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.set(0));
            let error = match reader.inspect_physical_census_v1() {
                Ok(_) => panic!("hostile physical row variant {variant} passed"),
                Err(error) => error,
            };
            let requests =
                super::MANAGED_PHYSICAL_BLOB_POINTER_REQUESTS.with(|requests| requests.get());
            let expected_requests = match variant {
                "nonblob-value" | "oversize-key" => 0,
                "malformed-key" | "unknown-id" | "oversize-value" => 1,
                "malformed-value" => 2,
                _ => unreachable!(),
            };
            assert_eq!(
                requests, expected_requests,
                "variant {variant} requested a disallowed BLOB pointer"
            );
            let message = error.to_string();
            assert!(!message.contains("ATTACKER_ROW_SENTINEL"));
            assert!(message.len() < 192, "unbounded diagnostic: {message}");
            assert!(reader.close_and_verify().is_err());
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn managed_physical_census_binds_id_zero_bytes_to_cached_catalog() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-cached-id-zero.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.inspect_primary_index_catalog_v1().unwrap();
        let cached = reader.catalog.as_mut().unwrap();
        assert!(!cached.raw_rows[0].value.is_empty());
        cached.raw_rows[0].value[0] ^= 0x01;

        let error = match reader.inspect_physical_census_v1() {
            Ok(_) => panic!("table bytes passed after cached id-zero evidence changed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cached catalog"));
        assert!(reader.close_and_verify().is_err());
    }

    #[test]
    fn managed_physical_census_rejects_duplicate_or_zero_top_level_catalog_ids() {
        for variant in ["duplicate", "zero"] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("managed-id-{variant}.db"));
            create_populated_managed_cozo_source(&source);
            let rows = relation_rows(&source);
            let first = RelationHandle::decode(&rows[0].1).unwrap();
            let mut changed = RelationHandle::decode(&rows[1].1).unwrap();
            changed.id = if variant == "duplicate" {
                first.id
            } else {
                RelationId::SYSTEM
            };
            let encoded = encode_test_catalog_record(&changed, CatalogCodec::StructMapV1);
            update_raw_value(&source, &rows[1].0, &encoded);

            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            let error = match reader.inspect_physical_census_v1() {
                Ok(_) => panic!("{variant} top-level id passed physical attribution"),
                Err(error) => error,
            };
            let message = error.to_string();
            assert!(message.contains(if variant == "duplicate" {
                "repeated a top-level relation id"
            } else {
                "system id zero"
            }));
            assert!(reader.close_and_verify().is_err());
        }
    }

    #[test]
    fn managed_covering_query_plan_is_covering_and_runtime_uses_no_sort() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-covering-plan.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let connection = reader.connection.as_ref().unwrap();
        super::managed_covering_index_plan_gate(connection).unwrap();

        let observation = reader.inspect_physical_census_v1().unwrap();
        assert_eq!(
            observation.physical().table_row_count(),
            observation.physical().index_row_count()
        );
        reader.close_and_verify().unwrap();
    }

    #[test]
    fn managed_primary_index_catalog_classifies_all_codecs_without_f7_admission() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-codecs.db");
        create_populated_managed_cozo_source(&source);
        let rows = relation_rows(&source);
        let codecs = [
            CatalogCodec::PositionalV0,
            CatalogCodec::PositionalV1,
            CatalogCodec::StructMapV1,
        ];
        for ((key, value), codec) in rows.iter().take(3).zip(codecs) {
            let mut handle = RelationHandle::decode(value).unwrap();
            // Hidden is syntactically representable but forbidden by Mneme's
            // later F7 oracle. This reader must classify, not semantically admit.
            handle.access_level = AccessLevel::Hidden;
            let encoded = encode_test_catalog_record(&handle, codec);
            update_raw_value(&source, key, &encoded);
        }

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let observation = reader.inspect_primary_index_catalog_v1().unwrap();
        let observed: Vec<_> = observation
            .catalog()
            .entries()
            .iter()
            .take(3)
            .map(|entry| entry.encoding())
            .collect();
        assert_eq!(
            observed,
            vec![
                ManagedCatalogEncodingV1::PositionalV0,
                ManagedCatalogEncodingV1::PositionalV1,
                ManagedCatalogEncodingV1::StructMapV1,
            ]
        );
        assert!(observation.catalog().entries()[..3]
            .iter()
            .all(|entry| entry.relation().access_level() == crate::ManagedAccessLevelV1::Hidden));
        reader.close_and_verify().unwrap();
    }

    #[test]
    fn managed_catalog_commitments_cover_metadata_and_normalize_relation_codecs() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-commitments.db");
        create_populated_managed_cozo_source(&source);
        let baseline = catalog_commitments(&source);

        let (key, value) = relation_rows(&source).remove(0);
        let handle = RelationHandle::decode(&value).unwrap();
        let positional = encode_test_catalog_record(&handle, CatalogCodec::PositionalV1);
        update_raw_value(&source, &key, &positional);
        let transcoded = catalog_commitments(&source);
        assert_ne!(transcoded.0, baseline.0);
        assert_eq!(transcoded.1, baseline.1);

        let counter_key = vec![DataValue::Null].encode_as_key(RelationId::SYSTEM);
        update_raw_value(&source, &counter_key, &27_u64.to_be_bytes());
        let counter_changed = catalog_commitments(&source);
        assert_ne!(counter_changed.0, transcoded.0);
        assert_ne!(counter_changed.1, transcoded.1);
    }

    #[test]
    fn managed_primary_index_catalog_rejects_key_name_mismatch_and_poison_sticks() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-linkage.db");
        create_populated_managed_cozo_source(&source);
        let (key, value) = relation_rows(&source).remove(0);
        let mut handle = RelationHandle::decode(&value).unwrap();
        handle.name = "different_name".into();
        let encoded = encode_test_catalog_record(&handle, CatalogCodec::StructMapV1);
        update_raw_value(&source, &key, &encoded);

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let error = primary_index_catalog_error(&mut reader);
        assert!(error.to_string().contains("linkage mismatch"));
        assert!(reader.inspect_primary_index_catalog_v1().is_err());
        let close = reader
            .close_and_verify()
            .expect_err("a poisoned scan must never close as success");
        assert!(close.to_string().contains("poisoned"));
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_primary_index_catalog_rejects_missing_and_foreign_visible_shapes() {
        for variant in ["missing", "foreign"] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("managed-{variant}.db"));
            create_populated_managed_cozo_source(&source);
            let (key, value) = relation_rows(&source).remove(0);
            if variant == "missing" {
                delete_raw_row(&source, &key);
            } else {
                let foreign = vec![DataValue::from(true)].encode_as_key(RelationId::SYSTEM);
                replace_raw_row(&source, &key, &foreign, &value);
            }

            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            let message = primary_index_catalog_error(&mut reader).to_string();
            if variant == "missing" {
                assert!(message.contains("exact V1 syntactic domain"));
            } else {
                assert!(message.contains("foreign id-zero key shape"));
            }
            assert!(reader.close_and_verify().is_err());
        }
    }

    #[test]
    fn managed_primary_index_catalog_uses_the_twenty_ninth_row_as_a_canary() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-row-canary.db");
        create_populated_managed_cozo_source(&source);
        let (_, value) = relation_rows(&source).remove(0);
        let mut handle = RelationHandle::decode(&value).unwrap();
        handle.name = "zz_extra_visible_relation".into();
        let key = vec![DataValue::from(handle.name.as_str())].encode_as_key(RelationId::SYSTEM);
        let value = encode_test_catalog_record(&handle, CatalogCodec::StructMapV1);
        insert_raw_row(&source, &key, &value);

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let message = primary_index_catalog_error(&mut reader).to_string();
        assert!(message.contains("fixed row boundary"));
        assert!(reader.close_and_verify().is_err());
    }

    #[test]
    fn managed_primary_index_catalog_requires_exact_counter_and_storage_version() {
        let counter_key = vec![DataValue::Null].encode_as_key(RelationId::SYSTEM);
        let version_key = vec![DataValue::Null, DataValue::from("STORAGE_VERSION")]
            .encode_as_key(RelationId::SYSTEM);
        for (label, key, value) in [
            ("counter", counter_key, vec![0_u8; 7]),
            ("version", version_key, vec![1_u8]),
        ] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("managed-{label}.db"));
            create_populated_managed_cozo_source(&source);
            update_raw_value(&source, &key, &value);
            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            let message = primary_index_catalog_error(&mut reader).to_string();
            assert!(message.contains(if label == "counter" {
                "counter row"
            } else {
                "storage-version row"
            }));
            assert!(reader.close_and_verify().is_err());
        }
    }

    #[test]
    fn managed_catalog_policy_fingerprint_is_a_hard_pinned_literal_oracle() {
        let derived = derive_catalog_policy_fingerprint_v1();
        assert_eq!(derived, MANAGED_CATALOG_POLICY_FINGERPRINT_V1);
        assert_ne!(MANAGED_CATALOG_POLICY_FINGERPRINT_V1, [0_u8; 32]);
    }

    #[test]
    fn managed_physical_policy_fingerprint_is_a_hard_pinned_literal_oracle() {
        let derived = derive_physical_policy_fingerprint_v1();
        assert_eq!(derived, super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1);
        assert_ne!(super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1, [0_u8; 32]);
    }

    #[test]
    fn managed_assertion_policy_fingerprint_is_a_hard_pinned_literal_oracle() {
        let derived = derive_assertion_policy_fingerprint_v1();
        assert_eq!(derived, super::MANAGED_ASSERTION_POLICY_FINGERPRINT_V1);
        assert_ne!(super::MANAGED_ASSERTION_POLICY_FINGERPRINT_V1, [0_u8; 32]);
    }

    #[test]
    fn managed_record_visit_policy_fingerprint_is_a_hard_pinned_literal_oracle() {
        let derived = derive_record_visit_policy_fingerprint_v1();
        assert_eq!(derived, super::MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1);
        assert_ne!(
            super::MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1,
            [0_u8; 32]
        );
    }

    #[test]
    fn managed_record_visit_emits_ordered_relations_and_binds_exact_raw_transcript() {
        enum Trace {
            Begin(u16, u64),
            Record(u16, Vec<DataValue>, Vec<DataValue>),
            End(u16, u64),
        }

        let directory = tempdir().unwrap();
        let source = test_path(&directory, "record-visit-success.db");
        create_relation_assertion_source(&source);
        let closed = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap()
            .close_with_relation_assertions_and_record_visit_v1(
                Vec::<Trace>::new(),
                record_visit_relation_ids,
                |trace, event| {
                    match event {
                        super::ManagedSqliteRecordVisitEventV1::BeginRelation {
                            ordinal,
                            expected_rows,
                        } => trace.push(Trace::Begin(ordinal, expected_rows)),
                        super::ManagedSqliteRecordVisitEventV1::Record {
                            ordinal,
                            key,
                            value,
                        } => trace.push(Trace::Record(ordinal, key.to_vec(), value.to_vec())),
                        super::ManagedSqliteRecordVisitEventV1::EndRelation { ordinal, rows } => {
                            trace.push(Trace::End(ordinal, rows))
                        }
                    }
                    Ok(())
                },
            )
            .unwrap();

        let evidence = closed.visit_evidence();
        assert_eq!(evidence.relation_ids().len(), 16);
        assert_eq!(evidence.relation_row_counts().len(), 16);
        assert_eq!(
            evidence.total_row_count(),
            evidence.relation_row_counts().iter().sum::<u64>()
        );
        assert!(
            evidence.progress_callbacks_used()
                >= closed.audit().physical().progress_callbacks_used()
        );
        assert!(
            evidence.progress_callbacks_used() <= super::MANAGED_PHYSICAL_PROGRESS_CALLBACK_LIMIT
        );
        assert_eq!(
            evidence.policy_fingerprint(),
            &super::MANAGED_RECORD_VISIT_POLICY_FINGERPRINT_V1
        );
        let expected_transcript = independently_hash_record_visit_transcript(
            &source,
            evidence.relation_ids(),
            evidence.relation_row_counts(),
            evidence.total_row_count(),
            evidence.progress_callbacks_used(),
        );
        assert_eq!(evidence.transcript_commitment(), &expected_transcript);

        let (_audit, evidence, trace) = closed.into_parts();
        let mut cursor = 0_usize;
        let mut observed_records = 0_u64;
        let mut saw_empty = false;
        for (index, expected_rows) in evidence.relation_row_counts().iter().copied().enumerate() {
            let ordinal = u16::try_from(index + 1).unwrap();
            match &trace[cursor] {
                Trace::Begin(actual, rows) => {
                    assert_eq!((*actual, *rows), (ordinal, expected_rows));
                }
                Trace::Record(_, _, _) | Trace::End(_, _) => panic!("relation did not begin"),
            }
            cursor += 1;
            for _ in 0..expected_rows {
                match &trace[cursor] {
                    Trace::Record(actual, key, value) => {
                        assert_eq!(*actual, ordinal);
                        assert!(!key.is_empty());
                        let _ = value;
                    }
                    Trace::Begin(_, _) | Trace::End(_, _) => {
                        panic!("relation record sequence was truncated")
                    }
                }
                observed_records += 1;
                cursor += 1;
            }
            match &trace[cursor] {
                Trace::End(actual, rows) => {
                    assert_eq!((*actual, *rows), (ordinal, expected_rows));
                }
                Trace::Begin(_, _) | Trace::Record(_, _, _) => panic!("relation did not end"),
            }
            if expected_rows == 0 {
                saw_empty = true;
            }
            cursor += 1;
        }
        assert!(saw_empty, "fixture must prove empty begin/end segments");
        assert_eq!(cursor, trace.len());
        assert_eq!(observed_records, evidence.total_row_count());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_record_visit_rejects_duplicate_foreign_and_zero_ids_with_sink_withheld() {
        for (label, mutate, expected) in [
            (
                "duplicate",
                (|ids: &mut [u64; 16]| ids[15] = ids[0]) as fn(&mut [u64; 16]),
                "relation id was repeated",
            ),
            (
                "foreign",
                (|ids: &mut [u64; 16]| ids[15] = (1_u64 << 48) - 1) as fn(&mut [u64; 16]),
                "relation id was not admitted",
            ),
            (
                "zero",
                (|ids: &mut [u64; 16]| ids[15] = 0) as fn(&mut [u64; 16]),
                "relation id was zero",
            ),
        ] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("record-visit-{label}.db"));
            create_relation_assertion_source(&source);
            let dropped = Arc::new(AtomicBool::new(false));
            let result = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap()
                .close_with_relation_assertions_and_record_visit_v1(
                    RecordVisitDropSpy(Arc::clone(&dropped)),
                    |planner| {
                        let mut ids = record_visit_relation_ids(planner)?;
                        mutate(&mut ids);
                        Ok(ids)
                    },
                    |_, _| Ok(()),
                );
            let error = match result {
                Ok(_) => panic!("hostile {label} record selection passed"),
                Err(error) => error,
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert!(dropped.load(Ordering::SeqCst), "{label} sink was returned");
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn managed_record_visit_callback_error_and_progress_exhaustion_drop_the_sink() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "record-visit-callback.db");
        create_relation_assertion_source(&source);
        let dropped = Arc::new(AtomicBool::new(false));
        let result = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap()
            .close_with_relation_assertions_and_record_visit_v1(
                RecordVisitDropSpy(Arc::clone(&dropped)),
                record_visit_relation_ids,
                |_, event| match event {
                    super::ManagedSqliteRecordVisitEventV1::Record { .. } => {
                        Err(miette::miette!("injected record callback failure"))
                    }
                    _ => Ok(()),
                },
            );
        let error = match result {
            Ok(_) => panic!("failing record callback returned a closed sink"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("record callback failure"));
        assert!(dropped.load(Ordering::SeqCst));

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let mut selected = None;
        reader
            .run_relation_assertions_v1(|planner| {
                selected = Some(record_visit_relation_ids(planner)?);
                Ok(())
            })
            .unwrap();
        let selected = selected.unwrap();
        let one_scan_callback = reader
            .census_progress_callbacks_used
            .checked_add(1)
            .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let result = reader.run_record_visit_with_budget_v1(
            super::ManagedProgressBudget {
                progress_interval: 1,
                max_callbacks: one_scan_callback,
                phase: super::ManagedProgressPhase::Census,
            },
            RecordVisitDropSpy(Arc::clone(&dropped)),
            selected,
            |_, _| Ok(()),
        );
        let error = match result {
            Ok(_) => panic!("record visitor escaped its remaining progress budget"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("census exhausted its fixed work budget")
        );
        assert!(dropped.load(Ordering::SeqCst));
        assert!(reader.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_record_visit_statement_reset_finalize_and_read_failures_withhold_sink() {
        for (failure, expected) in [
            ("prepare", "statement preparation failure"),
            ("reset", "reset failure"),
            ("clear", "clear failure"),
            ("finalize", "statement finalization failed"),
            ("read", "READ-state failure"),
        ] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("record-visit-{failure}.db"));
            create_relation_assertion_source(&source);
            let reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            let destroyed = install_connection_destruction_probe(&reader);
            let dropped = Arc::new(AtomicBool::new(false));
            match failure {
                "prepare" => {
                    super::MANAGED_RECORD_VISIT_PREPARE_FAILURE.with(|injected| injected.set(true))
                }
                "reset" => {
                    super::MANAGED_RECORD_VISIT_RESET_FAILURE.with(|injected| injected.set(true))
                }
                "clear" => {
                    super::MANAGED_RECORD_VISIT_CLEAR_FAILURE.with(|injected| injected.set(true))
                }
                "finalize" => super::MANAGED_RECORD_VISIT_FINALIZE_CODE
                    .with(|injected| injected.set(Some(ffi::SQLITE_ERROR))),
                "read" => {
                    super::MANAGED_RECORD_VISIT_READ_FAILURE.with(|injected| injected.set(true))
                }
                _ => unreachable!(),
            }
            let result = reader.close_with_relation_assertions_and_record_visit_v1(
                RecordVisitDropSpy(Arc::clone(&dropped)),
                record_visit_relation_ids,
                |_, _| Ok(()),
            );
            super::MANAGED_RECORD_VISIT_PREPARE_FAILURE.with(|injected| injected.set(false));
            super::MANAGED_RECORD_VISIT_RESET_FAILURE.with(|injected| injected.set(false));
            super::MANAGED_RECORD_VISIT_CLEAR_FAILURE.with(|injected| injected.set(false));
            super::MANAGED_RECORD_VISIT_FINALIZE_CODE.with(|injected| injected.set(None));
            super::MANAGED_RECORD_VISIT_READ_FAILURE.with(|injected| injected.set(false));
            let error = match result {
                Ok(_) => panic!("{failure} yielded a closed record-visit sink"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains(expected),
                "unexpected {failure} diagnostic: {error}"
            );
            assert!(dropped.load(Ordering::SeqCst), "{failure} sink survived");
            assert!(
                destroyed.load(Ordering::SeqCst),
                "{failure} skipped SQLite cleanup"
            );
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn managed_record_visit_rollback_close_and_source_failures_withhold_sink() {
        let directory = tempdir().unwrap();

        let rollback_source = test_path(&directory, "record-visit-rollback.db");
        create_relation_assertion_source(&rollback_source);
        let rollback_reader = ExistingSqliteSnapshotSource::open(&rollback_source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let rollback_destroyed = install_connection_destruction_probe(&rollback_reader);
        let rollback_dropped = Arc::new(AtomicBool::new(false));
        let rollback_finalize = Arc::new(AtomicI32::new(ffi::SQLITE_OK));
        let rollback_sink = RecordVisitControlSink {
            connection: rollback_reader.connection.as_ref().unwrap().as_raw(),
            statement: std::ptr::null_mut(),
            configured: false,
            dropped: Arc::clone(&rollback_dropped),
            statement_finalize_code: rollback_finalize,
        };
        let rollback_result = rollback_reader.close_with_relation_assertions_and_record_visit_v1(
            rollback_sink,
            record_visit_relation_ids,
            |sink, event| {
                if !sink.configured
                    && matches!(
                        event,
                        super::ManagedSqliteRecordVisitEventV1::BeginRelation { .. }
                    )
                {
                    let code = unsafe {
                        ffi::sqlite3_set_authorizer(
                            sink.connection,
                            Some(deny_sqlite_transaction_authorizer),
                            std::ptr::null_mut(),
                        )
                    };
                    if code != ffi::SQLITE_OK {
                        return Err(miette::miette!(
                            "record-visit rollback authorizer failed with code {code}"
                        ));
                    }
                    sink.configured = true;
                }
                Ok(())
            },
        );
        let rollback_error = match rollback_result {
            Ok(_) => panic!("rollback failure yielded a closed record-visit sink"),
            Err(error) => error,
        };
        assert!(rollback_error.to_string().contains("rollback"));
        assert!(rollback_dropped.load(Ordering::SeqCst));
        assert!(rollback_destroyed.load(Ordering::SeqCst));

        let close_source = test_path(&directory, "record-visit-close.db");
        create_relation_assertion_source(&close_source);
        let close_reader = ExistingSqliteSnapshotSource::open(&close_source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let close_destroyed = install_connection_destruction_probe(&close_reader);
        let close_dropped = Arc::new(AtomicBool::new(false));
        let close_finalize = Arc::new(AtomicI32::new(-1));
        let close_sink = RecordVisitControlSink {
            connection: close_reader.connection.as_ref().unwrap().as_raw(),
            statement: std::ptr::null_mut(),
            configured: false,
            dropped: Arc::clone(&close_dropped),
            statement_finalize_code: Arc::clone(&close_finalize),
        };
        let close_result = close_reader.close_with_relation_assertions_and_record_visit_v1(
            close_sink,
            record_visit_relation_ids,
            |sink, event| {
                if !sink.configured
                    && matches!(
                        event,
                        super::ManagedSqliteRecordVisitEventV1::BeginRelation { .. }
                    )
                {
                    let code = unsafe {
                        ffi::sqlite3_prepare_v2(
                            sink.connection,
                            CLEANUP_PROBE_STATEMENT.as_ptr().cast(),
                            -1,
                            &mut sink.statement,
                            std::ptr::null_mut(),
                        )
                    };
                    if code != ffi::SQLITE_OK || sink.statement.is_null() {
                        return Err(miette::miette!(
                            "record-visit close probe preparation failed with code {code}"
                        ));
                    }
                    sink.configured = true;
                }
                Ok(())
            },
        );
        let close_error = match close_result {
            Ok(_) => panic!("strict close failure yielded a closed record-visit sink"),
            Err(error) => error,
        };
        assert!(close_error.to_string().contains("failed to close"));
        assert!(
            close_error
                .to_string()
                .contains("deferred cleanup was armed")
        );
        assert!(close_dropped.load(Ordering::SeqCst));
        assert_eq!(close_finalize.load(Ordering::SeqCst), ffi::SQLITE_OK);
        assert!(close_destroyed.load(Ordering::SeqCst));

        let source_failure = test_path(&directory, "record-visit-source.db");
        create_relation_assertion_source(&source_failure);
        let source_reader = ExistingSqliteSnapshotSource::open(&source_failure)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let source_destroyed = install_connection_destruction_probe(&source_reader);
        let source_dropped = Arc::new(AtomicBool::new(false));
        super::MANAGED_SOURCE_RESIDUAL_TEST_FAILURE.with(|failure| failure.set(true));
        let source_result = source_reader.close_with_relation_assertions_and_record_visit_v1(
            RecordVisitDropSpy(Arc::clone(&source_dropped)),
            record_visit_relation_ids,
            |_, _| Ok(()),
        );
        super::MANAGED_SOURCE_RESIDUAL_TEST_FAILURE.with(|failure| failure.set(false));
        let source_error = match source_result {
            Ok(_) => panic!("source failure yielded a closed record-visit sink"),
            Err(error) => error,
        };
        assert!(
            source_error
                .to_string()
                .contains("injected managed SQLite residual")
        );
        assert!(source_dropped.load(Ordering::SeqCst));
        assert!(source_destroyed.load(Ordering::SeqCst));

        assert_no_sidecars(&rollback_source);
        assert_no_sidecars(&close_source);
        assert_no_sidecars(&source_failure);
    }

    #[test]
    fn catalog_fence_policy_fingerprints_are_hard_pinned_literal_oracles() {
        let assertion = derive_catalog_fence_assertion_policy_fingerprint_v1();
        let closed = derive_closed_catalog_fence_policy_fingerprint_v1();
        assert_eq!(
            assertion,
            super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            closed,
            super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
    }

    fn catalog_fence_one_of_transcript_kat(selected: u8) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1);
        let kind = [2_u8];
        let id = 0x0102_0304_0506_u64.to_be_bytes();
        let selected = [selected];
        let fields = [
            kind.as_slice(),
            b"synthetic-choice".as_slice(),
            id.as_slice(),
            b"key".as_slice(),
            b"old".as_slice(),
            b"new".as_slice(),
            selected.as_slice(),
        ];
        // Deliberately independent of the production transcript builder.
        hasher.update([0x01]);
        for field in fields {
            hasher.update(u64::try_from(field.len()).unwrap().to_be_bytes());
            hasher.update(field);
        }
        hasher.update(7_u64.to_be_bytes());
        hasher.update([0xff]);
        hasher.update(1_u64.to_be_bytes());
        hasher.finalize().into()
    }

    fn catalog_fence_transcript_oracle(entries: &[Vec<Vec<u8>>]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(super::MANAGED_CATALOG_FENCE_ASSERTION_TRANSCRIPT_DOMAIN_V1);
        for entry in entries {
            hasher.update([0x01]);
            for field in entry {
                hasher.update(u64::try_from(field.len()).unwrap().to_be_bytes());
                hasher.update(field);
            }
            hasher.update(u64::try_from(entry.len()).unwrap().to_be_bytes());
        }
        hasher.update([0xff]);
        hasher.update(u64::try_from(entries.len()).unwrap().to_be_bytes());
        hasher.finalize().into()
    }

    #[test]
    fn catalog_fence_transcript_selected_alternative_kats_are_hard_pinned() {
        let zero = catalog_fence_one_of_transcript_kat(0);
        let one = catalog_fence_one_of_transcript_kat(1);
        assert_eq!(
            zero,
            [
                142, 59, 22, 104, 189, 168, 224, 201, 63, 86, 130, 231, 196, 39, 89, 254, 223, 0,
                54, 96, 166, 112, 86, 171, 70, 160, 111, 239, 197, 139, 36, 16
            ]
        );
        assert_eq!(
            one,
            [
                181, 165, 118, 80, 62, 112, 218, 129, 67, 46, 114, 228, 237, 103, 88, 49, 247, 111,
                73, 90, 138, 255, 181, 238, 62, 91, 185, 222, 216, 102, 144, 184
            ]
        );
    }

    #[test]
    fn catalog_fence_production_transcript_matches_independent_count_pair_one_of_oracle() {
        for (key, expected_value, selected) in
            [("choice-zero", "old", 0_u8), ("choice-one", "new", 1_u8)]
        {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, "catalog-fence-transcript-oracle.db");
            create_relation_assertion_source(&source);
            let fence = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                    let relation = catalog_fence_relation_id(planner, "assertion_00");
                    planner.require_exact_row_count(relation, 8)?;
                    planner.require_string_pair(relation, "exact", "value")?;
                    planner.require_string_pair_one_of(
                        "oracle-choice",
                        relation,
                        key,
                        ["old", "new"],
                    )
                })
                .unwrap();
            let relation = fence
                .catalog()
                .catalog()
                .entries()
                .iter()
                .find(|entry| entry.relation().name() == "assertion_00")
                .unwrap()
                .relation()
                .id();
            let expected = catalog_fence_transcript_oracle(&[
                vec![
                    vec![0],
                    relation.to_be_bytes().to_vec(),
                    8_u64.to_be_bytes().to_vec(),
                ],
                vec![
                    vec![1],
                    relation.to_be_bytes().to_vec(),
                    b"exact".to_vec(),
                    b"value".to_vec(),
                ],
                vec![
                    vec![2],
                    b"oracle-choice".to_vec(),
                    relation.to_be_bytes().to_vec(),
                    key.as_bytes().to_vec(),
                    b"old".to_vec(),
                    b"new".to_vec(),
                    vec![selected],
                ],
            ]);
            assert_eq!(fence.assertions().transcript_commitment(), &expected);
            assert_eq!(
                fence.assertions().outcomes()[2].selected_index_if_string_pair_one_of(
                    "oracle-choice",
                    relation,
                    key,
                    ["old", "new"]
                ),
                Some(selected)
            );
            assert_eq!(expected_value, if selected == 0 { "old" } else { "new" });
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn catalog_fence_public_transcript_exact_8801_byte_maximum_succeeds() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-transcript-exact-limit.db");
        let inputs = catalog_fence_transcript_boundary_inputs(256);
        create_catalog_fence_transcript_boundary_source(&source, &inputs);
        super::MANAGED_CATALOG_FENCE_POINT_EXECUTIONS.with(|count| count.set(0));
        let fence = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let relation = catalog_fence_relation_id(planner, "assertion_00");
                for &(tag, key, alternatives) in &inputs {
                    planner.require_string_pair_one_of(tag, relation, key, alternatives)?;
                }
                Ok(())
            })
            .unwrap();
        let relation = fence
            .catalog()
            .catalog()
            .entries()
            .iter()
            .find(|entry| entry.relation().name() == "assertion_00")
            .unwrap()
            .relation()
            .id();
        let entries = catalog_fence_transcript_boundary_entries(relation, &inputs);
        assert_eq!(catalog_fence_transcript_framed_bytes(&entries), 8_801);
        assert_eq!(fence.assertions().assertion_count(), 8);
        assert_eq!(fence.assertions().point_read_count(), 8);
        assert_eq!(
            fence.assertions().transcript_commitment(),
            &catalog_fence_transcript_oracle(&entries)
        );
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_POINT_EXECUTIONS.with(|count| count.get()),
            8
        );
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_public_transcript_8802_bytes_refuses_before_eighth_point_sql() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-transcript-plus-one.db");
        let inputs = catalog_fence_transcript_boundary_inputs(257);
        create_catalog_fence_transcript_boundary_source(&source, &inputs);
        super::MANAGED_CATALOG_FENCE_POINT_EXECUTIONS.with(|count| count.set(0));
        let relation = Cell::new(None);
        let result = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let admitted = catalog_fence_relation_id(planner, "assertion_00");
                relation.set(Some(admitted));
                for &(tag, key, alternatives) in &inputs {
                    planner.require_string_pair_one_of(tag, admitted, key, alternatives)?;
                }
                Ok(())
            });
        let error = match result {
            Ok(_) => panic!("8,802-byte catalog-fence transcript yielded evidence"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("outcome tag exceeded its static string boundary")
        );
        let entries = catalog_fence_transcript_boundary_entries(relation.get().unwrap(), &inputs);
        assert_eq!(catalog_fence_transcript_framed_bytes(&entries), 8_802);
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_POINT_EXECUTIONS.with(|count| count.get()),
            7
        );
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_caught_method_error_poisoning_withholds_evidence() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-caught-error.db");
        create_relation_assertion_source(&source);
        let error = match ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let relation = catalog_fence_relation_id(planner, "assertion_00");
                assert!(planner.require_exact_row_count(relation, 0).is_err());
                assert!(planner.require_exact_row_count(relation, 8).is_err());
                Ok(())
            }) {
            Ok(_) => panic!("caught catalog-fence method error yielded evidence"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("planner was poisoned"));
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_callback_error_and_both_finalizer_failures_are_composed_and_attempted() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-callback-and-finalizers.db");
        create_relation_assertion_source(&source);
        super::MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CALLS.with(|calls| calls.set(0));
        super::MANAGED_CATALOG_FENCE_POINT_FINALIZE_CALLS.with(|calls| calls.set(0));
        super::MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CODE
            .with(|code| code.set(Some(ffi::SQLITE_BUSY)));
        super::MANAGED_CATALOG_FENCE_POINT_FINALIZE_CODE
            .with(|code| code.set(Some(ffi::SQLITE_ERROR)));
        let error = match ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |_| {
                miette::bail!("catalog-fence callback refused assertions")
            }) {
            Ok(_) => panic!("callback and finalizer failures yielded evidence"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("callback refused assertions"));
        assert!(message.contains("Some(5)/Some(1)"));
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CALLS.with(|calls| calls.get()),
            1
        );
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_POINT_FINALIZE_CALLS.with(|calls| calls.get()),
            1
        );
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_private_reader_path_rejects_preinspected_state() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-preinspected.db");
        create_relation_assertion_source(&source);
        let reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let mut reader = reader;
        reader.inspect_primary_index_catalog_v1().unwrap();
        let error = match reader.close_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |_| Ok(())) {
            Ok(_) => panic!("preinspected reader yielded a catalog fence"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("initial ready reader state"));
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_consumes_fresh_source_with_bounded_range_and_point_observations() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence.db");
        create_relation_assertion_source(&source);
        let before = fingerprint(&source);
        super::MANAGED_CATALOG_FENCE_PROGRESS_INSTALLS.with(|count| count.set(0));
        super::MANAGED_CATALOG_FENCE_PROGRESS_UNREGISTRATIONS.with(|count| count.set(0));

        let fence = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let populated = catalog_fence_relation_id(planner, "assertion_00");
                let empty = catalog_fence_relation_id(planner, "assertion_05");
                planner.require_exact_row_count(empty, 0)?;
                planner.require_exact_row_count(populated, 8)?;
                planner.require_string_pair(populated, "exact", "value")?;
                planner.require_string_pair_one_of(
                    "catalog-fence-choice",
                    populated,
                    "choice-one",
                    ["old", "new"],
                )?;
                Ok(())
            })
            .unwrap();

        assert_eq!(
            fence.catalog().catalog().len(),
            super::MANAGED_CATALOG_RELATION_COUNT
        );
        assert_eq!(fence.assertions().assertion_count(), 4);
        assert_eq!(fence.assertions().range_probe_count(), 2);
        assert_eq!(fence.assertions().point_read_count(), 2);
        assert!(fence.assertions().progress_callbacks_used() < 1_024);
        assert_eq!(
            fence.assertion_policy_fingerprint(),
            &super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.catalog_fence_policy_fingerprint(),
            &super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.source().managed_snapshot_policy(),
            ManagedSnapshotPolicy::V1
        );
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_PROGRESS_INSTALLS.with(|count| count.get()),
            1
        );
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_PROGRESS_UNREGISTRATIONS.with(|count| count.get()),
            1
        );
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_rejects_out_of_cap_range_before_sql() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-over-cap.db");
        create_relation_assertion_source(&source);
        super::MANAGED_CATALOG_FENCE_RANGE_EXECUTIONS.with(|count| count.set(0));
        let error = match ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let relation = catalog_fence_relation_id(planner, "assertion_00");
                planner.require_exact_row_count(relation, 9)
            }) {
            Ok(_) => panic!("out-of-cap catalog-fence range unexpectedly yielded evidence"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("expected row count exceeded"));
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_RANGE_EXECUTIONS.with(|count| count.get()),
            0
        );
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_rejects_duplicate_range_before_second_sql() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-duplicate.db");
        create_relation_assertion_source(&source);
        super::MANAGED_CATALOG_FENCE_RANGE_EXECUTIONS.with(|count| count.set(0));
        let error = match ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let relation = catalog_fence_relation_id(planner, "assertion_00");
                planner.require_exact_row_count(relation, 8)?;
                planner.require_exact_row_count(relation, 8)
            }) {
            Ok(_) => panic!("duplicate catalog-fence range unexpectedly yielded evidence"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("repeated a count"));
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_RANGE_EXECUTIONS.with(|count| count.get()),
            1
        );
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_range_count_matrix_is_exact_and_bounded() {
        for (actual, expected, accepted) in [
            (0, 0, true),
            (0, 1, false),
            (1, 0, false),
            (1, 1, true),
            (1, 2, false),
            (8, 8, true),
            (9, 8, false),
        ] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, "catalog-fence-count-matrix.db");
            create_catalog_fence_count_source(&source, actual);
            super::MANAGED_CATALOG_FENCE_RANGE_ROWS.with(|count| count.set(0));
            let result = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                    let relation = catalog_fence_relation_id(planner, "assertion_05");
                    planner.require_exact_row_count(relation, expected)
                });
            assert_eq!(
                result.is_ok(),
                accepted,
                "actual={actual} expected={expected}"
            );
            let rows = super::MANAGED_CATALOG_FENCE_RANGE_ROWS.with(|count| count.get());
            assert!(
                rows <= expected.saturating_add(1) as usize,
                "unbounded rows for actual={actual} expected={expected}"
            );
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn catalog_fence_large_unrelated_relation_does_not_expand_target_probe() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-unrelated.db");
        create_catalog_fence_count_source(&source, 1);
        let database = new_cozo_sqlite(&source).unwrap();
        let input = (0..2048)
            .map(|id| format!("[{id}]"))
            .collect::<Vec<_>>()
            .join(",");
        database
            .run_script(
                &format!("?[id] <- [{input}] :put assertion_06 {{id}}"),
                Default::default(),
                crate::ScriptMutability::Mutable,
            )
            .unwrap();
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        super::MANAGED_CATALOG_FENCE_RANGE_ROWS.with(|count| count.set(0));
        let _ = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                let relation = catalog_fence_relation_id(planner, "assertion_05");
                planner.require_exact_row_count(relation, 1)
            })
            .unwrap();
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_RANGE_ROWS.with(|count| count.get()),
            1
        );
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_range_bind_panic_unregisters_handler_and_withholds_evidence() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-range-panic.db");
        create_relation_assertion_source(&source);
        let before = fingerprint(&source);
        super::MANAGED_CATALOG_FENCE_PROGRESS_INSTALLS.with(|count| count.set(0));
        super::MANAGED_CATALOG_FENCE_PROGRESS_UNREGISTRATIONS.with(|count| count.set(0));
        super::MANAGED_CATALOG_FENCE_RANGE_PANIC_AFTER_BIND.with(|panic| panic.set(true));
        let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                    let relation = catalog_fence_relation_id(planner, "assertion_00");
                    planner.require_exact_row_count(relation, 8)
                });
        }));
        assert!(caught.is_err());
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_PROGRESS_INSTALLS.with(|count| count.get()),
            1
        );
        assert_eq!(
            super::MANAGED_CATALOG_FENCE_PROGRESS_UNREGISTRATIONS.with(|count| count.get()),
            1
        );
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn catalog_fence_light_failure_seams_withhold_evidence() {
        for seam in [
            "reset",
            "clear",
            "range-finalize",
            "point-finalize",
            "point-execution",
        ] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, "catalog-fence-light-failure.db");
            create_relation_assertion_source(&source);
            match seam {
                "reset" => {
                    super::MANAGED_CATALOG_FENCE_RANGE_RESET_FAILURE.with(|value| value.set(true))
                }
                "clear" => {
                    super::MANAGED_CATALOG_FENCE_RANGE_CLEAR_FAILURE.with(|value| value.set(true))
                }
                "range-finalize" => super::MANAGED_CATALOG_FENCE_RANGE_FINALIZE_CODE
                    .with(|value| value.set(Some(ffi::SQLITE_BUSY))),
                "point-finalize" => super::MANAGED_CATALOG_FENCE_POINT_FINALIZE_CODE
                    .with(|value| value.set(Some(ffi::SQLITE_BUSY))),
                "point-execution" => {
                    super::MANAGED_CATALOG_FENCE_POINT_FAILURE.with(|value| value.set(true))
                }
                _ => unreachable!(),
            }
            let result = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::V1, |planner| {
                    let relation = catalog_fence_relation_id(planner, "assertion_00");
                    if seam == "point-execution" || seam == "point-finalize" {
                        planner.require_string_pair(relation, "exact", "value")
                    } else {
                        planner.require_exact_row_count(relation, 8)
                    }
                });
            assert!(result.is_err(), "{seam} unexpectedly produced evidence");
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn catalog_fence_uses_one_cumulative_budget_for_catalog_and_assertions() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "catalog-fence-budget.db");
        create_relation_assertion_source(&source);
        let mut baseline = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let evidence = baseline
            .run_catalog_fence_with_budget_v1(
                ManagedCatalogFencePolicy::V1,
                super::ManagedProgressBudget {
                    progress_interval: 1,
                    max_callbacks: 1 << 20,
                    phase: super::ManagedProgressPhase::CatalogFence,
                },
                |planner| {
                    let relation = catalog_fence_relation_id(planner, "assertion_00");
                    planner.require_exact_row_count(relation, 8)
                },
            )
            .unwrap();
        let callbacks = evidence.progress_callbacks_used();
        baseline.close_and_verify().unwrap();

        let mut exhausted = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let error = match exhausted.run_catalog_fence_with_budget_v1(
            ManagedCatalogFencePolicy::V1,
            super::ManagedProgressBudget {
                progress_interval: 1,
                max_callbacks: callbacks,
                phase: super::ManagedProgressPhase::CatalogFence,
            },
            |planner| {
                let relation = catalog_fence_relation_id(planner, "assertion_00");
                planner.require_exact_row_count(relation, 8)
            },
        ) {
            Ok(_) => panic!("fresh cumulative budget unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("work budget"));
        assert!(exhausted.close_and_verify().is_err());
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_closed_source_policy_fingerprint_is_a_hard_pinned_literal_oracle() {
        let derived = derive_closed_source_policy_fingerprint_v1();
        assert_eq!(derived, super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1);
        assert_ne!(
            super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
            [0_u8; 32]
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_closed_source_identity_has_an_independent_hostile_kat() {
        let path_bytes = vec![b'/', b't', b'm', b'p', b'/', 0x80, b'-', b'd', b'b'];
        let supplied_path = PathBuf::from(std::ffi::OsString::from_vec(path_bytes.clone()));
        let main = |length| super::ManagedSqliteClosedMainFileIdentityV1 {
            device: 0x0102_0304_0506_0708,
            inode: 0x1112_1314_1516_1718,
            mode: 0x2122_2324,
            uid: 0x3132_3334,
            gid: 0x4142_4344,
            link_count: 1,
            length,
            mtime_seconds: -9,
            mtime_nanoseconds: 123_456_789,
            ctime_seconds: -17,
            ctime_nanoseconds: -123_456_789,
            _thread_bound: std::marker::PhantomData,
        };
        let absent = || super::ManagedSqliteClosedResidualV1 {
            present: false,
            metadata: None,
            sha256: None,
            _thread_bound: std::marker::PhantomData,
        };
        let present_empty = || super::ManagedSqliteClosedResidualV1 {
            present: true,
            metadata: Some(super::ManagedSqliteClosedResidualMetadataV1 {
                length: 0,
                device: 0x5152_5354_5556_5758,
                inode: 0x6162_6364_6566_6768,
                mode: 0x7172_7374,
                link_count: 1,
                uid: 0x1112_1314,
                gid: 0x2122_2324,
                rdev: 0x8182_8384_8586_8788,
                blocks: 0,
                block_size: 4_096,
                mtime_seconds: -33,
                mtime_nanoseconds: -44,
                ctime_seconds: 55,
                ctime_nanoseconds: 66,
                _thread_bound: std::marker::PhantomData,
            }),
            sha256: Some([
                227, 176, 196, 66, 152, 252, 28, 20, 154, 251, 244, 200, 153, 111, 185, 36, 39,
                174, 65, 228, 100, 155, 147, 76, 164, 149, 153, 27, 120, 82, 184, 85,
            ]),
            _thread_bound: std::marker::PhantomData,
        };

        let production = super::managed_closed_source_identity_fingerprint_v1(
            &supplied_path,
            ManagedSnapshotPolicy::V1,
            &main(0x9192_9394_9596_9798),
            &absent(),
            &present_empty(),
        )
        .unwrap();

        // Independent oracle: spell out every byte and frame here. Do not call
        // the production frame or main/residual encoders.
        let mut main_bytes = Vec::new();
        main_bytes.extend_from_slice(&0x0102_0304_0506_0708_u64.to_be_bytes());
        main_bytes.extend_from_slice(&0x1112_1314_1516_1718_u64.to_be_bytes());
        main_bytes.extend_from_slice(&0x2122_2324_u32.to_be_bytes());
        main_bytes.extend_from_slice(&0x3132_3334_u32.to_be_bytes());
        main_bytes.extend_from_slice(&0x4142_4344_u32.to_be_bytes());
        main_bytes.extend_from_slice(&1_u64.to_be_bytes());
        main_bytes.extend_from_slice(&0x9192_9394_9596_9798_u64.to_be_bytes());
        main_bytes.extend_from_slice(&(-9_i64).to_be_bytes());
        main_bytes.extend_from_slice(&123_456_789_i64.to_be_bytes());
        main_bytes.extend_from_slice(&(-17_i64).to_be_bytes());
        main_bytes.extend_from_slice(&(-123_456_789_i64).to_be_bytes());
        let wal_bytes = [0x00_u8];
        let mut shm_bytes = Vec::new();
        shm_bytes.push(0x01);
        shm_bytes.extend_from_slice(&0_u64.to_be_bytes());
        shm_bytes.extend_from_slice(&0x5152_5354_5556_5758_u64.to_be_bytes());
        shm_bytes.extend_from_slice(&0x6162_6364_6566_6768_u64.to_be_bytes());
        shm_bytes.extend_from_slice(&0x7172_7374_u32.to_be_bytes());
        shm_bytes.extend_from_slice(&1_u64.to_be_bytes());
        shm_bytes.extend_from_slice(&0x1112_1314_u32.to_be_bytes());
        shm_bytes.extend_from_slice(&0x2122_2324_u32.to_be_bytes());
        shm_bytes.extend_from_slice(&0x8182_8384_8586_8788_u64.to_be_bytes());
        shm_bytes.extend_from_slice(&0_u64.to_be_bytes());
        shm_bytes.extend_from_slice(&4_096_u64.to_be_bytes());
        shm_bytes.extend_from_slice(&(-33_i64).to_be_bytes());
        shm_bytes.extend_from_slice(&(-44_i64).to_be_bytes());
        shm_bytes.extend_from_slice(&55_i64.to_be_bytes());
        shm_bytes.extend_from_slice(&66_i64.to_be_bytes());
        shm_bytes.extend_from_slice(&[
            227, 176, 196, 66, 152, 252, 28, 20, 154, 251, 244, 200, 153, 111, 185, 36, 39, 174,
            65, 228, 100, 155, 147, 76, 164, 149, 153, 27, 120, 82, 184, 85,
        ]);
        let source_policy = [
            196, 62, 40, 190, 206, 23, 138, 178, 241, 29, 93, 139, 70, 3, 194, 169, 218, 247, 88,
            214, 192, 121, 7, 229, 80, 220, 204, 131, 121, 109, 100, 154,
        ];
        let mut independent = Sha256::new();
        independent.update(b"mnestic.managed-sqlite-closed-source-identity.transcript.v1\0");
        independent.update([0x01]);
        independent.update(6_u64.to_be_bytes());
        for (name, value) in [
            (
                b"source-policy-fingerprint".as_slice(),
                source_policy.as_slice(),
            ),
            (
                b"managed-snapshot-policy".as_slice(),
                b"mnestic.managed-snapshot-policy.v1".as_slice(),
            ),
            (
                b"supplied-absolute-unix-path".as_slice(),
                path_bytes.as_slice(),
            ),
            (b"main-identity".as_slice(), main_bytes.as_slice()),
            (b"wal-residual".as_slice(), wal_bytes.as_slice()),
            (b"shm-residual".as_slice(), shm_bytes.as_slice()),
        ] {
            independent.update(u64::try_from(name.len()).unwrap().to_be_bytes());
            independent.update(name);
            independent.update(u64::try_from(value.len()).unwrap().to_be_bytes());
            independent.update(value);
        }
        independent.update([0xff]);
        independent.update(1_u64.to_be_bytes());
        let independent: [u8; 32] = independent.finalize().into();
        assert_eq!(production, independent);
        assert_eq!(
            production,
            [
                134, 3, 7, 96, 82, 79, 207, 41, 158, 225, 204, 64, 61, 143, 164, 206, 96, 59, 15,
                218, 249, 97, 203, 196, 119, 192, 165, 104, 71, 13, 216, 103,
            ]
        );

        let mutated_main = super::managed_closed_source_identity_fingerprint_v1(
            &supplied_path,
            ManagedSnapshotPolicy::V1,
            &main(0x9192_9394_9596_9799),
            &absent(),
            &present_empty(),
        )
        .unwrap();
        let swapped_roles = super::managed_closed_source_identity_fingerprint_v1(
            &supplied_path,
            ManagedSnapshotPolicy::V1,
            &main(0x9192_9394_9596_9798),
            &present_empty(),
            &absent(),
        )
        .unwrap();
        let both_absent = super::managed_closed_source_identity_fingerprint_v1(
            &supplied_path,
            ManagedSnapshotPolicy::V1,
            &main(0x9192_9394_9596_9798),
            &absent(),
            &absent(),
        )
        .unwrap();
        let changed_spelling = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b't', b'm', b'p', b'/', 0x80, b'-', b'd', b'b', b'.',
        ]));
        let changed_path = super::managed_closed_source_identity_fingerprint_v1(
            &changed_spelling,
            ManagedSnapshotPolicy::V1,
            &main(0x9192_9394_9596_9798),
            &absent(),
            &present_empty(),
        )
        .unwrap();
        assert_ne!(production, mutated_main);
        assert_ne!(production, swapped_roles);
        assert_ne!(production, both_absent);
        assert_ne!(production, changed_path);
    }

    #[test]
    fn managed_assertion_transcript_has_an_independent_kat_and_exact_caps() {
        let count_kind = [0_u8];
        let exact_kind = [1_u8];
        let one_of_kind = [2_u8];
        let id = 42_u64.to_be_bytes();
        let count = 8_u64.to_be_bytes();
        let selected = [1_u8];
        let count_fields = [count_kind.as_slice(), id.as_slice(), count.as_slice()];
        let exact_fields = [
            exact_kind.as_slice(),
            id.as_slice(),
            b"exact".as_slice(),
            b"value".as_slice(),
        ];
        let one_of_fields = [
            one_of_kind.as_slice(),
            b"generation".as_slice(),
            id.as_slice(),
            b"marker".as_slice(),
            b"old".as_slice(),
            b"new".as_slice(),
            selected.as_slice(),
        ];

        let mut transcript = super::ManagedAssertionTranscriptBuilderV1::new();
        for fields in [
            count_fields.as_slice(),
            exact_fields.as_slice(),
            one_of_fields.as_slice(),
        ] {
            let reservation = transcript.reserve(fields).unwrap();
            transcript.commit(reservation, fields).unwrap();
        }
        let production = transcript.finish().unwrap();

        let mut independent = Sha256::new();
        independent.update(super::MANAGED_ASSERTION_TRANSCRIPT_DOMAIN_V1);
        for fields in [
            count_fields.as_slice(),
            exact_fields.as_slice(),
            one_of_fields.as_slice(),
        ] {
            independent.update([0x01]);
            for field in fields {
                independent.update(u64::try_from(field.len()).unwrap().to_be_bytes());
                independent.update(field);
            }
            independent.update(u64::try_from(fields.len()).unwrap().to_be_bytes());
        }
        independent.update([0xff]);
        independent.update(3_u64.to_be_bytes());
        let independently_derived: [u8; 32] = independent.finalize().into();
        assert_eq!(production, independently_derived);
        assert_eq!(
            production,
            [
                139, 232, 32, 52, 96, 2, 141, 253, 1, 128, 17, 67, 50, 39, 203, 210, 94, 61, 125,
                236, 30, 47, 148, 82, 46, 133, 255, 185, 170, 187, 31, 209,
            ]
        );

        let maximum = vec![b'x'; super::MANAGED_ASSERTION_STRING_BYTE_LIMIT];
        let max_fields = [
            one_of_kind.as_slice(),
            maximum.as_slice(),
            id.as_slice(),
            maximum.as_slice(),
            maximum.as_slice(),
            maximum.as_slice(),
            selected.as_slice(),
        ];
        let mut maximum_transcript = super::ManagedAssertionTranscriptBuilderV1::new();
        for _ in 0..super::MANAGED_ASSERTION_LIMIT {
            let reservation = maximum_transcript.reserve(&max_fields).unwrap();
            maximum_transcript.commit(reservation, &max_fields).unwrap();
        }
        assert_eq!(maximum_transcript.entry_count, 8);
        assert_eq!(maximum_transcript.framed_bytes, 8_792);
        assert_eq!(
            maximum_transcript.framed_bytes + 9,
            super::MANAGED_ASSERTION_TRANSCRIPT_BYTE_LIMIT
        );
        maximum_transcript.finish().unwrap();

        let mut entry_capped = super::ManagedAssertionTranscriptBuilderV1::new();
        for _ in 0..super::MANAGED_ASSERTION_LIMIT {
            let reservation = entry_capped.reserve(&count_fields).unwrap();
            entry_capped.commit(reservation, &count_fields).unwrap();
        }
        assert!(entry_capped.reserve(&count_fields).is_err());
        let oversized = vec![0_u8; super::MANAGED_ASSERTION_TRANSCRIPT_BYTE_LIMIT as usize];
        assert!(
            super::ManagedAssertionTranscriptBuilderV1::new()
                .reserve(&[oversized.as_slice()])
                .is_err()
        );
    }

    #[test]
    fn managed_covering_plan_treats_typed_auxiliary_as_opaque_telemetry() {
        assert!(super::managed_covering_index_plan_tree_is_admitted_v1(
            3, 0, 213
        ));
        assert!(super::managed_covering_index_plan_tree_is_admitted_v1(
            0,
            0,
            i64::MIN
        ));
        assert!(!super::managed_covering_index_plan_tree_is_admitted_v1(
            -1, 0, 213
        ));
        assert!(!super::managed_covering_index_plan_tree_is_admitted_v1(
            3, 1, 213
        ));
    }

    #[test]
    fn managed_physical_commitments_have_independent_known_answer_vectors() {
        const TABLE_KAT_V1: [u8; 32] = [
            78, 124, 74, 2, 13, 180, 103, 118, 3, 31, 30, 88, 214, 203, 195, 248, 129, 64, 65, 210,
            10, 222, 177, 159, 55, 237, 128, 95, 229, 47, 94, 2,
        ];
        const INDEX_KAT_V1: [u8; 32] = [
            212, 130, 192, 253, 114, 87, 46, 246, 231, 239, 157, 83, 89, 167, 137, 125, 144, 179,
            90, 117, 125, 163, 43, 60, 138, 155, 177, 145, 218, 12, 5, 21,
        ];

        let mut table = Sha256::new();
        table.update(super::MANAGED_PHYSICAL_TABLE_COMMITMENT_DOMAIN_V1);
        super::update_table_commitment(&mut table, -2, &[0x00, 0xff], &[0x10]).unwrap();
        super::update_table_commitment(&mut table, 5, &[], &[0xaa, 0xbb]).unwrap();
        super::finish_physical_commitment(&mut table, 2).unwrap();
        let table: [u8; 32] = table.finalize().into();
        assert_eq!(table, TABLE_KAT_V1);

        let mut index = Sha256::new();
        index.update(super::MANAGED_PHYSICAL_INDEX_COMMITMENT_DOMAIN_V1);
        super::update_index_commitment(&mut index, &[], -2).unwrap();
        super::update_index_commitment(&mut index, &[0x00, 0xff], 5).unwrap();
        super::finish_physical_commitment(&mut index, 2).unwrap();
        let index: [u8; 32] = index.finalize().into();
        assert_eq!(index, INDEX_KAT_V1);
    }

    #[test]
    fn managed_linked_runtime_identity_framing_has_a_known_answer() {
        const RUNTIME_IDENTITY_KAT_V1: [u8; 32] = [
            162, 100, 220, 188, 122, 127, 185, 213, 230, 159, 145, 38, 186, 231, 137, 7, 180, 228,
            137, 111, 161, 49, 154, 203, 156, 131, 22, 209, 70, 107, 87, 12,
        ];
        let derived = super::managed_linked_sqlite_runtime_identity_fingerprint(
            3_045_003,
            b"3.45.3",
            b"2024-04-15 13:34:05 8653b758870e6ef0c98d46b3ace27849054af85da891eb121e9aaa537f1e8355",
        )
        .unwrap();
        assert_eq!(derived, RUNTIME_IDENTITY_KAT_V1);
    }

    #[test]
    fn managed_catalog_blob_gate_handles_empty_blobs_and_rejects_non_blob_cells() {
        let connection = Connection::open(":memory:").unwrap();
        let empty =
            prepare_test_statement(&connection, b"SELECT length(x''), length(x''), x'', x'';\0");
        assert_eq!(unsafe { ffi::sqlite3_step(empty.raw) }, ffi::SQLITE_ROW);
        let (key, value) = managed_catalog_blob_cells(&empty, 0).unwrap();
        // SAFETY: the statement is still positioned on the same SQLITE_ROW.
        assert!(unsafe { key.as_slice() }.is_empty());
        // SAFETY: the statement is still positioned on the same SQLITE_ROW.
        assert!(unsafe { value.as_slice() }.is_empty());
        drop(empty);

        for sql in [
            b"SELECT length(CAST(x'00' AS TEXT)), length(x'00'), CAST(x'00' AS TEXT), x'00';\0"
                .as_slice(),
            b"SELECT length(x'00'), length(CAST(x'00' AS TEXT)), x'00', CAST(x'00' AS TEXT);\0"
                .as_slice(),
        ] {
            let statement = prepare_test_statement(&connection, sql);
            assert_eq!(unsafe { ffi::sqlite3_step(statement.raw) }, ffi::SQLITE_ROW);
            let error = managed_catalog_blob_cells(&statement, 0)
                .err()
                .expect("non-BLOB cell must fail");
            assert!(error.to_string().contains("non-BLOB"));
        }
    }

    #[test]
    fn managed_catalog_blob_gate_checks_caps_before_requesting_blob_pointers() {
        let connection = Connection::open(":memory:").unwrap();
        let cases = [
            (
                b"SELECT length(zeroblob(65537)), length(x''), zeroblob(65537), x'';\0".as_slice(),
                0,
                "key exceeded",
            ),
            (
                b"SELECT length(x''), length(zeroblob(4194305)), x'', zeroblob(4194305);\0"
                    .as_slice(),
                0,
                "value exceeded",
            ),
            (
                b"SELECT length(x'00'), length(x''), x'00', x'';\0".as_slice(),
                MANAGED_CATALOG_CUMULATIVE_RAW_LIMIT,
                "cumulative byte boundary",
            ),
        ];
        for (sql, cumulative, expected) in cases {
            let statement = prepare_test_statement(&connection, sql);
            assert_eq!(unsafe { ffi::sqlite3_step(statement.raw) }, ffi::SQLITE_ROW);
            let before = MANAGED_CATALOG_BLOB_POINTER_REQUESTS.with(|requests| requests.get());
            let message = managed_catalog_blob_cells(&statement, cumulative)
                .err()
                .expect("cap+1 must fail")
                .to_string();
            let after = MANAGED_CATALOG_BLOB_POINTER_REQUESTS.with(|requests| requests.get());
            assert_eq!(after, before, "rejected row requested a blob pointer");
            assert!(
                message.contains(expected),
                "unexpected diagnostic: {message}"
            );
            assert!(message.len() < 192);
        }
        assert_eq!(MAX_ENCODED_KEY_BYTES, 65_536);
        assert_eq!(MANAGED_CATALOG_VALUE_LIMIT, 4_194_304);
        let canonical = checked_catalog_total(
            MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT,
            1,
            0,
            MANAGED_CATALOG_CUMULATIVE_CANONICAL_LIMIT,
            "canonical",
        )
        .expect_err("canonical cumulative cap+1 must fail");
        assert!(canonical.to_string().contains("canonical catalog"));
    }

    #[test]
    fn managed_primary_index_catalog_rejects_cumulative_raw_overflow() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-cumulative.db");
        create_populated_managed_cozo_source(&source);
        for (key, value) in relation_rows(&source) {
            let mut handle = RelationHandle::decode(&value).unwrap();
            handle.description = "d".repeat(350_000).into();
            let encoded = encode_test_catalog_record(&handle, CatalogCodec::StructMapV1);
            assert!(encoded.len() < MANAGED_CATALOG_VALUE_LIMIT);
            update_raw_value(&source, &key, &encoded);
        }

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let message = primary_index_catalog_error(&mut reader).to_string();
        assert!(message.contains("raw catalog exceeded its cumulative byte boundary"));
        assert!(message.len() < 192);
        assert!(reader.close_and_verify().is_err());
    }

    #[test]
    fn managed_primary_index_catalog_rejects_malformed_keys_and_values_code_only() {
        for variant in ["key", "value"] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("managed-malformed-{variant}.db"));
            create_populated_managed_cozo_source(&source);
            let (key, value) = relation_rows(&source).remove(0);
            if variant == "key" {
                let mut malformed = vec![0_u8; 8];
                malformed.push(0xff);
                replace_raw_row(&source, &key, &malformed, &value);
            } else {
                update_raw_value(&source, &key, &[0xc1]);
            }

            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            let message = primary_index_catalog_error(&mut reader).to_string();
            assert!(message.contains(if variant == "key" {
                "key rejected by exact decoder"
            } else {
                "value rejected by exact codec"
            }));
            assert!(!message.contains("ff"));
            assert!(!message.contains("c1"));
            assert!(message.len() < 192);
            assert!(reader.close_and_verify().is_err());
        }
    }

    #[test]
    fn managed_catalog_attacker_text_never_enters_diagnostics() {
        const SENTINEL: &str = "ATTACKER_CATALOG_SENTINEL";
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-attacker-catalog.db");
        create_populated_managed_cozo_source(&source);
        let (key, _) = relation_rows(&source).remove(0);
        let attacker_name = SENTINEL.repeat(256);
        let attacker_key = vec![DataValue::from(attacker_name)].encode_as_key(RelationId::SYSTEM);
        replace_raw_row(&source, &key, &attacker_key, &[0xc1]);

        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let message = primary_index_catalog_error(&mut reader).to_string();
        assert!(!message.contains(SENTINEL));
        assert!(message.len() < 192);
        assert!(reader.close_and_verify().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn managed_catalog_close_detects_rename_after_pinned_observation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-observed-rename.db");
        let moved = test_path(&directory, "managed-observed-moved.db");
        create_populated_managed_cozo_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.inspect_primary_index_catalog_v1().unwrap();
        fs::rename(&source, &moved).unwrap();
        let error = reader
            .close_and_verify()
            .expect_err("rename after observation must withhold close success");
        assert!(error.to_string().contains("source"));
        assert!(!source.exists());
        assert_no_sidecars(&source);
    }

    #[test]
    fn snapshot_source_rejects_aggregate_schema_work_before_sqlite_parsing() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "schema-count-bomb.db");
        let connection = Connection::open(&source).unwrap();
        connection.execute("PRAGMA page_size=65536;").unwrap();
        for index in 0..=super::MAX_SNAPSHOT_SCHEMA_CELLS {
            connection
                .execute(format!("CREATE TABLE schema_bomb_{index} (value BLOB);"))
                .unwrap();
        }
        drop(connection);
        let before = fingerprint(&source);
        assert_eq!(u16::from_be_bytes([before.0[16], before.0[17]]), 1);
        assert_eq!(before.0[100], 0x0d);
        assert_eq!(
            u16::from_be_bytes([before.0[103], before.0[104]]),
            super::MAX_SNAPSHOT_SCHEMA_CELLS + 1
        );

        let outcome = std::panic::catch_unwind(|| ExistingSqliteSnapshotSource::open(&source));
        let error = match outcome.expect("schema-count bomb must not panic") {
            Ok(_) => panic!("schema-count bomb passed the pre-SQLite fuse"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("schema"));
        assert!(message.len() < 192);
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_reader_does_not_census_or_decode_cozo_rows() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "opaque-rows.db");
        create_managed_cozo_source(&source);
        let connection = Connection::open(&source).unwrap();
        connection
            .execute("INSERT INTO cozo (k, v) VALUES (x'c1', x'ffffffff');")
            .unwrap();
        drop(connection);
        let before = fingerprint(&source);

        let reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.close_and_verify().unwrap();

        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_reader_rejects_every_extra_persistent_schema_object() {
        let extras = [
            ("table", "CREATE TABLE extra (value BLOB);"),
            ("index", "CREATE INDEX extra_index ON cozo (v);"),
            ("view", "CREATE VIEW extra_view AS SELECT k FROM cozo;"),
            (
                "trigger",
                "CREATE TRIGGER extra_trigger AFTER INSERT ON cozo BEGIN SELECT 1; END;",
            ),
        ];

        for (kind, ddl) in extras {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("extra-{kind}.db"));
            create_managed_cozo_source(&source);
            let connection = Connection::open(&source).unwrap();
            connection.execute(ddl).unwrap();
            drop(connection);
            let before = fingerprint(&source);

            let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();
            let error = match guarded.into_managed_reader(ManagedSnapshotPolicy::V1) {
                Ok(_) => panic!("managed reader admitted an extra {kind}"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("exact schema objects"));
            assert_eq!(fingerprint(&source), before);
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn managed_reader_rejects_altered_column_type_and_collation() {
        let variants = [
            (
                "wrong-type",
                "CREATE TABLE cozo\n        (\n            k BLOB primary key,\n            v TEXT\n        );",
            ),
            (
                "wrong-collation",
                "CREATE TABLE cozo\n        (\n            k BLOB COLLATE NOCASE primary key,\n            v BLOB\n        );",
            ),
        ];

        for (kind, ddl) in variants {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("{kind}.db"));
            create_schema_source(&source, ddl);
            let before = fingerprint(&source);

            let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();
            let error = match guarded.into_managed_reader(ManagedSnapshotPolicy::V1) {
                Ok(_) => panic!("managed reader admitted {kind}"),
                Err(error) => error,
            };
            let message = error.to_string();
            assert!(message.contains("exact schema objects"));
            assert!(!message.contains(ddl));
            assert!(message.len() < 192);
            assert_eq!(fingerprint(&source), before);
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn managed_reader_rejects_aliased_table_and_index_roots() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "aliased-roots.db");
        create_managed_cozo_source(&source);
        let connection = Connection::open(&source).unwrap();
        let mut observed = -1;
        assert_eq!(
            unsafe {
                ffi::sqlite3_db_config(
                    connection.as_raw(),
                    ffi::SQLITE_DBCONFIG_DEFENSIVE,
                    0,
                    &mut observed,
                )
            },
            ffi::SQLITE_OK
        );
        assert_eq!(observed, 0);
        assert_eq!(
            unsafe {
                ffi::sqlite3_db_config(
                    connection.as_raw(),
                    ffi::SQLITE_DBCONFIG_WRITABLE_SCHEMA,
                    1,
                    &mut observed,
                )
            },
            ffi::SQLITE_OK
        );
        assert_eq!(observed, 1);
        connection
            .execute(
                "UPDATE sqlite_master \
                 SET rootpage = (SELECT rootpage FROM sqlite_master \
                                 WHERE type = 'table' AND name = 'cozo') \
                 WHERE type = 'index' AND name = 'sqlite_autoindex_cozo_1';",
            )
            .unwrap();
        assert_eq!(
            unsafe {
                ffi::sqlite3_db_config(
                    connection.as_raw(),
                    ffi::SQLITE_DBCONFIG_WRITABLE_SCHEMA,
                    0,
                    &mut observed,
                )
            },
            ffi::SQLITE_OK
        );
        assert_eq!(observed, 0);
        drop(connection);
        let before = fingerprint(&source);

        let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();
        let error = match guarded.into_managed_reader(ManagedSnapshotPolicy::V1) {
            Ok(_) => panic!("managed reader admitted aliased table and index roots"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exact schema objects"));
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_reader_rejects_an_existing_attachment() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "attached.db");
        create_managed_cozo_source(&source);
        let before = fingerprint(&source);
        let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();
        guarded.attach_in_memory_for_managed_reader_test().unwrap();

        let error = match guarded.into_managed_reader(ManagedSnapshotPolicy::V1) {
            Ok(_) => panic!("managed reader admitted an attached database"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("main-only attachment set"));
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_schema_rejection_never_copies_attacker_text() {
        const SENTINEL: &str = "ATTACKER_SCHEMA_SENTINEL";

        let directory = tempdir().unwrap();
        let source = test_path(&directory, "attacker-schema.db");
        let attacker_name = SENTINEL.repeat(100);
        let ddl = format!("CREATE TABLE \"{attacker_name}\" (value BLOB);");
        create_schema_source(&source, &ddl);
        let before = fingerprint(&source);

        let outcome = std::panic::catch_unwind(|| {
            let guarded = ExistingSqliteSnapshotSource::open(&source)?;
            match guarded.into_managed_reader(ManagedSnapshotPolicy::V1) {
                Ok(_) => Ok(()),
                Err(error) => Err(error),
            }
        });
        let result = outcome.expect("adversarial schema must not panic");
        let error = result.expect_err("adversarial schema must be rejected");
        let message = error.to_string();
        assert!(message.len() < 192);
        assert!(!message.contains(SENTINEL));
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_preparation_failure_is_code_only_and_bounded() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "prepare-failure.db");
        create_managed_cozo_source(&source);
        let before = fingerprint(&source);
        let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let error = managed_boolean_check(
            &guarded.connection,
            b"SELECT (\0",
            "synthetic preparation failure",
        )
        .expect_err("invalid fixed SQL must fail during preparation");
        let message = error.to_string();
        assert!(message.contains("SQLite"));
        assert!(message.contains("code"));
        assert!(message.len() < 192);
        drop(guarded);
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_close_failure_withholds_seal_and_arms_deferred_cleanup() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "close-busy.db");
        create_managed_cozo_source(&source);
        let before = fingerprint(&source);
        let reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();

        let destroyed = Arc::new(AtomicBool::new(false));
        let destructor_context = Box::into_raw(Box::new(Arc::clone(&destroyed))).cast();
        let registration = unsafe {
            ffi::sqlite3_create_function_v2(
                reader.connection.as_ref().unwrap().as_raw(),
                CLEANUP_PROBE_FUNCTION_NAME.as_ptr().cast(),
                0,
                ffi::SQLITE_UTF8,
                destructor_context,
                None,
                None,
                None,
                Some(mark_sqlite_connection_destroyed),
            )
        };
        // SQLite invokes the registered destructor even when registration
        // fails, so ownership has transferred regardless of this result.
        assert_eq!(registration, ffi::SQLITE_OK);

        let mut statement = std::ptr::null_mut();
        let code = unsafe {
            ffi::sqlite3_prepare_v2(
                reader.connection.as_ref().unwrap().as_raw(),
                CLEANUP_PROBE_STATEMENT.as_ptr().cast(),
                -1,
                &mut statement,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(code, ffi::SQLITE_OK);
        assert!(!statement.is_null());

        let error = match reader.close_and_verify() {
            Ok(_) => panic!("strict close issued readiness evidence with a live statement"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("failed to close"));
        assert!(message.contains("deferred cleanup was armed"));
        assert!(message.len() < 192);
        // close_v2 immediately tears down connection registrations while the
        // live statement retains only SQLite's documented zombie allocation.
        assert!(destroyed.load(Ordering::SeqCst));
        assert_eq!(unsafe { ffi::sqlite3_finalize(statement) }, ffi::SQLITE_OK);
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn source_verification_runs_final_main_check_after_residual_failure() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source-bracket.db");
        create_managed_cozo_source(&source);
        let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"injected main verification drift")
            .unwrap();
        super::MANAGED_SOURCE_MAIN_VERIFY_CALLS.with(|calls| calls.set(0));
        super::MANAGED_SOURCE_RESIDUAL_VERIFY_CALLS.with(|calls| calls.set(0));
        super::MANAGED_SOURCE_RESIDUAL_TEST_FAILURE.with(|failure| failure.set(true));
        let error = super::verify_snapshot_source(
            &guarded.main_file,
            &guarded.residuals,
            &guarded.path,
            "injected source bracket",
        )
        .expect_err("injected residual failure unexpectedly passed");
        let message = error.to_string();
        assert!(message.contains("durable identity changed"));
        assert!(message.contains("injected managed SQLite residual"));
        assert!(message.contains("final snapshot main-file"));
        assert_eq!(
            super::MANAGED_SOURCE_MAIN_VERIFY_CALLS.with(|calls| calls.get()),
            2,
            "residual failure skipped the trailing main-file verification"
        );
        assert_eq!(
            super::MANAGED_SOURCE_RESIDUAL_VERIFY_CALLS.with(|calls| calls.get()),
            1,
            "initial main-file failure skipped residual verification"
        );
        drop(guarded);
        assert_no_sidecars(&source);
    }

    #[test]
    fn managed_close_rejects_hidden_and_stale_tracked_transactions_after_cleanup() {
        let directory = tempdir().unwrap();
        let hidden_source = test_path(&directory, "hidden-transaction.db");
        create_managed_cozo_source(&hidden_source);
        let hidden_before = fingerprint(&hidden_source);
        let hidden = ExistingSqliteSnapshotSource::open(&hidden_source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let hidden_destroyed = install_connection_destruction_probe(&hidden);
        super::managed_exec_fixed(
            hidden.connection.as_ref().unwrap(),
            MANAGED_BEGIN_READ_SQL,
            "test hidden transaction",
        )
        .unwrap();
        assert!(!hidden.transaction_active);
        assert_eq!(
            unsafe { ffi::sqlite3_get_autocommit(hidden.connection.as_ref().unwrap().as_raw()) },
            0
        );
        let rollback_calls = Cell::new(0_usize);
        assert_eq!(
            unsafe {
                ffi::sqlite3_set_authorizer(
                    hidden.connection.as_ref().unwrap().as_raw(),
                    Some(count_sqlite_rollback_authorizer),
                    std::ptr::from_ref(&rollback_calls).cast_mut().cast(),
                )
            },
            ffi::SQLITE_OK
        );
        let hidden_error = hidden
            .close_and_verify()
            .expect_err("untracked live transaction passed close");
        assert!(
            hidden_error
                .to_string()
                .contains("tracked transaction state")
        );
        assert_eq!(
            rollback_calls.get(),
            1,
            "hidden transaction was not explicitly rolled back"
        );
        assert!(hidden_destroyed.load(Ordering::SeqCst));
        assert_eq!(fingerprint(&hidden_source), hidden_before);

        let stale_source = test_path(&directory, "stale-tracked-transaction.db");
        create_populated_managed_cozo_source(&stale_source);
        let stale_before = fingerprint(&stale_source);
        let mut stale = ExistingSqliteSnapshotSource::open(&stale_source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        stale.inspect_primary_index_catalog_v1().unwrap();
        let stale_destroyed = install_connection_destruction_probe(&stale);
        super::managed_exec_fixed(
            stale.connection.as_ref().unwrap(),
            MANAGED_ROLLBACK_READ_SQL,
            "test external rollback",
        )
        .unwrap();
        assert!(stale.transaction_active);
        assert_eq!(
            unsafe { ffi::sqlite3_get_autocommit(stale.connection.as_ref().unwrap().as_raw()) },
            1
        );
        let stale_error = stale
            .close_and_verify()
            .expect_err("stale tracked transaction passed close");
        let stale_message = stale_error.to_string();
        assert!(stale_message.contains("tracked transaction state"));
        assert!(stale_message.contains("rollback"));
        assert!(stale_destroyed.load(Ordering::SeqCst));
        assert_eq!(fingerprint(&stale_source), stale_before);
        assert_no_sidecars(&hidden_source);
        assert_no_sidecars(&stale_source);
    }

    #[test]
    fn managed_close_does_not_let_strict_close_hide_a_failed_rollback() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "rollback-denied.db");
        create_populated_managed_cozo_source(&source);
        let before = fingerprint(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.inspect_primary_index_catalog_v1().unwrap();
        let destroyed = install_connection_destruction_probe(&reader);
        let authorizer = unsafe {
            ffi::sqlite3_set_authorizer(
                reader.connection.as_ref().unwrap().as_raw(),
                Some(deny_sqlite_transaction_authorizer),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(authorizer, ffi::SQLITE_OK);
        let error = reader
            .close_and_verify()
            .expect_err("implicit close-time rollback hid an explicit rollback failure");
        let message = error.to_string();
        assert!(message.contains("rollback"));
        assert!(message.contains("autocommit"));
        assert!(destroyed.load(Ordering::SeqCst));
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn managed_closed_audit_combines_operation_transaction_close_and_source_failures() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "combined-close-failures.db");
        create_relation_assertion_source(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        reader.inspect_primary_index_catalog_v1().unwrap();
        super::managed_exec_fixed(
            reader.connection.as_ref().unwrap(),
            MANAGED_ROLLBACK_READ_SQL,
            "test external rollback",
        )
        .unwrap();

        let mut statement = std::ptr::null_mut();
        let prepare = unsafe {
            ffi::sqlite3_prepare_v2(
                reader.connection.as_ref().unwrap().as_raw(),
                CLEANUP_PROBE_STATEMENT.as_ptr().cast(),
                -1,
                &mut statement,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(prepare, ffi::SQLITE_OK);
        assert!(!statement.is_null());
        OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"combined post-open drift")
            .unwrap();

        let error = closed_audit_error(
            reader.close_with_relation_assertions_v1(|_| Ok(())),
            "combined lifecycle failures yielded a closed audit",
        );
        let message = error.to_string();
        for expected in [
            "pinned READ transaction",
            "poisoned",
            "tracked transaction state",
            "rollback",
            "strict close",
            "deferred cleanup was armed",
            "durable identity changed",
        ] {
            assert!(
                message.contains(expected),
                "combined diagnostic omitted {expected:?}: {message}"
            );
        }
        assert_eq!(unsafe { ffi::sqlite3_finalize(statement) }, ffi::SQLITE_OK);
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn managed_closed_audit_withholds_evidence_for_path_link_and_residual_drift() {
        for drift in ["append", "rename", "replacement", "hardlink", "new-wal"] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("closed-drift-{drift}.db"));
            let auxiliary = test_path(&directory, &format!("closed-drift-{drift}-aux.db"));
            create_relation_assertion_source(&source);
            let reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            match drift {
                "append" => OpenOptions::new()
                    .append(true)
                    .open(&source)
                    .unwrap()
                    .write_all(b"post-open append")
                    .unwrap(),
                "rename" => fs::rename(&source, &auxiliary).unwrap(),
                "replacement" => {
                    fs::rename(&source, &auxiliary).unwrap();
                    create_relation_assertion_source(&source);
                }
                "hardlink" => fs::hard_link(&source, &auxiliary).unwrap(),
                "new-wal" => {
                    fs::File::create(sidecar_path(&source, "-wal")).unwrap();
                }
                _ => unreachable!(),
            }
            let error = closed_audit_error(
                reader.close_with_relation_assertions_v1(|_| Ok(())),
                "source drift yielded a closed audit",
            );
            let message = error.to_string();
            assert!(
                message.contains("source")
                    || message.contains("identity")
                    || message.contains("residual"),
                "unexpected {drift} diagnostic: {message}"
            );

            if drift == "hardlink" {
                fs::remove_file(&auxiliary).unwrap();
            }
            if drift == "new-wal" {
                fs::remove_file(sidecar_path(&source, "-wal")).unwrap();
            }
            if source.exists() {
                assert_no_sidecars(&source);
            }
            if auxiliary.exists() {
                assert_no_sidecars(&auxiliary);
            }
        }
    }

    #[test]
    fn every_assertion_completion_failure_consumes_and_closes_the_reader() {
        for failure in ["callback", "assertion", "finalize", "budget"] {
            let directory = tempdir().unwrap();
            let source = test_path(&directory, &format!("closed-{failure}.db"));
            create_relation_assertion_source(&source);
            let before = fingerprint(&source);
            let mut reader = ExistingSqliteSnapshotSource::open(&source)
                .unwrap()
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .unwrap();
            let destroyed = install_connection_destruction_probe(&reader);
            let error = match failure {
                "callback" => closed_audit_error(
                    reader.close_with_relation_assertions_v1(|_| {
                        Err(miette::miette!("injected closed-audit callback failure"))
                    }),
                    "callback failure yielded an audit",
                ),
                "assertion" => closed_audit_error(
                    reader.close_with_relation_assertions_v1(|planner| {
                        let relation_id = assertion_relation_id(planner, "assertion_00");
                        planner.require_exact_row_count(relation_id, 7)
                    }),
                    "assertion failure yielded an audit",
                ),
                "finalize" => {
                    super::MANAGED_ASSERTION_TEST_FINALIZE_CODE
                        .with(|injected| injected.set(Some(ffi::SQLITE_ERROR)));
                    let result = reader.close_with_relation_assertions_v1(|_| Ok(()));
                    super::MANAGED_ASSERTION_TEST_FINALIZE_CODE.with(|injected| injected.set(None));
                    closed_audit_error(result, "finalization failure yielded an audit")
                }
                "budget" => {
                    let operation = reader.run_relation_assertions_with_budget_v1(
                        super::ManagedProgressBudget {
                            progress_interval: 1,
                            max_callbacks: 1,
                            phase: super::ManagedProgressPhase::RelationAssertions,
                        },
                        |planner| {
                            let relation_id = assertion_relation_id(planner, "assertion_00");
                            planner.require_string_pair(relation_id, "exact", "value")
                        },
                    );
                    match reader.finish_close_preserving(operation) {
                        Ok(_) => panic!("assertion budget failure passed consuming cleanup"),
                        Err(error) => error,
                    }
                }
                _ => unreachable!(),
            };
            let message = error.to_string();
            assert!(
                message.contains(match failure {
                    "callback" => "callback failure",
                    "assertion" => "row-count assertion failed",
                    "finalize" => "finalization failed",
                    "budget" => "work budget",
                    _ => unreachable!(),
                }),
                "unexpected {failure} diagnostic: {message}"
            );
            assert!(destroyed.load(Ordering::SeqCst));
            assert_eq!(fingerprint(&source), before);
            assert_no_sidecars(&source);
        }
    }

    #[test]
    fn managed_closed_audit_panic_uses_drop_cleanup_and_returns_no_evidence() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "closed-audit-panic.db");
        create_relation_assertion_source(&source);
        let before = fingerprint(&source);
        let reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let destroyed = install_connection_destruction_probe(&reader);
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _audit = reader.close_with_relation_assertions_v1(|_| {
                panic!("injected consuming assertion panic")
            });
        }));
        assert!(panic.is_err());
        assert!(destroyed.load(Ordering::SeqCst));
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn assertion_point_panic_after_transient_bind_finalizes_before_local_key_drops() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "assertion-transient-bind-panic.db");
        create_relation_assertion_source(&source);
        let before = fingerprint(&source);
        let mut reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.set(0));
        super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.set(0));
        let panic = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _evidence = reader.run_relation_assertions_v1(|planner| {
                let relation_id = assertion_relation_id(planner, "assertion_00");
                super::MANAGED_POINT_TEST_PANIC_AFTER_BIND.with(|panic| panic.set(true));
                planner.require_string_pair(relation_id, "exact", "value")
            });
        }));
        assert!(panic.is_err());
        assert!(!super::MANAGED_POINT_TEST_PANIC_AFTER_BIND.with(|panic| panic.get()));
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_INSTALLS.with(|count| count.get()),
            1
        );
        assert_eq!(
            super::MANAGED_ASSERTION_PROGRESS_UNREGISTRATIONS.with(|count| count.get()),
            1
        );
        assert_eq!(
            prepared_statement_count(reader.connection.as_ref().unwrap()),
            0,
            "post-bind panic left the SQLITE_TRANSIENT statement live"
        );
        assert!(reader.close_and_verify().is_err());
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn managed_reader_detects_source_drift_after_explicit_close() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "close-drift.db");
        create_managed_cozo_source(&source);
        let reader = ExistingSqliteSnapshotSource::open(&source)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();

        OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"post-readiness drift")
            .unwrap();
        let drifted = fingerprint(&source);
        assert!(reader.close_and_verify().is_err());
        assert_eq!(fingerprint(&source), drifted);
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn managed_reader_detects_rename_before_readiness_setup() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "rename-source.db");
        let moved = test_path(&directory, "rename-moved.db");
        create_managed_cozo_source(&source);
        let guarded = ExistingSqliteSnapshotSource::open(&source).unwrap();
        fs::rename(&source, &moved).unwrap();
        let moved_before = fingerprint(&moved);

        assert!(
            guarded
                .into_managed_reader(ManagedSnapshotPolicy::V1)
                .is_err()
        );
        assert_eq!(fingerprint(&moved), moved_before);
        assert!(!source.exists());
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn held_source_descriptor_is_readonly_blocking_and_cloexec() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        let fd = snapshot.main_file.file.as_raw_fd();

        let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(status_flags, -1);
        assert_eq!(status_flags & libc::O_ACCMODE, libc::O_RDONLY);
        assert_eq!(status_flags & libc::O_NONBLOCK, 0);

        let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(descriptor_flags, -1);
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn clean_source_backup_is_usable_and_source_is_unchanged() {
        let directory = tempdir().unwrap();
        #[cfg(unix)]
        let source = test_path(&directory, "source ?#%.db");
        #[cfg(not(unix))]
        let source = test_path(&directory, "source unicode ü.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let before = fingerprint(&source);

        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        assert_eq!(fingerprint(&source), before);
        snapshot.backup_to_new(&destination).unwrap();

        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
        assert_payload_copied(&destination);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let metadata = fs::metadata(&destination).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
            assert_eq!(metadata.nlink(), 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_retains_custody_and_readonly_reader() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempdir().unwrap();
        let source = test_path(&directory, "handoff-source.db");
        let destination = test_path(&directory, "handoff-destination.db");
        create_source(&source);
        let source_before = fingerprint(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let handed_off = snapshot
            .backup_to_new_snapshot_source_v1(&destination)
            .unwrap();

        assert_eq!(fingerprint(&source), source_before);
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
        assert_payload_copied(&destination);
        handed_off
            .verify_source("test private backup handoff")
            .unwrap();
        assert_eq!(
            unsafe {
                ffi::sqlite3_db_readonly(
                    handed_off.connection.as_raw(),
                    SQLITE_MAIN_SCHEMA.as_ptr().cast(),
                )
            },
            1
        );

        let retained = handed_off
            .retained_backup_destination
            .as_ref()
            .expect("private backup handoff must retain its create-new custody descriptor");
        let main_flags =
            unsafe { libc::fcntl(handed_off.main_file.file.as_raw_fd(), libc::F_GETFL) };
        let retained_flags = unsafe { libc::fcntl(retained.file.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(main_flags, -1);
        assert_ne!(retained_flags, -1);
        assert_eq!(main_flags & libc::O_ACCMODE, libc::O_RDONLY);
        assert_eq!(retained_flags & libc::O_ACCMODE, libc::O_RDWR);
        assert_eq!(main_flags & libc::O_NONBLOCK, 0);
        assert_eq!(retained_flags & libc::O_NONBLOCK, 0);
        let main_descriptor_flags =
            unsafe { libc::fcntl(handed_off.main_file.file.as_raw_fd(), libc::F_GETFD) };
        let retained_descriptor_flags =
            unsafe { libc::fcntl(retained.file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(main_descriptor_flags, -1);
        assert_ne!(retained_descriptor_flags, -1);
        assert_ne!(main_descriptor_flags & libc::FD_CLOEXEC, 0);
        assert_ne!(retained_descriptor_flags & libc::FD_CLOEXEC, 0);

        let named = fs::metadata(&destination).unwrap();
        let main = handed_off.main_file.file.metadata().unwrap();
        let custody = retained.file.metadata().unwrap();
        assert_eq!((main.dev(), main.ino()), (named.dev(), named.ino()));
        assert_eq!((custody.dev(), custody.ino()), (named.dev(), named.ino()));
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_enters_and_strictly_closes_managed_reader() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "managed-handoff-source.db");
        let destination = test_path(&directory, "managed-handoff-destination.db");
        create_managed_cozo_source(&source);
        let source_before = fingerprint(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        snapshot
            .backup_to_new_snapshot_source_v1(&destination)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap()
            .close_and_verify()
            .unwrap();

        assert_eq!(fingerprint(&source), source_before);
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_never_clobbers_existing_destination() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "handoff-no-clobber-source.db");
        let destination = test_path(&directory, "handoff-no-clobber-destination.db");
        let sentinel = b"private handoff destination sentinel";
        create_source(&source);
        let source_before = fingerprint(&source);
        fs::write(&destination, sentinel).unwrap();
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        assert!(
            snapshot
                .backup_to_new_snapshot_source_v1(&destination)
                .is_err()
        );

        assert_eq!(fingerprint(&source), source_before);
        assert_eq!(fs::read(&destination).unwrap(), sentinel);
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_failure_after_create_cleans_destination() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "handoff-create-failure-source.db");
        let destination = test_path(&directory, "handoff-create-failure-destination.db");
        create_source(&source);
        let source_before = fingerprint(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        assert!(
            snapshot
                .backup_to_new_snapshot_source_failing_after_create(&destination)
                .is_err()
        );

        assert_eq!(fingerprint(&source), source_before);
        assert!(!destination.exists());
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_rejects_corruption_before_source_construction() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "handoff-corrupt-source.db");
        let destination = test_path(&directory, "handoff-corrupt-destination.db");
        create_source(&source);
        let source_before = fingerprint(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let error = snapshot
            .backup_to_new_snapshot_source_with_after_native_backup_hook(&destination, || {
                OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&destination)
                    .map_err(|error| {
                        miette::miette!("cannot corrupt handoff destination: {error}")
                    })?
                    .write_all(b"broken private backup")
                    .map_err(|error| miette::miette!("cannot write handoff corruption: {error}"))?;
                Ok(())
            })
            .err()
            .expect("corrupt private backup must not become a guarded source");

        assert!(
            error.to_string().contains("bounded SQLite header"),
            "unexpected corruption error: {error}"
        );
        assert_eq!(fingerprint(&source), source_before);
        assert!(!destination.exists());
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_keeps_cleanup_armed_through_ready_hook() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "handoff-ready-failure-source.db");
        let destination = test_path(&directory, "handoff-ready-failure-destination.db");
        create_source(&source);
        let source_before = fingerprint(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let error = snapshot
            .backup_to_new_snapshot_source_with_ready_hook(&destination, |ready, created| {
                assert!(created.cleanup_armed);
                assert!(ready.retained_backup_destination.is_some());
                assert_eq!(
                    unsafe {
                        ffi::sqlite3_db_readonly(
                            ready.connection.as_raw(),
                            SQLITE_MAIN_SCHEMA.as_ptr().cast(),
                        )
                    },
                    1
                );
                Err(miette::miette!(
                    "injected private backup ready-hook failure"
                ))
            })
            .err()
            .expect("ready-hook failure must withhold the handed-off source");

        assert!(
            error
                .to_string()
                .contains("injected private backup ready-hook failure")
        );
        assert_eq!(fingerprint(&source), source_before);
        assert!(!destination.exists());
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_preserves_foreign_sidecar_seen_after_ready() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "handoff-sidecar-source.db");
        let destination = test_path(&directory, "handoff-sidecar-destination.db");
        let foreign_sidecar = sidecar_path(&destination, "-wal");
        let sentinel = b"foreign post-ready sidecar";
        create_source(&source);
        let source_before = fingerprint(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        let error = snapshot
            .backup_to_new_snapshot_source_with_ready_hook(&destination, |_, _| {
                fs::write(&foreign_sidecar, sentinel).map_err(|error| {
                    miette::miette!("cannot create post-ready sidecar: {error}")
                })?;
                Ok(())
            })
            .err()
            .expect("post-ready sidecar must block handoff publication");

        assert!(error.to_string().contains("sidecar"));
        assert_eq!(fingerprint(&source), source_before);
        assert!(!destination.exists());
        assert_eq!(fs::read(&foreign_sidecar).unwrap(), sentinel);
        assert_no_sidecars(&source);
    }

    #[cfg(unix)]
    #[test]
    fn private_backup_handoff_combines_close_cleanup_and_original_source_failures() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "handoff-combined-source.db");
        let destination = test_path(&directory, "handoff-combined-destination.db");
        let moved_owned = test_path(&directory, "handoff-combined-owned.db");
        let foreign = b"foreign replacement after private backup readiness";
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        super::MANAGED_BACKUP_HANDOFF_CLOSE_TEST_FAILURE.with(|failure| failure.set(true));

        let error = snapshot
            .backup_to_new_snapshot_source_with_ready_hook(&destination, |_, _| {
                OpenOptions::new()
                    .append(true)
                    .open(&source)
                    .map_err(|error| miette::miette!("cannot drift original source: {error}"))?
                    .write_all(b"original source drift")
                    .map_err(|error| {
                        miette::miette!("cannot write original source drift: {error}")
                    })?;
                fs::rename(&destination, &moved_owned).map_err(|error| {
                    miette::miette!("cannot move owned handoff destination: {error}")
                })?;
                fs::write(&destination, foreign).map_err(|error| {
                    miette::miette!("cannot create foreign handoff replacement: {error}")
                })?;
                Err(miette::miette!("injected combined handoff failure"))
            })
            .err()
            .expect("combined handoff failure must withhold the source");

        let message = error.to_string();
        assert!(message.contains("injected combined handoff failure"));
        assert!(message.contains("injected private backup snapshot source strict-close failure"));
        assert!(message.contains("cleanup of incomplete snapshot destination also failed"));
        assert!(
            message.contains("snapshot source identity postcondition failed after failed backup")
        );
        assert_eq!(fs::read(&destination).unwrap(), foreign);
        assert_payload_copied(&moved_owned);
    }

    #[test]
    fn closed_catalog_fixture_backup_preserves_source_and_publishes_auditable_destination() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "fixture-source.db");
        let destination = test_path(&directory, "fixture-destination.db");
        create_populated_managed_cozo_source(&source);
        let source_before = fingerprint(&source);

        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        snapshot
            .backup_to_new_with_catalog_fixture_v1_for_tests(
                &destination,
                ManagedCatalogFixtureV1::AllPositionalV1,
            )
            .unwrap();

        assert_eq!(fingerprint(&source), source_before);
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
        let mut reader = ExistingSqliteSnapshotSource::open(&destination)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::V1)
            .unwrap();
        let census = reader.inspect_physical_census_v1().unwrap();
        assert!(
            census
                .catalog()
                .catalog()
                .entries()
                .iter()
                .all(|entry| entry.encoding() == ManagedCatalogEncodingV1::PositionalV1)
        );
        reader.close_and_verify().unwrap();
    }

    #[test]
    fn closed_catalog_fixture_backup_never_clobbers_an_existing_destination() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "fixture-source.db");
        let destination = test_path(&directory, "fixture-destination.db");
        create_populated_managed_cozo_source(&source);
        let source_before = fingerprint(&source);
        let sentinel = b"existing fixture destination";
        fs::write(&destination, sentinel).unwrap();

        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        assert!(
            snapshot
                .backup_to_new_with_catalog_fixture_v1_for_tests(
                    &destination,
                    ManagedCatalogFixtureV1::AllStructMapV1,
                )
                .is_err()
        );

        assert_eq!(fingerprint(&source), source_before);
        assert_eq!(fs::read(&destination).unwrap(), sentinel);
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
    }

    #[test]
    fn closed_catalog_fixture_invalid_preimage_cleans_owned_destination() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "fixture-source.db");
        let destination = test_path(&directory, "fixture-destination.db");
        // This exact 26-relation image passes G2a/G2c but deliberately has no
        // nested child from which the closed mismatch fixture can be derived.
        create_populated_managed_cozo_source(&source);
        let source_before = fingerprint(&source);

        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        let error = snapshot
            .backup_to_new_with_catalog_fixture_v1_for_tests(
                &destination,
                ManagedCatalogFixtureV1::NestedChildRelationIdMismatch,
            )
            .expect_err("invalid fixture preimage must fail");

        assert!(error.to_string().contains("fixture rewrite precondition"));
        assert_eq!(fingerprint(&source), source_before);
        assert!(!destination.exists());
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
    }

    #[test]
    fn zero_wal_and_shm_residuals_are_frozen_through_drop() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        create_wal_source(&source, true, true);
        let wal = sidecar_path(&source, "-wal");
        let shm = sidecar_path(&source, "-shm");
        assert!(wal.is_file());
        assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
        assert!(shm.is_file());
        let source_before = fingerprint(&source);
        let wal_before = fingerprint(&wal);
        let shm_before = fingerprint(&shm);

        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        snapshot.backup_to_new(&destination).unwrap();
        drop(snapshot);

        assert_eq!(fingerprint(&source), source_before);
        assert_eq!(fingerprint(&wal), wal_before);
        assert_eq!(fingerprint(&shm), shm_before);
        assert_no_sidecars(&destination);
        assert_payload_copied(&destination);
    }

    #[test]
    fn wal_header_without_sidecars_backs_up_under_immutable_open() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        create_sidecar_free_wal_header_source(&source);
        assert_no_sidecars(&source);
        let header = fs::read(&source).unwrap();
        assert_eq!(&header[..16], b"SQLite format 3\0");
        assert_eq!((header[18], header[19]), (2, 2));
        let before = fingerprint(&source);

        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        snapshot.backup_to_new(&destination).unwrap();
        drop(snapshot);

        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
        assert_no_sidecars(&destination);
        assert_payload_copied(&destination);
    }

    #[test]
    fn nonzero_uncheckpointed_wal_refuses_unchanged() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let main_image = test_path(&directory, "main-only.db");
        let writer = create_committed_uncheckpointed_wal_source(&source);
        let wal = sidecar_path(&source, "-wal");
        let shm = sidecar_path(&source, "-shm");
        assert!(fs::metadata(&wal).unwrap().len() > 0);
        assert_payload_absent_from_main_image(&source, &main_image);
        assert_payload_visible_through_wal(&source);
        let source_before = fingerprint(&source);
        let wal_before = fingerprint(&wal);
        let shm_before = fingerprint(&shm);

        assert!(ExistingSqliteSnapshotSource::open(&source).is_err());

        assert_eq!(fingerprint(&source), source_before);
        assert_eq!(fingerprint(&wal), wal_before);
        assert_eq!(fingerprint(&shm), shm_before);
        drop(writer);
    }

    #[test]
    fn hot_rollback_journal_refuses_before_permit_without_family_mutation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let writer = create_hot_rollback_journal_source(&source);
        let journal = sidecar_path(&source, "-journal");
        assert!(journal.is_file());
        assert!(fs::metadata(&journal).unwrap().len() > 0);
        let source_before = fingerprint(&source);
        let journal_before = fingerprint(&journal);
        let family_before = directory_byte_census(directory.path());

        assert!(existing_open_permit(&source).is_err());

        assert_eq!(fingerprint(&source), source_before);
        assert_eq!(fingerprint(&journal), journal_before);
        assert_eq!(directory_byte_census(directory.path()), family_before);
        drop(writer);
    }

    #[test]
    fn sidecar_created_after_open_blocks_backup_and_is_not_removed() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        let source_before = fingerprint(&source);
        let sidecar = sidecar_path(&source, "-wal");
        fs::write(&sidecar, b"not ours").unwrap();

        assert!(snapshot.backup_to_new(&destination).is_err());
        assert_eq!(fingerprint(&source), source_before);
        assert_eq!(fs::read(&sidecar).unwrap(), b"not ours");
        assert!(!destination.exists());
        assert_no_sidecars(&destination);
    }

    #[test]
    fn existing_destination_sentinel_is_untouched() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();
        let sentinel = b"do not clobber";
        fs::write(&destination, sentinel).unwrap();

        assert!(snapshot.backup_to_new(&destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), sentinel);
        assert_no_sidecars(&destination);
    }

    #[test]
    fn oversized_schema_is_fused_before_parsing_with_bounded_diagnostics() {
        const SENTINEL: &str = "OVERSIZED_ATTACKER_SCHEMA_SENTINEL";

        let directory = tempdir().unwrap();
        let source = test_path(&directory, "oversized-schema.db");
        let repetitions =
            super::MANAGED_SQLITE_SCHEMA_LENGTH_LIMIT as usize / SENTINEL.len() + 1024;
        let attacker_text = SENTINEL.repeat(repetitions);
        let ddl = format!("CREATE TABLE cozo (k BLOB primary key, v BLOB /*{attacker_text}*/);");
        assert!(ddl.len() > super::MANAGED_SQLITE_SCHEMA_LENGTH_LIMIT as usize);
        create_schema_source(&source, &ddl);
        let before = fingerprint(&source);

        let outcome = std::panic::catch_unwind(|| ExistingSqliteSnapshotSource::open(&source));
        let result = outcome.expect("oversized schema must not panic");
        let error = match result {
            Ok(_) => panic!("oversized schema unexpectedly passed the source guardrails"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("SQLite code"));
        assert!(message.len() < 192);
        assert!(!message.contains(SENTINEL));
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn malformed_source_fails_without_mutation() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "malformed.db");
        const SENTINEL: &str = "MALFORMED_SQLITE_ATTACKER_SENTINEL";
        fs::write(&source, SENTINEL).unwrap();
        let before = fingerprint(&source);

        let outcome = std::panic::catch_unwind(|| ExistingSqliteSnapshotSource::open(&source));
        let result = outcome.expect("malformed SQLite must not panic");
        let error = match result {
            Ok(_) => panic!("malformed SQLite unexpectedly opened"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.len() < 192);
        assert!(!message.contains(SENTINEL));
        assert_eq!(fingerprint(&source), before);
        assert_no_sidecars(&source);
    }

    #[test]
    fn injected_backup_failure_removes_incomplete_destination() {
        let directory = tempdir().unwrap();
        let source = test_path(&directory, "source.db");
        let destination = test_path(&directory, "destination.db");
        create_source(&source);
        let source_before = fingerprint(&source);
        let snapshot = ExistingSqliteSnapshotSource::open(&source).unwrap();

        assert!(
            snapshot
                .backup_to_new_failing_after_create(&destination)
                .is_err()
        );
        assert_eq!(fingerprint(&source), source_before);
        assert_no_sidecars(&source);
        assert!(!destination.exists());
        assert_no_sidecars(&destination);
    }

    fn create_managed_episode_policy_source(path: &Path, relations: usize) {
        let database = new_cozo_sqlite(path).unwrap();
        for index in 0..relations {
            database
                .run_script(
                    &format!(":create episode_policy_{index:02} {{id: Int}}"),
                    Default::default(),
                    crate::ScriptMutability::Mutable,
                )
                .unwrap();
        }
        database.db.prepare_for_file_move().unwrap();
        drop(database);
        assert_no_sidecars(path);
    }

    #[test]
    fn managed_episode_policy_exact_catalog_boundary_preserves_v1() {
        let directory = tempdir().unwrap();
        for count in [26, 30, 31, 32] {
            let path = test_path(&directory, &format!("episode-policy-{count}.db"));
            create_managed_episode_policy_source(&path, count);
            let before = fingerprint(&path);
            for (policy, expected) in [
                (ManagedSnapshotPolicy::V1, 26),
                (ManagedSnapshotPolicy::EpisodeV1, 31),
            ] {
                let mut reader = ExistingSqliteSnapshotSource::open(&path)
                    .unwrap()
                    .into_managed_reader(policy)
                    .unwrap();
                if count == expected {
                    let catalog = reader.inspect_primary_index_catalog_v1().unwrap();
                    assert_eq!(catalog.catalog().len(), expected);
                    assert_eq!(catalog.policy_fingerprint(), policy.catalog_fingerprint());
                    reader.close_and_verify().unwrap();
                } else {
                    assert!(reader.inspect_primary_index_catalog_v1().is_err());
                    assert!(
                        reader.close_and_verify().is_err(),
                        "wrong policy must poison reader"
                    );
                }
                assert_eq!(fingerprint(&path), before);
                assert_no_sidecars(&path);
            }
        }
    }

    #[test]
    fn managed_single_graph_policy_exact_catalog_boundary_preserves_predecessors() {
        let directory = tempdir().unwrap();
        for count in [26, 28, 29, 30, 31, 32] {
            let path = test_path(&directory, &format!("episode-policy-{count}.db"));
            create_managed_episode_policy_source(&path, count);
            let before = fingerprint(&path);
            for (policy, expected) in [
                (ManagedSnapshotPolicy::V1, 26),
                (ManagedSnapshotPolicy::EpisodeV1, 31),
                (ManagedSnapshotPolicy::SingleGraphV1, 29),
            ] {
                let mut reader = ExistingSqliteSnapshotSource::open(&path)
                    .unwrap()
                    .into_managed_reader(policy)
                    .unwrap();
                if count == expected {
                    let catalog = reader.inspect_primary_index_catalog_v1().unwrap();
                    assert_eq!(catalog.catalog().len(), expected);
                    assert_eq!(catalog.policy_fingerprint(), policy.catalog_fingerprint());
                    reader.close_and_verify().unwrap();
                } else {
                    assert!(reader.inspect_primary_index_catalog_v1().is_err());
                    assert!(
                        reader.close_and_verify().is_err(),
                        "wrong policy must poison reader"
                    );
                }
                assert_eq!(fingerprint(&path), before);
                assert_no_sidecars(&path);
            }
        }
    }

    #[test]
    fn managed_episode_policy_physical_and_fence_evidence_pin_successor() {
        let directory = tempdir().unwrap();
        let path = test_path(&directory, "episode-policy-evidence.db");
        create_managed_episode_policy_source(&path, 31);
        let before = fingerprint(&path);
        let reader = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::EpisodeV1)
            .unwrap();
        let audit = reader
            .close_with_relation_assertions_v1(|planner| {
                let first = planner.catalog().catalog().entries()[0].relation().id();
                planner.require_exact_row_count(first, 0)
            })
            .unwrap();
        assert_eq!(audit.policy(), ManagedSnapshotPolicy::EpisodeV1);
        assert_eq!(
            audit.catalog().policy_fingerprint(),
            &super::MANAGED_EPISODE_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit.physical().policy_fingerprint(),
            &super::MANAGED_EPISODE_PHYSICAL_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit.source().policy_fingerprint(),
            &super::MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit
                .physical()
                .relation_row_counts()
                .iter()
                .find(|entry| entry.relation_id() == 0)
                .unwrap()
                .row_count(),
            33
        );
        drop(audit);
        let fence = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::EpisodeV1, |planner| {
                let first = planner.catalog().catalog().entries()[0].relation().id();
                planner.require_exact_row_count(first, 0)
            })
            .unwrap();
        assert!(fence.selector() == ManagedCatalogFencePolicy::EpisodeV1);
        assert_eq!(fence.snapshot_policy(), ManagedSnapshotPolicy::EpisodeV1);
        assert_eq!(
            fence.catalog_policy_fingerprint(),
            &super::MANAGED_EPISODE_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.source_policy_fingerprint(),
            &super::MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.catalog_fence_policy_fingerprint(),
            &super::MANAGED_EPISODE_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        let permit = fence.into_existing_sqlite_open_permit_v1();
        drop(permit);
        assert_eq!(fingerprint(&path), before);
        assert_no_sidecars(&path);
    }

    #[test]
    fn managed_single_graph_policy_physical_and_fence_evidence_pin_successor() {
        let directory = tempdir().unwrap();
        let path = test_path(&directory, "episode-policy-evidence.db");
        create_managed_episode_policy_source(&path, 29);
        let before = fingerprint(&path);
        let reader = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::SingleGraphV1)
            .unwrap();
        let audit = reader
            .close_with_relation_assertions_v1(|planner| {
                let first = planner.catalog().catalog().entries()[0].relation().id();
                planner.require_exact_row_count(first, 0)
            })
            .unwrap();
        assert_eq!(audit.policy(), ManagedSnapshotPolicy::SingleGraphV1);
        assert_eq!(
            audit.catalog().policy_fingerprint(),
            &super::MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit.physical().policy_fingerprint(),
            &super::MANAGED_SINGLE_GRAPH_PHYSICAL_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit.source().policy_fingerprint(),
            &super::MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit
                .physical()
                .relation_row_counts()
                .iter()
                .find(|entry| entry.relation_id() == 0)
                .unwrap()
                .row_count(),
            31
        );
        drop(audit);
        let fence = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::SingleGraphV1, |planner| {
                let first = planner.catalog().catalog().entries()[0].relation().id();
                planner.require_exact_row_count(first, 0)
            })
            .unwrap();
        assert!(fence.selector() == ManagedCatalogFencePolicy::SingleGraphV1);
        assert_eq!(
            fence.snapshot_policy(),
            ManagedSnapshotPolicy::SingleGraphV1
        );
        assert_eq!(
            fence.catalog_policy_fingerprint(),
            &super::MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.source_policy_fingerprint(),
            &super::MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.catalog_fence_policy_fingerprint(),
            &super::MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        let permit = fence.into_existing_sqlite_open_permit_v1();
        drop(permit);
        assert_eq!(fingerprint(&path), before);
        assert_no_sidecars(&path);
    }

    #[test]
    fn managed_episode_policy_refuses_the_frozen_sixteen_relation_visitor() {
        let directory = tempdir().unwrap();
        let path = test_path(&directory, "episode-policy-visitor-refusal.db");
        create_managed_episode_policy_source(&path, 31);
        let before = fingerprint(&path);
        let reader = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::EpisodeV1)
            .unwrap();
        let result = reader.close_with_relation_assertions_and_record_visit_v1(
            (),
            |_| panic!("predecessor visitor must refuse before planning"),
            |_, _| panic!("predecessor visitor must never emit episode rows"),
        );
        assert!(result.is_err());
        assert_eq!(fingerprint(&path), before);
        assert_no_sidecars(&path);
    }

    #[test]
    fn managed_single_graph_policy_refuses_the_frozen_sixteen_relation_visitor() {
        let directory = tempdir().unwrap();
        let path = test_path(&directory, "episode-policy-visitor-refusal.db");
        create_managed_episode_policy_source(&path, 29);
        let before = fingerprint(&path);
        let reader = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::SingleGraphV1)
            .unwrap();
        let result = reader.close_with_relation_assertions_and_record_visit_v1(
            (),
            |_| panic!("predecessor visitor must refuse before planning"),
            |_, _| panic!("predecessor visitor must never emit episode rows"),
        );
        assert!(result.is_err());
        assert_eq!(fingerprint(&path), before);
        assert_no_sidecars(&path);
    }

    #[test]
    fn managed_episode_policy_fingerprints_bind_only_explicit_successor_overrides() {
        fn derive(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
            let mut hash = Sha256::new();
            hash.update(domain);
            for field in fields {
                hash.update(field);
            }
            hash.finalize().into()
        }
        let identity = b"mnestic.managed-snapshot-policy.episode.v1";
        let query = b"SELECT length(k), length(v), k, v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' ORDER BY k LIMIT 34;\0";
        assert_eq!(super::MANAGED_EPISODE_CATALOG_RELATION_COUNT_V1, 31);
        assert_eq!(super::MANAGED_EPISODE_CATALOG_QUERY_V1, query);
        assert_eq!(
            super::MANAGED_SNAPSHOT_EPISODE_POLICY_V1_IDENTITY_BYTES,
            identity
        );
        assert_eq!(ManagedSnapshotPolicy::V1.relation_count(), 26);
        assert_eq!(ManagedSnapshotPolicy::V1.catalog_row_count(), 28);
        assert_eq!(ManagedSnapshotPolicy::EpisodeV1.catalog_row_count(), 33);
        let catalog = derive(
            b"mnestic.managed-sqlite-episode-catalog.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CATALOG_POLICY_FINGERPRINT_V1,
                identity,
                &31_u64.to_be_bytes(),
                &33_u64.to_be_bytes(),
                &34_u64.to_be_bytes(),
                query,
            ],
        );
        let physical = derive(
            b"mnestic.managed-sqlite-episode-physical.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1,
                &catalog,
                &33_u64.to_be_bytes(),
            ],
        );
        let source = derive(
            b"mnestic.managed-sqlite-episode-source.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
                identity,
            ],
        );
        let fence = derive(
            b"mnestic.managed-sqlite-episode-catalog-fence.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
                b"ManagedCatalogFencePolicy::EpisodeV1",
                identity,
                &catalog,
                &source,
                &super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1,
            ],
        );
        assert_eq!(
            catalog,
            super::MANAGED_EPISODE_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            physical,
            super::MANAGED_EPISODE_PHYSICAL_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            source,
            super::MANAGED_EPISODE_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence,
            super::MANAGED_EPISODE_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        assert_ne!(catalog, super::MANAGED_CATALOG_POLICY_FINGERPRINT_V1);
        assert_ne!(physical, super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1);
        assert_ne!(source, super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1);
        assert_ne!(
            fence,
            super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
    }
    #[test]
    fn managed_single_graph_policy_fingerprints_bind_only_explicit_successor_overrides() {
        fn derive(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
            let mut hash = Sha256::new();
            hash.update(domain);
            for field in fields {
                hash.update(field);
            }
            hash.finalize().into()
        }
        let identity = b"mnestic.managed-snapshot-policy.single-graph.v1";
        let query = b"SELECT length(k), length(v), k, v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' ORDER BY k LIMIT 32;\0";
        assert_eq!(super::MANAGED_SINGLE_GRAPH_CATALOG_RELATION_COUNT_V1, 29);
        assert_eq!(super::MANAGED_SINGLE_GRAPH_CATALOG_QUERY_V1, query);
        assert_eq!(
            super::MANAGED_SNAPSHOT_SINGLE_GRAPH_POLICY_V1_IDENTITY_BYTES,
            identity
        );
        assert_eq!(ManagedSnapshotPolicy::V1.relation_count(), 26);
        assert_eq!(ManagedSnapshotPolicy::V1.catalog_row_count(), 28);
        assert_eq!(ManagedSnapshotPolicy::SingleGraphV1.catalog_row_count(), 31);
        let catalog = derive(
            b"mnestic.managed-sqlite-single-graph-catalog.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CATALOG_POLICY_FINGERPRINT_V1,
                identity,
                &29_u64.to_be_bytes(),
                &31_u64.to_be_bytes(),
                &32_u64.to_be_bytes(),
                query,
            ],
        );
        let physical = derive(
            b"mnestic.managed-sqlite-single-graph-physical.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1,
                &catalog,
                &31_u64.to_be_bytes(),
            ],
        );
        let source = derive(
            b"mnestic.managed-sqlite-single-graph-source.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
                identity,
            ],
        );
        let fence = derive(
            b"mnestic.managed-sqlite-single-graph-catalog-fence.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
                b"ManagedCatalogFencePolicy::SingleGraphV1",
                identity,
                &catalog,
                &source,
                &super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1,
            ],
        );
        assert_eq!(
            catalog,
            super::MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            physical,
            super::MANAGED_SINGLE_GRAPH_PHYSICAL_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            source,
            super::MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence,
            super::MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        assert_ne!(catalog, super::MANAGED_CATALOG_POLICY_FINGERPRINT_V1);
        assert_ne!(physical, super::MANAGED_PHYSICAL_POLICY_FINGERPRINT_V1);
        assert_ne!(source, super::MANAGED_CLOSED_SOURCE_POLICY_FINGERPRINT_V1);
        assert_ne!(
            fence,
            super::MANAGED_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
    }
    #[test]
    fn concern_policy_fingerprints_bind_exact_successor_without_changing_predecessors() {
        fn derive(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
            let mut hash = Sha256::new();
            hash.update(domain);
            for field in fields {
                hash.update(field);
            }
            hash.finalize().into()
        }
        let identity = b"mnestic.managed-snapshot-policy.concern.v1";
        let query = b"SELECT length(k), length(v), k, v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' ORDER BY k LIMIT 34;\0";
        assert_eq!(ManagedSnapshotPolicy::ConcernV1.relation_count(), 31);
        assert_eq!(ManagedSnapshotPolicy::ConcernV1.catalog_row_count(), 33);
        assert_eq!(super::MANAGED_CONCERN_CATALOG_QUERY_V1, query);
        assert_eq!(
            super::MANAGED_SNAPSHOT_CONCERN_POLICY_V1_IDENTITY_BYTES,
            identity
        );
        let catalog = derive(
            b"mnestic.managed-sqlite-concern-catalog.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_SINGLE_GRAPH_CATALOG_POLICY_FINGERPRINT_V1,
                identity,
                &31_u64.to_be_bytes(),
                &33_u64.to_be_bytes(),
                &34_u64.to_be_bytes(),
                query,
            ],
        );
        let physical = derive(
            b"mnestic.managed-sqlite-concern-physical.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_SINGLE_GRAPH_PHYSICAL_POLICY_FINGERPRINT_V1,
                &catalog,
                &33_u64.to_be_bytes(),
            ],
        );
        let source = derive(
            b"mnestic.managed-sqlite-concern-source.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_SINGLE_GRAPH_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
                identity,
            ],
        );
        let fence = derive(
            b"mnestic.managed-sqlite-concern-catalog-fence.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_SINGLE_GRAPH_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
                b"ManagedCatalogFencePolicy::ConcernV1",
                identity,
                &catalog,
                &source,
                &super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1,
            ],
        );
        assert_eq!(
            catalog,
            super::MANAGED_CONCERN_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            physical,
            super::MANAGED_CONCERN_PHYSICAL_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            source,
            super::MANAGED_CONCERN_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence,
            super::MANAGED_CONCERN_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(ManagedSnapshotPolicy::SingleGraphV1.relation_count(), 29);
        assert_eq!(ManagedSnapshotPolicy::EpisodeV1.relation_count(), 31);
    }
    #[test]
    fn touchstones_policy_fingerprints_bind_exact_successor_without_changing_predecessors() {
        fn derive(domain: &[u8], fields: &[&[u8]]) -> [u8; 32] {
            let mut hash = Sha256::new();
            hash.update(domain);
            for field in fields {
                hash.update(field);
            }
            hash.finalize().into()
        }
        let identity = b"mnestic.managed-snapshot-policy.touchstones.v1";
        let query = b"SELECT length(k), length(v), k, v FROM main.cozo INDEXED BY sqlite_autoindex_cozo_1 WHERE k >= x'0000000000000000' AND k < x'0000000000000001' ORDER BY k LIMIT 37;\0";
        assert_eq!(ManagedSnapshotPolicy::TouchstonesV1.relation_count(), 34);
        assert_eq!(ManagedSnapshotPolicy::TouchstonesV1.catalog_row_count(), 36);
        assert_eq!(super::MANAGED_TOUCHSTONES_CATALOG_QUERY_V1, query);
        assert_eq!(
            super::MANAGED_SNAPSHOT_TOUCHSTONES_POLICY_V1_IDENTITY_BYTES,
            identity
        );
        let catalog = derive(
            b"mnestic.managed-sqlite-touchstones-catalog.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CONCERN_CATALOG_POLICY_FINGERPRINT_V1,
                identity,
                &34_u64.to_be_bytes(),
                &36_u64.to_be_bytes(),
                &37_u64.to_be_bytes(),
                query,
            ],
        );
        let physical = derive(
            b"mnestic.managed-sqlite-touchstones-physical.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CONCERN_PHYSICAL_POLICY_FINGERPRINT_V1,
                &catalog,
                &36_u64.to_be_bytes(),
            ],
        );
        let source = derive(
            b"mnestic.managed-sqlite-touchstones-source.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CONCERN_CLOSED_SOURCE_POLICY_FINGERPRINT_V1,
                identity,
            ],
        );
        let fence = derive(
            b"mnestic.managed-sqlite-touchstones-catalog-fence.policy-fingerprint.v1\0",
            &[
                &super::MANAGED_CONCERN_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1,
                b"ManagedCatalogFencePolicy::TouchstonesV1",
                identity,
                &catalog,
                &source,
                &super::MANAGED_CATALOG_FENCE_ASSERTION_POLICY_FINGERPRINT_V1,
            ],
        );
        assert_eq!(
            catalog,
            super::MANAGED_TOUCHSTONES_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            physical,
            super::MANAGED_TOUCHSTONES_PHYSICAL_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            source,
            super::MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence,
            super::MANAGED_TOUCHSTONES_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(ManagedSnapshotPolicy::ConcernV1.relation_count(), 31);
        assert_eq!(ManagedSnapshotPolicy::ConcernV1.catalog_row_count(), 33);
        assert_eq!(ManagedSnapshotPolicy::SingleGraphV1.relation_count(), 29);
        assert_eq!(ManagedSnapshotPolicy::EpisodeV1.relation_count(), 31);
    }

    #[test]
    fn managed_touchstones_policy_exact_catalog_boundary_preserves_predecessors() {
        let directory = tempdir().unwrap();
        for count in [26, 29, 31, 33, 34, 35] {
            let path = test_path(&directory, &format!("touchstones-policy-{count}.db"));
            create_managed_episode_policy_source(&path, count);
            let before = fingerprint(&path);
            for (policy, expected) in [
                (ManagedSnapshotPolicy::V1, 26),
                (ManagedSnapshotPolicy::EpisodeV1, 31),
                (ManagedSnapshotPolicy::SingleGraphV1, 29),
                (ManagedSnapshotPolicy::ConcernV1, 31),
                (ManagedSnapshotPolicy::TouchstonesV1, 34),
            ] {
                let mut reader = ExistingSqliteSnapshotSource::open(&path)
                    .unwrap()
                    .into_managed_reader(policy)
                    .unwrap();
                if count == expected {
                    let catalog = reader.inspect_primary_index_catalog_v1().unwrap();
                    assert_eq!(catalog.catalog().len(), expected);
                    assert_eq!(catalog.policy_fingerprint(), policy.catalog_fingerprint());
                    reader.close_and_verify().unwrap();
                } else {
                    assert!(reader.inspect_primary_index_catalog_v1().is_err());
                    assert!(reader.close_and_verify().is_err());
                }
                assert_eq!(fingerprint(&path), before);
                assert_no_sidecars(&path);
            }
        }
    }

    #[test]
    fn managed_touchstones_policy_physical_and_fence_evidence_pin_successor() {
        let directory = tempdir().unwrap();
        let path = test_path(&directory, "touchstones-policy-evidence.db");
        create_managed_episode_policy_source(&path, 34);
        let before = fingerprint(&path);
        let reader = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::TouchstonesV1)
            .unwrap();
        let audit = reader
            .close_with_relation_assertions_v1(|planner| {
                let first = planner.catalog().catalog().entries()[0].relation().id();
                planner.require_exact_row_count(first, 0)
            })
            .unwrap();
        assert_eq!(audit.policy(), ManagedSnapshotPolicy::TouchstonesV1);
        assert_eq!(
            audit.catalog().policy_fingerprint(),
            &super::MANAGED_TOUCHSTONES_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit.physical().policy_fingerprint(),
            &super::MANAGED_TOUCHSTONES_PHYSICAL_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit.source().policy_fingerprint(),
            &super::MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            audit
                .physical()
                .relation_row_counts()
                .iter()
                .find(|entry| entry.relation_id() == 0)
                .unwrap()
                .row_count(),
            36
        );
        drop(audit);
        let fence = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_closed_catalog_fence_v1(ManagedCatalogFencePolicy::TouchstonesV1, |planner| {
                let first = planner.catalog().catalog().entries()[0].relation().id();
                planner.require_exact_row_count(first, 0)
            })
            .unwrap();
        assert!(fence.selector() == ManagedCatalogFencePolicy::TouchstonesV1);
        assert_eq!(fence.snapshot_policy(), ManagedSnapshotPolicy::TouchstonesV1);
        assert_eq!(
            fence.catalog_policy_fingerprint(),
            &super::MANAGED_TOUCHSTONES_CATALOG_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.source_policy_fingerprint(),
            &super::MANAGED_TOUCHSTONES_CLOSED_SOURCE_POLICY_FINGERPRINT_V1
        );
        assert_eq!(
            fence.catalog_fence_policy_fingerprint(),
            &super::MANAGED_TOUCHSTONES_CLOSED_CATALOG_FENCE_POLICY_FINGERPRINT_V1
        );
        let permit = fence.into_existing_sqlite_open_permit_v1();
        drop(permit);
        assert_eq!(fingerprint(&path), before);
        assert_no_sidecars(&path);
    }

    #[test]
    fn managed_touchstones_policy_refuses_the_frozen_sixteen_relation_visitor() {
        let directory = tempdir().unwrap();
        let path = test_path(&directory, "touchstones-policy-visitor-refusal.db");
        create_managed_episode_policy_source(&path, 34);
        let before = fingerprint(&path);
        let reader = ExistingSqliteSnapshotSource::open(&path)
            .unwrap()
            .into_managed_reader(ManagedSnapshotPolicy::TouchstonesV1)
            .unwrap();
        let result = reader.close_with_relation_assertions_and_record_visit_v1(
            (),
            |_| panic!("predecessor visitor must refuse before planning"),
            |_, _| panic!("predecessor visitor must never emit touchstone rows"),
        );
        assert!(result.is_err());
        assert_eq!(fingerprint(&path), before);
        assert_no_sidecars(&path);
    }
}
