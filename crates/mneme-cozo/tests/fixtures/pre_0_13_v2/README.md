# Genuine pre-0.13 vector-v2 fixture

`memory.sqlite` was created by a historical Mneme implementation using
Mnestic 0.8.6. It is a physical compatibility canary, not a
current database whose generation marker was edited to look old.

The store contains 96 deterministic nodes and four-dimensional vectors:

- 64 active, 16 candidate, and 16 archived nodes;
- three FTS indexes and three status-filtered HNSW indexes built in bulk after
  the rows were inserted;
- `vector_projection_v2=status_partitioned_hnsw_v2`;
- a valid hashing-embedder fingerprint and the other metadata required by the
  old Mneme build.

Eight active summaries are the nonblank punctuation-only string `!!!`, which
the old Simple tokenizer turns into no terms. The same value is written to the
canonical Node blob and `node_search`, matching the old `GraphStore` write
shape while preserving the historical distinction between old base-relation
BM25 corpus statistics and Mnestic 0.13's indexed-document statistics. Under
0.8.6 the top `needle` score is `1.9037788616931108`.

## Integrity

Do not open the checked-in file directly from a test: SQLite and migration code
may mutate it. Copy it to a temporary path first.

```text
memory.sqlite  5afbb762396fa61fb542b35e5d85542a7ea31f81e723074f93f086f44aeb81cd
generator/Cargo.toml  cdc153d2bac926a70c72f8d84ffb25c69247d532b3e1d6ef2ad0d17ac13c7506
generator/src/main.rs  4e848922e49ffa44fc4e5faa185f59bec48258dd5f8dd0f6ad4b30099cb32f5d
```

The HNSW builder uses randomized layer assignment, so regeneration is expected
to produce a byte-different SQLite file. Tests assert the physical contents and
current-generation admission refusal; the retired in-place v3 upgrader is no
longer exercised. The SHA-256 above protects the particular committed canary.

## Historical regeneration

The retained generator documents the synthetic contents and old construction
path. It requires the historical Mneme and Mnestic 0.8.6 source APIs; this
release does not contain that old workspace or promise regeneration from its
Git history. The checked-in canary and hashes are the reproducible inputs for
current admission tests. Regenerating with a current implementation would not
produce an equivalent prior-generation fixture.

The old vendor required its `rayon` feature in that build configuration. That
was a historical feature-hygiene quirk; current Mneme does not need Rayon for
HNSW.
