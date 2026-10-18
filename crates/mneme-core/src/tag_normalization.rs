//! Lexical normalization, separate from exact submitted and historical tags.
//!
//! A source proof hashes the submitted spellings, not their normalized result.
//! Retained node/snapshot decoders therefore keep using `BoundedTagSet` without
//! rewriting their input. New materialization and the explicit tag migration
//! call this module; protected membership is checked on the normalized result.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::{BoundedTagSet, MAX_NODE_TAGS, MAX_TAG_BYTES, TagValidationError};

/// Identifies the deterministic lexical policy, not a migration receipt.
pub const TAG_NORMALIZATION_VERSION: &str = "ascii-slug-v1";

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TagNormalizationError {
    #[error(transparent)]
    InvalidInput(#[from] TagValidationError),
    #[error(
        "tag contains unsupported character {character:?} at byte {byte_offset}; use ASCII letters, digits, or separators (space, -, _, :, /, .), or choose an explicit alias"
    )]
    UnsupportedCharacter { character: char, byte_offset: usize },
    #[error("tag has no letters or digits after normalization")]
    EmptyResult,
}

fn validate_submitted_tag(tag: &str) -> Result<(), TagValidationError> {
    if tag.is_empty() {
        return Err(TagValidationError::Empty);
    }
    if tag.len() > MAX_TAG_BYTES {
        return Err(TagValidationError::TooLong { actual: tag.len() });
    }
    if tag.chars().any(char::is_control) {
        return Err(TagValidationError::ControlCharacter);
    }
    Ok(())
}

/// Bounded, sorted, exact request spellings, before lexical normalization.
///
/// This deliberately accepts old Unicode/punctuation spellings for exact replay
/// and allows surrounding literal spaces in new input. It is not a canonical
/// node tag set. Only `normalize_tags` may turn new intent into stored tags.
/// No deserializer silently substitutes this type for historical tag fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubmittedTagSet(Arc<[String]>);

impl SubmittedTagSet {
    pub fn try_from_iter<I, T>(tags: I) -> Result<Self, TagValidationError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        let mut submitted = BTreeSet::new();
        for (index, tag) in tags.into_iter().enumerate() {
            if index == MAX_NODE_TAGS {
                return Err(TagValidationError::TooMany);
            }
            let tag = tag.as_ref();
            validate_submitted_tag(tag)?;
            if !submitted.insert(tag.to_owned()) {
                return Err(TagValidationError::Duplicate {
                    tag: tag.to_owned(),
                });
            }
        }
        Ok(Self(Arc::from(submitted.into_iter().collect::<Vec<_>>())))
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.0
            .binary_search_by(|value| value.as_str().cmp(tag))
            .is_ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Normalize one input spelling to lowercase ASCII letters/digits and dashes.
///
/// Literal spaces, underscores, colons, slashes, dots and dashes are separators.
/// Runs collapse and edge separators disappear. Controls, other punctuation and
/// non-ASCII characters reject rather than silently erase meaningful text.
/// Aliases such as `C++ -> cpp` are explicit caller/migration decisions, not
/// transliteration performed by this lexical policy.
pub fn normalize_tag(tag: &str) -> Result<String, TagNormalizationError> {
    validate_submitted_tag(tag)?;
    let mut output = String::with_capacity(tag.len());
    let mut separator = false;
    for (byte_offset, character) in tag.char_indices() {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('-');
            }
            separator = false;
            output.push(character.to_ascii_lowercase());
        } else if matches!(character, ' ' | '-' | '_' | ':' | '/' | '.') {
            separator = true;
        } else {
            return Err(TagNormalizationError::UnsupportedCharacter {
                character,
                byte_offset,
            });
        }
    }
    if output.is_empty() {
        return Err(TagNormalizationError::EmptyResult);
    }
    Ok(output)
}

/// Normalize a complete input set and coalesce spellings of the same tag.
///
/// Count/byte bounds apply to raw input, before coalescing, so duplicates cannot
/// turn a bounded tag set into unbounded normalization work. Source-keyed callers
/// retain a separate `SubmittedTagSet` for their exact request proof.
pub fn normalize_tags<I, T>(tags: I) -> Result<BoundedTagSet, TagNormalizationError>
where
    I: IntoIterator<Item = T>,
    T: AsRef<str>,
{
    let mut normalized = BTreeSet::new();
    for (index, tag) in tags.into_iter().enumerate() {
        if index == MAX_NODE_TAGS {
            return Err(TagValidationError::TooMany.into());
        }
        normalized.insert(normalize_tag(tag.as_ref())?);
    }
    Ok(BoundedTagSet::try_from_iter(normalized)?)
}

