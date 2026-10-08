//! Pure arithmetic and admission controls for a conditional-preference experiment.
//!
//! These controls are not wired into retrieval, feedback, or storage. They do
//! not decide whether two contexts are equivalent, whether a card is useful,
//! or whether a judgment has adequate evidence. The caller chooses a canonical
//! cell and owns those decisions, task-level deduplication, and atomic storage.
//! A preference is not calibrated confidence or proof of applicability.
//! Clipping only avoids numeric extremes: identical repeated positive labels
//! still flatten every updated value to 0.95. These controls cannot manufacture
//! discrimination, qualify graph health, or recover candidates retrieval lost.

/// Experimental cell limit, not a production storage default.
pub const CONTROL_CAPACITY: usize = 4;
/// Experimental update rate; repeated contrary judgments remain corrective.
pub const CONTROL_LEARNING_RATE: f32 = 0.25;
/// Experimental clipping interval, also enforced on reconstructed values.
pub const CONTROL_MIN_VALUE: f32 = 0.05;
pub const CONTROL_MAX_VALUE: f32 = 0.95;

/// One checked scalar for an already identified canonical condition.
///
/// No context identity or evidence count is inferred from this value. In
/// particular, a saturated preference is not evidence of independent support.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConditionalPreference(f32);

impl ConditionalPreference {
    pub const NEUTRAL: Self = Self(0.5);

    /// Reconstruct a value without silently repairing invalid state.
    ///
    /// This defines no serialization or persistence format.
    pub fn from_stored(value: f32) -> Result<Self, PreferenceError> {
        if !value.is_finite() || !(CONTROL_MIN_VALUE..=CONTROL_MAX_VALUE).contains(&value) {
            return Err(PreferenceError::InvalidValue);
        }
        Ok(Self(value))
    }

    pub const fn value(self) -> f32 {
        self.0
    }

    /// Apply a judgment only to this cell. Unknown preserves the exact bits.
    pub fn updated(self, judgment: Judgment) -> Self {
        let target = match judgment {
            Judgment::Helpful => 1.0,
            Judgment::ActivelyMisled => 0.0,
            Judgment::Unknown => return self,
        };
        Self(
            (self.0 + CONTROL_LEARNING_RATE * (target - self.0))
                .clamp(CONTROL_MIN_VALUE, CONTROL_MAX_VALUE),
        )
    }
}

/// Caller-supplied attributable outcome, not a relevance or selection label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Judgment {
    Helpful,
    ActivelyMisled,
    Unknown,
}

/// A proposed local change. Nothing is persisted or evicted by this module.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PreferenceDecision {
    /// An unknown judgment: preserve both the existing value and its absence.
    NoChange,
    /// Replace only the chosen existing cell; clipping may leave its value equal.
    Update(ConditionalPreference),
    /// Allocate one cell, starting at neutral and applying the first judgment.
    Admit(ConditionalPreference),
    /// Preserve all incumbents and leave the new condition without a cell.
    RejectFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreferenceError {
    InvalidValue,
    InvalidCellCount,
    ExistingCellWithEmptyCount,
}

impl std::fmt::Display for PreferenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidValue => "conditional preference must be finite and within [0.05, 0.95]",
            Self::InvalidCellCount => "conditional preference cell count exceeds four",
            Self::ExistingCellWithEmptyCount => "existing conditional preference requires a cell",
        })
    }
}

impl std::error::Error for PreferenceError {}

