//! Ambient-authority vocabulary shared by invocation parsing and tool policy.
//! Profiles are process settings, not caller authentication or store leases.

type AnyErr = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum CapabilityProfile {
    ReadOnly,
    #[default]
    ReceiptGrounded,
    Curator,
    Operator,
}

impl CapabilityProfile {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReceiptGrounded => "receipt-grounded",
            Self::Curator => "curator",
            Self::Operator => "operator",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, AnyErr> {
        match value {
            "read-only" => Ok(Self::ReadOnly),
            "receipt-grounded" => Ok(Self::ReceiptGrounded),
            "curator" => Ok(Self::Curator),
            "operator" => Ok(Self::Operator),
            other => Err(format!(
                "--capability-profile must be read-only|receipt-grounded|curator|operator, got {other:?}"
            )
            .into()),
        }
    }

    pub(crate) const fn permits(self, required: CapabilityClass) -> bool {
        match (self, required) {
            (Self::ReadOnly, CapabilityClass::ReadOnly) => true,
            (
                Self::ReceiptGrounded,
                CapabilityClass::ReadOnly | CapabilityClass::ReceiptGrounded,
            ) => true,
            (
                Self::Curator,
                CapabilityClass::ReadOnly
                | CapabilityClass::ReceiptGrounded
                | CapabilityClass::Curator,
            ) => true,
            (
                Self::Operator,
                CapabilityClass::ReadOnly
                | CapabilityClass::ReceiptGrounded
                | CapabilityClass::Curator
                | CapabilityClass::Operator,
            ) => true,
            (
                Self::ReadOnly,
                CapabilityClass::ReceiptGrounded
                | CapabilityClass::Curator
                | CapabilityClass::Operator,
            )
            | (Self::ReceiptGrounded, CapabilityClass::Curator | CapabilityClass::Operator)
            | (Self::Curator, CapabilityClass::Operator) => false,
        }
    }
}

/// Minimum ambient authority required by one fully parsed public action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapabilityClass {
    ReadOnly,
    ReceiptGrounded,
    Curator,
    Operator,
}

impl CapabilityClass {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReceiptGrounded => "receipt-grounded",
            Self::Curator => "curator",
            Self::Operator => "operator",
        }
    }
}
