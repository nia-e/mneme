//! Shared application operations over an already admitted memory instance.
//!
//! Shared operations own capture request validation and defaults; frontends own
//! database selection and output framing. These operations neither open stores
//! nor grant feedback authority.

pub mod capture;
pub mod concern;
mod context;
pub mod episode;
pub mod graph_view;
pub mod list;
pub mod neighbors;
pub mod save;
pub mod touchstone;
pub use context::{
    ContextObservation, ContextWindow, ObservedContext, prepare_context_window, recall_context,
    recall_context_observed, recall_context_routed,
};

use mneme_engine::RetrievalBatch;
use mneme_present::{RetrievalMetadata, RetrievalMetadataError, RetrievalStamp};

/// Errors retain the underlying display text for the frontend's existing error
/// presentation. The shared operation adds no transport-specific error framing.
#[derive(thiserror::Error)]
pub enum ContextError {
    #[error(transparent)]
    Retrieval(#[from] mneme_core::ports::Error),
    #[error(transparent)]
    Metadata(#[from] RetrievalMetadataError),
    #[error(transparent)]
    Input(#[from] mneme_present::InputError),
    #[error(transparent)]
    Pack(#[from] mneme_present::PackError),
}

// `mnemed` returns a boxed error from main, so Rust's Termination path uses
// Debug, while MCP renders Display. Preserve both pre-extraction forms.
impl std::fmt::Debug for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retrieval(error) => std::fmt::Debug::fmt(error, f),
            Self::Metadata(error) => std::fmt::Debug::fmt(error, f),
            Self::Input(error) => std::fmt::Debug::fmt(error, f),
            Self::Pack(error) => std::fmt::Debug::fmt(error, f),
        }
    }
}

/// Convert retrieval diagnostics for both ordinary context and query siblings.
/// Tagged work, seed coverage and projection watermarks are preserved verbatim.
pub fn presentation_retrieval_metadata(
    batch: &RetrievalBatch,
) -> Result<RetrievalMetadata, RetrievalMetadataError> {
    let stamp = &batch.stamp;
    let stamp = RetrievalStamp::new(
        stamp.retrieval_policy.as_str(),
        stamp.retrieval_policy.fingerprint,
        stamp.index_set.embedding.clone(),
        stamp.index_set.vector_semantics,
        stamp.index_set.lexical_semantics.map(str::to_owned),
        stamp.index_set.reranker_semantics.map(str::to_owned),
        stamp
            .projection_watermarks
            .as_slice()
            .iter()
            .map(|watermark| (watermark.projection.clone(), watermark.watermark.clone()))
            .collect(),
    )?;
    match &batch.tagged {
        Some(tagged) => {
            RetrievalMetadata::tagged(stamp, tagged.work, tagged.primary_seed_coverage.clone())
        }
        None => RetrievalMetadata::untagged(stamp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_errors_preserve_cli_termination_and_mcp_display() {
        fn assert_rendering<E: std::error::Error + Into<ContextError>>(error: E) {
            let cli_debug = format!("{error:?}");
            let alternate_debug = format!("{error:#?}");
            let mcp_display = error.to_string();
            let shared: ContextError = error.into();
            assert_eq!(format!("{shared:?}"), cli_debug);
            assert_eq!(format!("{shared:#?}"), alternate_debug);
            assert_eq!(shared.to_string(), mcp_display);
        }

        assert_rendering(mneme_core::ports::Error::InvalidInput(
            "query cannot be empty".into(),
        ));
        assert_rendering(RetrievalMetadataError::MissingTagMembershipWatermark);
        assert_rendering(mneme_present::InputError::SummaryTooLarge);
    }
}

pub mod retag;

pub mod edit_body;

pub mod edit_summary;