/// Decide a single update or full-reject admission against a caller's snapshot.
///
/// `occupied_cells` includes `current` when present. The caller must resolve
/// the canonical condition before calling, and must revalidate this snapshot
/// if it later commits the decision. This is not admission authority or a CAS.
/// No judgment changes another cell, frees a slot, or changes a base edge.
pub fn decide_update(
    current: Option<ConditionalPreference>,
    occupied_cells: usize,
    judgment: Judgment,
) -> Result<PreferenceDecision, PreferenceError> {
    if occupied_cells > CONTROL_CAPACITY {
        return Err(PreferenceError::InvalidCellCount);
    }
    if current.is_some() && occupied_cells == 0 {
        return Err(PreferenceError::ExistingCellWithEmptyCount);
    }
    if judgment == Judgment::Unknown {
        return Ok(PreferenceDecision::NoChange);
    }
    if let Some(value) = current {
        return Ok(PreferenceDecision::Update(value.updated(judgment)));
    }
    if occupied_cells == CONTROL_CAPACITY {
        return Ok(PreferenceDecision::RejectFull);
    }
    Ok(PreferenceDecision::Admit(
        ConditionalPreference::NEUTRAL.updated(judgment),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_values_are_checked_not_repaired() {
        for value in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.1,
            -0.0,
            0.0,
            0.049,
            0.951,
            1.0,
        ] {
            assert_eq!(
                ConditionalPreference::from_stored(value),
                Err(PreferenceError::InvalidValue)
            );
        }
        for value in [CONTROL_MIN_VALUE, 0.5, CONTROL_MAX_VALUE] {
            assert_eq!(
                ConditionalPreference::from_stored(value).unwrap().value(),
                value
            );
        }
    }

    #[test]
    fn signed_updates_are_monotone_on_a_dense_grid() {
        for step in 0..=10_000 {
            let raw = (CONTROL_MIN_VALUE
                + (CONTROL_MAX_VALUE - CONTROL_MIN_VALUE) * step as f32 / 10_000.0)
                .clamp(CONTROL_MIN_VALUE, CONTROL_MAX_VALUE);
            let original = ConditionalPreference::from_stored(raw).unwrap();
            let helpful = original.updated(Judgment::Helpful).value();
            let misled = original.updated(Judgment::ActivelyMisled).value();
            assert!(helpful >= raw);
            assert!(misled <= raw);
            assert!((CONTROL_MIN_VALUE..=CONTROL_MAX_VALUE).contains(&helpful));
            assert!((CONTROL_MIN_VALUE..=CONTROL_MAX_VALUE).contains(&misled));
            assert_eq!(
                original.updated(Judgment::Unknown).value().to_bits(),
                raw.to_bits()
            );
        }
    }

    #[test]
    fn thousands_of_repetitions_saturate_without_zero_or_one() {
        for (judgment, boundary) in [
            (Judgment::Helpful, CONTROL_MAX_VALUE),
            (Judgment::ActivelyMisled, CONTROL_MIN_VALUE),
        ] {
            let mut value = ConditionalPreference::NEUTRAL;
            for _ in 0..10_000 {
                value = value.updated(judgment);
                assert!(value.value() > 0.0 && value.value() < 1.0);
            }
            assert_eq!(value.value(), boundary);
        }
    }

    #[test]
    fn three_opposite_judgments_reverse_either_saturated_preference() {
        let mut positive = ConditionalPreference::from_stored(CONTROL_MAX_VALUE).unwrap();
        let mut negative = ConditionalPreference::from_stored(CONTROL_MIN_VALUE).unwrap();
        for _ in 0..2 {
            positive = positive.updated(Judgment::ActivelyMisled);
            negative = negative.updated(Judgment::Helpful);
        }
        assert!(positive.value() > 0.5 && negative.value() < 0.5);
        positive = positive.updated(Judgment::ActivelyMisled);
        negative = negative.updated(Judgment::Helpful);
        assert!(positive.value() < 0.5 && negative.value() > 0.5);
    }

    #[test]
    fn unknown_preserves_absence_and_existing_cells_even_when_full() {
        for occupied in 0..=CONTROL_CAPACITY {
            assert_eq!(
                decide_update(None, occupied, Judgment::Unknown),
                Ok(PreferenceDecision::NoChange)
            );
            if occupied > 0 {
                assert_eq!(
                    decide_update(
                        Some(ConditionalPreference::NEUTRAL),
                        occupied,
                        Judgment::Unknown
                    ),
                    Ok(PreferenceDecision::NoChange)
                );
            }
        }
    }

    #[test]
    fn admission_starts_neutral_then_applies_the_judgment() {
        for occupied in 0..CONTROL_CAPACITY {
            for (judgment, expected) in [
                (Judgment::Helpful, 0.625),
                (Judgment::ActivelyMisled, 0.375),
            ] {
                assert_eq!(
                    decide_update(None, occupied, judgment),
                    Ok(PreferenceDecision::Admit(
                        ConditionalPreference::from_stored(expected).unwrap()
                    ))
                );
            }
        }
    }

    #[test]
    fn fifth_admission_rejected_but_existing_cells_remain_correctable() {
        let original = [ConditionalPreference::NEUTRAL; CONTROL_CAPACITY];
        let mut cells = original;
        assert_eq!(
            decide_update(None, cells.len(), Judgment::Helpful),
            Ok(PreferenceDecision::RejectFull)
        );
        assert_eq!(
            decide_update(None, cells.len(), Judgment::ActivelyMisled),
            Ok(PreferenceDecision::RejectFull)
        );
        for _ in 0..1000 {
            match decide_update(Some(cells[2]), cells.len(), Judgment::ActivelyMisled).unwrap() {
                PreferenceDecision::Update(value) => cells[2] = value,
                other => panic!("existing cell was not updated: {other:?}"),
            }
        }
        assert_eq!(cells[2].value(), CONTROL_MIN_VALUE);
        for index in [0, 1, 3] {
            assert_eq!(
                cells[index].value().to_bits(),
                original[index].value().to_bits()
            );
        }
        for _ in 0..3 {
            match decide_update(Some(cells[2]), cells.len(), Judgment::Helpful).unwrap() {
                PreferenceDecision::Update(value) => cells[2] = value,
                other => panic!("existing cell was not updated: {other:?}"),
            }
        }
        assert!(cells[2].value() > 0.5);
    }

    #[test]
    fn invalid_counts_are_rejected_even_for_unknown() {
        for judgment in [
            Judgment::Helpful,
            Judgment::ActivelyMisled,
            Judgment::Unknown,
        ] {
            for current in [None, Some(ConditionalPreference::NEUTRAL)] {
                for count in [CONTROL_CAPACITY + 1, usize::MAX] {
                    assert_eq!(
                        decide_update(current, count, judgment),
                        Err(PreferenceError::InvalidCellCount)
                    );
                }
            }
            assert_eq!(
                decide_update(Some(ConditionalPreference::NEUTRAL), 0, judgment),
                Err(PreferenceError::ExistingCellWithEmptyCount)
            );
        }
    }
}