/// Whether a spelling is already the exact canonical output of this policy.
pub fn is_normalized_tag(tag: &str) -> bool {
    if tag.is_empty() || tag.len() > MAX_TAG_BYTES {
        return false;
    }
    let mut previous_dash = true;
    for byte in tag.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            previous_dash = false;
        } else if byte == b'-' && !previous_dash {
            previous_dash = true;
        } else {
            return false;
        }
    }
    !previous_dash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_only_the_declared_ascii_separators() {
        for (raw, expected) in [
            ("Rust Compiler", "rust-compiler"),
            (" rust__Compiler / 1.82:API-- ", "rust-compiler-1-82-api"),
            ("Example-Handle", "example-handle"),
            ("Core", "core"),
            ("A0", "a0"),
        ] {
            assert_eq!(normalize_tag(raw).unwrap(), expected);
            assert!(is_normalized_tag(expected));
            assert_eq!(normalize_tag(expected).unwrap(), expected);
        }
    }

    #[test]
    fn refuses_erasure_transliteration_controls_and_empty_results() {
        for tag in [
            "C++",
            "C#",
            "F#",
            "café",
            "İstanbul",
            "東京",
            "foo\u{a0}bar",
            "x\ty",
            "x\ny",
            "x\0y",
            "",
            "---",
            " _:/.",
        ] {
            assert!(normalize_tag(tag).is_err(), "{tag:?}");
            assert!(!is_normalized_tag(tag), "{tag:?}");
        }
        for tag in ["Core", "a--b", "-a", "a-", "a_b", "a b"] {
            assert!(!is_normalized_tag(tag), "{tag:?}");
        }
    }

    #[test]
    fn charges_raw_input_before_collapsing_aliases() {
        let tags = normalize_tags(["Rust Compiler", "rust_compiler", "rust-compiler"]).unwrap();
        assert_eq!(tags.iter().collect::<Vec<_>>(), ["rust-compiler"]);
        assert!(normalize_tags(std::iter::repeat_n("rust", MAX_NODE_TAGS)).is_ok());
        assert!(normalize_tags(std::iter::repeat_n("rust", MAX_NODE_TAGS + 1)).is_err());
        assert!(normalize_tag(&"A".repeat(MAX_TAG_BYTES)).is_ok());
        assert!(normalize_tag(&" ".repeat(MAX_TAG_BYTES + 1)).is_err());
        assert!(
            normalize_tags(std::iter::empty::<&str>())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn submitted_tags_preserve_exact_proof_preimages() {
        let raw =
            SubmittedTagSet::try_from_iter(["Rust Compiler", "C++", "café", " Core "]).unwrap();
        assert_eq!(
            raw.iter().collect::<Vec<_>>(),
            [" Core ", "C++", "Rust Compiler", "café"]
        );
        assert!(raw.contains("C++"));
        assert!(!raw.contains("cpp"));
        assert!(normalize_tags(raw.iter()).is_err());
        assert!(SubmittedTagSet::try_from_iter(["a", "a"]).is_err());
        assert!(SubmittedTagSet::try_from_iter(["a\nb"]).is_err());
        // Historical decoding remains exact, never normalization on read.
        let legacy: BoundedTagSet = serde_json::from_str(r#"["Rust Compiler","café"]"#).unwrap();
        assert_eq!(legacy.iter().collect::<Vec<_>>(), ["Rust Compiler", "café"]);
        assert_eq!(
            serde_json::to_string(&legacy).unwrap(),
            r#"["Rust Compiler","café"]"#
        );
    }

    #[test]
    fn normalized_predicate_matches_the_transform_for_ascii_inputs() {
        for first in 0..=127_u8 {
            for second in 0..=127_u8 {
                let input = String::from_utf8(vec![first, second]).unwrap();
                assert_eq!(
                    is_normalized_tag(&input),
                    normalize_tag(&input).is_ok_and(|v| v == input)
                );
            }
        }
    }
}
