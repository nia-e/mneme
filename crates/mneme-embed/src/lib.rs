//! mneme-embed — the [`Embedder`] adapter.
//!
//! Ships [`HashingEmbedder`]: a deterministic, dependency-free bag-of-words
//! embedder. Tokens are hashed into a fixed number of buckets (the "hashing
//! trick"), counted, and L2-normalized, so cosine similarity between two
//! summaries tracks their token overlap. It needs no model download, no ONNX
//! runtime, and no network — which is what lets the whole system run in a bare
//! sandbox and what makes tests deterministic.
//!
//! It is a stand-in, not a semantic model: it captures lexical overlap, not
//! meaning ("car" and "automobile" look unrelated). The production embedder is a
//! `FastEmbed` adapter wrapping a real sentence-transformer; it implements the
//! same [`Embedder`] port, so swapping it in is a one-line change at the daemon
//! and a dimension change in the vector index. The default dim here is 768 to
//! match BGE-base (and the cozo `node_vec` relation).

use async_trait::async_trait;
use mneme_core::EmbeddingFingerprint;
use mneme_core::ports::{Embedder, Result};

pub const DEFAULT_DIM: usize = 768;

/// Stable vector-space identity for the dependency-free lexical embedder. Any
/// change to tokenization, hashing, weighting, or normalization must bump the
/// implementation suffix even when the output dimension stays unchanged.
pub fn hashing_fingerprint(dim: usize) -> EmbeddingFingerprint {
    EmbeddingFingerprint::new(
        "mneme:hashing-fnv1a64-token-count-lower-alnum-v1",
        dim,
        "l2-f32-v1",
        "symmetric-document-v1",
    )
}

#[cfg(feature = "fastembed")]
pub use fast_embed::{FastEmbedder, fingerprint as fastembed_fingerprint};

#[cfg(feature = "fastembed")]
mod fast_embed {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use mneme_core::EmbeddingFingerprint;
    use mneme_core::ports::{Embedder, Error, Result};

    /// hf-hub's on-disk repo dir for [`EmbeddingModel::BGEBaseENV15`]
    /// (`Xenova/bge-base-en-v1.5`) under a cache root.
    const MODEL_REPO: &str = "models--Xenova--bge-base-en-v1.5";

    /// Stable identity of the model weights plus Mneme's document/query adapter.
    /// This ID is the compatibility promise: bump it if the upstream artifact,
    /// pooling/export, or either side's preprocessing changes. Keeping it separate
    /// from the crate version avoids needless rebuilds for behavior-neutral bumps.
    pub fn fingerprint() -> EmbeddingFingerprint {
        EmbeddingFingerprint::new(
            "fastembed:Xenova/bge-base-en-v1.5:onnx-fp32:mneme-adapter-v1",
            super::DEFAULT_DIM,
            "l2-f32-v1",
            "bge-v1.5-search-instruction-v1",
        )
    }

    /// Real sentence-transformer embeddings via fastembed + the ONNX runtime.
    /// Uses BGE-base-en-v1.5 — 768-dim, matching [`super::DEFAULT_DIM`]. The model
    /// is downloaded and cached on first construction. Behind the `fastembed`
    /// feature because it pulls `ort`/onnxruntime.
    pub struct FastEmbedder {
        model: TextEmbedding,
    }

    impl FastEmbedder {
        /// Load the model (served from cache; downloads on first use).
        pub fn new() -> Result<Self> {
            let cache = resolve_cache_dir();
            let mut opts = InitOptions::new(EmbeddingModel::BGEBaseENV15)
                // Never render the download/progress bar: it writes to stdout,
                // which would corrupt a stdio JSON-RPC stream (the mneme-mcp
                // server).
                .with_show_download_progress(false);
            if let Some(dir) = &cache {
                opts = opts.with_cache_dir(dir.clone());
            }
            TextEmbedding::try_new(opts)
                .map(|model| Self { model })
                // The bare fastembed error ("Failed to retrieve onnx/model.onnx")
                // hides *where* it looked — name the resolved cache so an offline
                // server (e.g. a sandboxed mneme-mcp) is debuggable.
                .map_err(|e| {
                    Error::Backend(format!("fastembed init: {e} — {}", cache_hint(&cache)))
                })
        }

        /// Synchronous ONNX inference. One-shot hosts may call the async port
        /// directly; long-lived async hosts use this inside `spawn_blocking` so
        /// CPU inference never pins a Tokio worker.
        pub fn embed_sync(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            // fastembed L2-normalizes, matching the index's cosine assumption.
            self.model
                .embed(texts.to_vec(), None)
                .map_err(|e| Error::Backend(format!("fastembed embed: {e}")))
        }

        /// Synchronous query-side adapter plus inference; see [`Self::embed_sync`].
        pub fn embed_query_sync(&self, query: &str) -> Result<Vec<f32>> {
            let prefixed = format!("{BGE_QUERY_PREFIX}{query}");
            self.embed_sync(&[&prefixed])?
                .into_iter()
                .next()
                .ok_or_else(|| Error::Backend("fastembed returned no query vector".into()))
        }
    }

