//! Read-only observed semantic tag membership, not a taxonomy or truth score.
use crate::{
    NodeId,
    ports::{Error, Result},
};
use serde::{Deserialize, Serialize};

pub const MAX_TAG_VOCABULARY_ITEMS: usize = 64;
pub const MAX_TAG_VOCABULARY_SEEKS: usize = 256;
pub const MAX_TAG_VOCABULARY_MEMBERSHIPS: usize = 4096;
pub const MAX_TAG_VOCABULARY_EXAMPLES: usize = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagVocabularyStatus {
    Active,
    Archived,
    #[default]
    All,
}

impl TagVocabularyStatus {
    pub fn allows(self, status: crate::NodeStatus) -> bool {
        match self {
            Self::All => true,
            Self::Active => status == crate::NodeStatus::Active,
            Self::Archived => status == crate::NodeStatus::Archived,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagVocabularyRequest {
    pub prefix: String,
    pub status: TagVocabularyStatus,
    /// Last examined name, not a transaction snapshot or owner-bound cursor.
    pub after: Option<String>,
    pub limit: usize,
}
impl TagVocabularyRequest {
    pub fn validate(&self) -> Result<()> {
        if self.prefix.len() > crate::MAX_TAG_BYTES || self.prefix.chars().any(char::is_control) {
            return Err(Error::InvalidInput(
                "tag prefix exceeds byte bound or contains controls".into(),
            ));
        }
        if !(1..=MAX_TAG_VOCABULARY_ITEMS).contains(&self.limit) {
            return Err(Error::InvalidInput(
                "tag vocabulary limit must be 1..=64".into(),
            ));
        }
        if let Some(after) = &self.after {
            crate::validate_tag(after).map_err(|e| Error::InvalidInput(e.to_string()))?;
            if !after.starts_with(&self.prefix) {
                return Err(Error::InvalidInput(
                    "tag vocabulary progress does not match prefix".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TagVocabularyCount {
    Exact { value: usize },
    LowerBound { value: usize },
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagVocabularyItem {
    pub name: String,
    pub count: TagVocabularyCount,
    pub examples: Vec<NodeId>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagVocabularyStop {
    #[default]
    Exhausted,
    ItemLimit,
    SeekBudget,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagVocabularyWork {
    /// Every distinct-name seek and physical status probe, including lookahead.
    pub name_seeks: usize,
    pub membership_rows: usize,
    pub stopped: TagVocabularyStop,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagVocabularyPage {
    pub items: Vec<TagVocabularyItem>,
    /// Resume after this examined name. A conservative continuation can end empty.
    pub next: Option<String>,
    pub work: TagVocabularyWork,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_exact_prefix_and_progress_admission() {
        let mut request = TagVocabularyRequest {
            prefix: "People/".into(),
            status: TagVocabularyStatus::All,
            after: Some("People/ExactHandle".into()),
            limit: 64,
        };
        assert!(request.validate().is_ok());
        request.after = Some("people/lowercase".into());
        assert!(request.validate().is_err());
        request.after = None;
        for prefix in ["x".repeat(crate::MAX_TAG_BYTES + 1), "bad\n".into()] {
            request.prefix = prefix;
            assert!(request.validate().is_err());
        }
        request.prefix.clear();
        for limit in [0, 65] {
            request.limit = limit;
            assert!(request.validate().is_err());
        }
        assert_ne!(
            serde_json::to_value(TagVocabularyCount::Exact { value: 0 }).unwrap(),
            serde_json::to_value(TagVocabularyCount::Unavailable).unwrap()
        );
    }
}
