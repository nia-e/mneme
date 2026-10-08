# mneme-vsa

An experimental crate for synthetic vector-memory studies, not a Mneme runtime
backend. Fourier holographic reduced representations (FHRR) encode several
role–filler pairs in one complex-valued vector. The study binaries measure how
well those pairs can be recovered as more associations and distractors are added.

This crate is **not canonical memory storage** and is not wired into mneme's
runtime nodes, bodies, graph, or indexes. Treat a hologram as a rebuildable
derived index. Cleanup also needs a codebook (or the compute to regenerate one),
so the studies report vector and codebook bytes separately. Both count toward
the storage cost.

The algebra is deliberately small:

- atomic vectors: deterministic independent unit-complex coordinates;
- binding: element-wise complex multiplication;
- superposition: an `1/sqrt(n)`-scaled sum;
- unbinding: multiplication by the role's complex conjugate;
- cleanup: real cosine similarity over the flattened complex coordinates.

Every vector carries a versioned fingerprint containing the dimension, seed,
binding operator, and atom generator. Mixing incompatible vectors is rejected.

Run a concise study:

```sh
cargo run -p mneme-vsa --bin study --release
```

Emit machine-readable results (the human summary moves to stderr):

```sh
cargo run -p mneme-vsa --bin study --release -- \
  --dimensions 128,256,512 --pairs 1,4,8,16 \
  --distractors 0,16,64,256 --trials 32 --top-k 5 --format json \
  --output /tmp/fhrr.json

cargo run -p mneme-vsa --bin study --release -- --format csv
```

`--output` is available for JSON/CSV and publishes via a synced same-directory
temporary file plus rename, so an interrupted study does not leave a truncated
artifact at the requested path.

The results are a synthetic capacity/cleanup study, not evidence that FHRR
improves end-to-end agent memory. Runtime includes atom/codebook construction,
binding, bundling, unbinding, and exhaustive cleanup; it is not a calibrated
microbenchmark.

## Node-centric typed relations

`relational-study` asks the more relevant graph question. Every node has a
unique target for each globally shared typed role; one-hop queries unbind and
clean up a target, while sequential two-hop queries commit to the cleaned
top-1 intermediate before querying its next role. A bad first hop therefore
compounds instead of receiving an exact/oracle intermediate.

The report compares all node holograms **plus** the globally shared role/filler
codebook against a compact exact adjacency payload. The exact encoding is one
flat `u16` target per implicit role slot (`nodes * degree * 2` bytes); labels,
container metadata, and allocator overhead are excluded on both sides. Exact
adjacency remains canonical and the holograms remain rebuildable derived data.
Deletion/subtraction drift is deliberately not fabricated: this prototype has
no subtraction API, so rebuilding from exact adjacency is the safe protocol.

Run the default human-readable sweep:

```sh
cargo run -p mneme-vsa --bin relational-study --release
```

Write a machine-labeled JSON artifact with an atomic publish:

```sh
cargo run -p mneme-vsa --bin relational-study --release -- \
  --machine-label "local test host" --format json \
  --output /tmp/fhrr-relational.json
```

The deterministic graph, atoms, and query samples are reproducible from the
reported seed and fingerprint. Wall-clock fields are intentionally not
deterministic and should only be treated as local directional measurements.

## Lifecycle and churn

`lifecycle-study` is a numeric lifecycle-validation probe for a rebuildable,
derived FHRR bundle. It is **not evidence for an FHRR adjacency lookup**. Exact
typed adjacency remains canonical; the experiment only exercises deterministic
replacement arithmetic, incremental-`f32` drift against clean rebuilds, rebuild
cadence, empty-bundle abstention, and exhaustive cleanup on a synthetic graph.

Run the small JSON smoke configuration:

```sh
cargo run -p mneme-vsa --release --bin lifecycle-study -- \
  --dimensions 128 --nodes 64 --degrees 4 --distractors 16 \
  --operations 128 --rebuild-every 0,32 --deleted-node-percent 25 \
  --trials 2 --machine-label smoke --format json
```

`--rebuild-every 0` means never during the replacement stream. The binary
validates dimensions, graph/replacement feasibility, checked products, and a
100,000,000-work-unit ceiling before allocating the study. JSON output can be
published with `--output PATH`; it uses the same synced, same-directory
temporary file plus rename discipline as the other studies. A post-rename
directory-sync failure is reported as published with durability unconfirmed,
not as a pre-publication failure.

The mutable accumulator has no membership set. `subtract_known_term` and its
transactional replacement path are valid only when the caller proves the old
bound term from canonical adjacency/provenance; a cleanup hit never authorizes
mutation. Incremental estimates carry a versioned mutable-arithmetic provenance
tag, rather than pretending that their `f32` update history is a clean one-shot
`f64` rebuild.

Schema v2 plans replacements in an independent oracle before production state
exists. Production adjacency is constructed separately, retains exact `u16`
target provenance, carries one canonical live bit per relation, and is audited
against that oracle initially, after every replacement, at rebuild boundaries,
and after deletion. Clean references and query gold come only from the oracle.
The report also separates drift by checkpoint class, times setup and stale
probes, emits per-operation/query rates, and admits a grid only under a checked
coordinate-plus-scalar work model. Oracle validation is charged to that safety
model but excluded from benchmark timings. Clean-first and incremental-first
cleanup timing alternate deterministically so neither path always inherits the
warmed dictionary. Because atom generation is rejection-sampled and sorting is
implementation-defined, the model is not mislabeled as an instruction-count
upper bound; independent operation, trace, dictionary, dimension, and
peak-payload caps close the allocation hazards.

Direct typed adjacency is an exact `O(1)` lookup; FHRR updates are `O(D)`,
and this study's exhaustive cleanup is `O(D * C)` for dimension `D` and candidate
count `C`. The derived vectors also require a cleanup dictionary and canonical
relation provenance. This prototype therefore gives no reason to replace exact
adjacency with FHRR; its synthetic studies do not establish agent-memory quality.

There is only a plausible production follow-up if a separately designed task
needs noisy or semantic cues for *candidate generation*—not exact graph lookup.
That study would need ANN/PQ candidates and appropriately strong exact,
sparse/dense, and semantic-retrieval baselines, with their construction,
payload, quality, and end-to-end costs included.

The report charges accumulator counters, role codebooks, filler cleanup
dictionaries, exact target provenance, and canonical liveness as payload. It
separates derived-only, deployable-total, clean-reference, and peak-study
figures; none is mislabeled as resident memory. Deleted/absent relations
abstain before cleanup, so this study deliberately does not estimate their
false-activation rate.
