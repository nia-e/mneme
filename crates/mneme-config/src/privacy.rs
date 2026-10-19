//! Privacy ordering after admission, with one explicit raw-DTO merge boundary.

use crate::{Result, error};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PrivacyProfile {
    #[default]
    Default,
    Private,
    Isolated,
}

impl PrivacyProfile {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "default" => Ok(Self::Default),
            "private" => Ok(Self::Private),
            "isolated" => Ok(Self::Isolated),
            _ => Err(error("profile must be default, private or isolated")),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Private => "private",
            Self::Isolated => "isolated",
        }
    }

    pub(crate) fn narrow(self, other: Self) -> Self {
        self.max(other)
    }
}

/// `Config::merge_project` accepts raw DTOs and validates the merged result, not
/// both inputs. Preserve its precedence for unvalidated strings: an unknown
/// value compares like default, but remains unchanged if selected so that final
/// validation rejects it. A discarded invalid input was never admitted policy.
pub(crate) fn narrow_unvalidated_profiles(
    parent: Option<String>,
    overlay: Option<String>,
) -> Option<String> {
    let precedence = |raw: &str| PrivacyProfile::parse(raw).unwrap_or_default();
    if overlay.as_ref().is_some_and(|new| {
        parent
            .as_ref()
            .is_none_or(|old| precedence(new) > precedence(old))
    }) {
        overlay
    } else {
        parent
    }
}