    /// Human note for an init failure: the cache the model was sought in and what
    /// to do. `None` means an `HF_HOME`/`FASTEMBED_CACHE_DIR` env is steering it.
    fn cache_hint(cache: &Option<PathBuf>) -> String {
        match cache {
            Some(dir) => format!(
                "looked in {} (no {MODEL_REPO}/…/onnx/model.onnx, and couldn't download); \
                 populate it by running one query with network, or point \
                 HF_HOME at a populated cache",
                dir.display()
            ),
            None => {
                "using HF_HOME/FASTEMBED_CACHE_DIR; check it points at a populated cache".into()
            }
        }
    }

    /// Pick the fastembed cache directory, so a model fetched once is found from
    /// any working directory (and an installed binary doesn't re-download per
    /// CWD). Returns `None` to leave fastembed's own default in place.
    ///
    /// Precedence:
    /// 1. An explicit `HF_HOME` / `FASTEMBED_CACHE_DIR` env wins — fastembed and
    ///    hf-hub already honor those (the mcpb bundle sets `HF_HOME` at a vendored
    ///    copy), so don't second-guess the operator.
    /// 2. A populated **local** `./.fastembed_cache` (a repo checkout, or a CWD
    ///    bundle) — keeps an already-downloaded model working with no surprise
    ///    relocation.
    /// 3. A populated **global** `$XDG_DATA_HOME/mneme/.fastembed_cache` — the
    ///    shared per-user store, checked before any download so an offline CWD
    ///    without a local copy still resolves.
    /// 4. Neither populated → where a fresh download lands: the local cache for a
    ///    dev (debug) build, the shared global store for an installed (release)
    ///    one.
    pub(crate) fn resolve_cache_dir() -> Option<PathBuf> {
        if std::env::var_os("HF_HOME").is_some()
            || std::env::var_os("FASTEMBED_CACHE_DIR").is_some()
        {
            return None;
        }
        let local = PathBuf::from(".fastembed_cache");
        if model_present(&local) {
            return Some(local);
        }
        let global = global_cache_dir();
        if global.as_deref().is_some_and(model_present) {
            return global;
        }
        // Nothing cached yet — choose where the download should land.
        if cfg!(debug_assertions) {
            Some(local)
        } else {
            global
        }
    }

    /// Whether `dir` holds the *complete* model, not just a `refs/main` stub: read
    /// the pinned revision and confirm its snapshot has the ONNX file. (A cache
    /// with refs but a half-populated snapshot would otherwise be picked and then
    /// fail the offline `get`.)
    fn model_present(dir: &std::path::Path) -> bool {
        let repo = dir.join(MODEL_REPO);
        let Ok(rev) = std::fs::read_to_string(repo.join("refs").join("main")) else {
            return false;
        };
        repo.join("snapshots")
            .join(rev.trim())
            .join("onnx")
            .join("model.onnx")
            .exists()
    }

    /// `$XDG_DATA_HOME/mneme/.fastembed_cache` (or `~/.local/share/...`) — mirrors
    /// the db location in `mnemed`, so memory and its model sit side by side.
    fn global_cache_dir() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Some(base.join("mneme").join(".fastembed_cache"))
    }

    /// BGE's query-side instruction. The passage side (node summaries, via
    /// `embed`) gets *no* prefix; only a search query does — that asymmetry is how
    /// the model was trained, and honoring it lifts retrieval. bge-small/base/large
    /// all share this instruction, so it survives the model swap.
    const BGE_QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

    #[async_trait]
    impl Embedder for FastEmbedder {
        fn dim(&self) -> usize {
            768
        }

        fn fingerprint(&self) -> EmbeddingFingerprint {
            fingerprint()
        }

        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            // Synchronous CPU inference — fine for the one-shot CLI. fastembed
            // exposes a sync method so async hosts can choose their blocking pool.
            self.embed_sync(texts)
        }

        async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
            self.embed_query_sync(query)
        }
    }
}

#[cfg(feature = "fastembed")]
pub use fast_rerank::FastReranker;

#[cfg(feature = "fastembed")]
mod fast_rerank {
    use async_trait::async_trait;
    use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
    use mneme_core::ports::{Error, Reranker, Result};

    /// Cross-encoder reranker via fastembed + ONNX (default model
    /// `bge-reranker-base`). Behind the `fastembed` feature; construct once and
    /// reuse. Shares the embedder's cache-dir resolution so one HF cache serves both.
    pub struct FastReranker {
        model: TextRerank,
    }

    impl FastReranker {
        /// Load the reranker model (served from cache; downloads on first use).
        pub fn new() -> Result<Self> {
            let cache = crate::fast_embed::resolve_cache_dir();
            let mut opts = RerankInitOptions::new(RerankerModel::BGERerankerBase)
                // Same stdout-corruption guard as the embedder: a progress bar would
                // corrupt the mneme-mcp stdio JSON-RPC stream.
                .with_show_download_progress(false);
            if let Some(dir) = cache {
                opts = opts.with_cache_dir(dir);
            }
            TextRerank::try_new(opts)
                .map(|model| Self { model })
                .map_err(|e| Error::Backend(format!("fastembed reranker init: {e}")))
        }

        /// Synchronous cross-encoder inference for async hosts to run on their
        /// blocking pool under an explicit concurrency bound.
        pub fn rerank_sync(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            if docs.is_empty() {
                return Ok(Vec::new());
            }
            // fastembed returns results sorted by score desc; restore input order via
            // each result's `index` so the engine can zip scores back onto its cands.
            let results = self
                .model
                .rerank(query, docs.to_vec(), false, None)
                .map_err(|e| Error::Backend(format!("fastembed rerank: {e}")))?;
            let mut scores = vec![0f32; docs.len()];
            for r in results {
                if let Some(slot) = scores.get_mut(r.index) {
                    *slot = r.score;
                }
            }
            Ok(scores)
        }
    }

    #[async_trait]
    impl Reranker for FastReranker {
        fn semantic_id(&self) -> &'static str {
            "fastembed-bge-reranker-base-v1"
        }

        async fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            self.rerank_sync(query, docs)
        }
    }
}

pub struct HashingEmbedder {
    dim: usize,
}

impl HashingEmbedder {
    pub fn new(dim: usize) -> Self {
        assert!(dim > 0, "embedding dimension must be positive");
        Self { dim }
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for token in tokenize(text) {
            let bucket = (fnv1a(token.as_bytes()) as usize) % self.dim;
            v[bucket] += 1.0;
        }
        l2_normalize(&mut v);
        v
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(DEFAULT_DIM)
    }
}

#[async_trait]
impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn fingerprint(&self) -> EmbeddingFingerprint {
        hashing_fingerprint(self.dim)
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

/// Lowercase, split on non-alphanumeric, drop single chars. Yields owned tokens
/// so the caller doesn't juggle borrows from a temporary lowercased string.
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .map(|t| t.to_lowercase())
}

/// FNV-1a, 64-bit. Deterministic across runs and platforms (no RNG, no
/// `RandomState`), which is exactly what a reproducible embedder needs.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_captures_more_than_dimension() {
        let lexical = HashingEmbedder::new(DEFAULT_DIM).fingerprint();
        assert_eq!(lexical.dimension, DEFAULT_DIM);
        assert_eq!(lexical.normalization, "l2-f32-v1");
        assert_eq!(lexical.query_mode, "symmetric-document-v1");

        #[cfg(feature = "fastembed")]
        {
            let semantic = fastembed_fingerprint();
            assert_eq!(semantic.dimension, lexical.dimension);
            assert_ne!(semantic.embedding_id, lexical.embedding_id);
            assert_ne!(semantic.query_mode, lexical.query_mode);
        }
    }

    #[tokio::test]
    async fn deterministic_and_normalized() {
        let e = HashingEmbedder::new(64);
        let a = e.embed(&["the quick brown fox"]).await.unwrap();
        let b = e.embed(&["the quick brown fox"]).await.unwrap();
        assert_eq!(a, b, "embedding must be deterministic");
        let norm: f32 = a[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "embedding must be L2-normalized");
    }

    #[tokio::test]
    async fn overlap_beats_disjoint() {
        let e = HashingEmbedder::new(256);
        let v = e
            .embed(&[
                "rust async runtime tokio",
                "rust async runtime executor",
                "baking sourdough bread recipe",
            ])
            .await
            .unwrap();
        let sim = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(a, b)| a * b).sum::<f32>();
        let related = sim(&v[0], &v[1]);
        let unrelated = sim(&v[0], &v[2]);
        assert!(
            related > unrelated,
            "shared tokens should score higher: {related} !> {unrelated}"
        );
    }

    #[cfg(feature = "fastembed")]
    #[tokio::test]
    #[ignore = "downloads an ONNX model on first run; run explicitly"]
    async fn fastembed_captures_meaning_not_just_tokens() {
        let e = super::FastEmbedder::new().expect("load model");
        assert_eq!(e.dim(), DEFAULT_DIM);
        let v = e
            .embed(&[
                "a feline animal",
                "a domestic cat",
                "the stock market crashed",
            ])
            .await
            .unwrap();
        let sim = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(a, b)| a * b).sum::<f32>();
        // "feline animal" and "domestic cat" share no tokens — the lexical
        // embedder scores them as unrelated; a real model must not.
        assert!(
            sim(&v[0], &v[1]) > sim(&v[0], &v[2]),
            "semantic match (feline≈cat) should beat the unrelated sentence"
        );
    }
}
