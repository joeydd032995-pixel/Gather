//! Typed reason codes: why an inference was routed to review or blocked.
//!
//! Every non-automatic outcome names one or more of these, so a certificate
//! can be filtered, counted and explained without parsing prose. Each code
//! also carries a plain-language sentence for the UI.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReasonCode {
    /// Members are linked only through a third item, not to each other.
    ChainedSimilarity,
    /// At least one pair inside a would-be group lacks direct qualifying evidence.
    PairwiseEvidenceGap,
    /// Both items carry explicit, different types.
    EntityTypeMismatch,
    /// One item is untyped and the other typed: the type cannot be confirmed.
    EntityTypeUncertain,
    /// Explicit context (employer, location, sense, scope) differs.
    ContextScopeMismatch,
    /// One side has scope/metric context the other lacks.
    ContextScopeUnknown,
    /// Not enough time information to align the two claims.
    TimeScopeUnknown,
    /// The claims describe periods that do not overlap.
    TimeWindowsNonOverlapping,
    /// A later statement of current state replaces an earlier one.
    TemporalSuccession,
    /// Supporting sources are copies or derivations of one another.
    SourceNotIndependent,
    /// A derived artifact repeats its origin; counted once.
    DerivedSourceDuplication,
    /// The negation of the statement could not be read reliably.
    NegationAmbiguity,
    /// The claims differ in modality (done / planned / possible / conditional).
    ModalityMismatch,
    /// Values are in units that cannot be compared without conversion.
    UnitNormalizationRequired,
    /// The claims are at different levels of detail (Missouri vs a city in it).
    GranularityMismatch,
    /// Two extractor/model versions disagree.
    ModelDisagreement,
    /// The user already marked these as different / not a duplicate / not a conflict.
    UserRejectionExists,
    /// A prior automatic conclusion must be withdrawn but cannot be undone automatically.
    RetractionRequired,
    /// No source artifact backs the conclusion.
    InsufficientProvenance,
    /// A weak, generic name or image that resembles too many different items.
    GenericIdentifier,
    /// An item links to more candidates than a real duplicate plausibly would.
    HubDegreeExceeded,
    /// The linked group is too large to act on automatically.
    ComponentTooLarge,
}

impl ReasonCode {
    pub const ALL: &'static [ReasonCode] = &[
        ReasonCode::ChainedSimilarity,
        ReasonCode::PairwiseEvidenceGap,
        ReasonCode::EntityTypeMismatch,
        ReasonCode::EntityTypeUncertain,
        ReasonCode::ContextScopeMismatch,
        ReasonCode::ContextScopeUnknown,
        ReasonCode::TimeScopeUnknown,
        ReasonCode::TimeWindowsNonOverlapping,
        ReasonCode::TemporalSuccession,
        ReasonCode::SourceNotIndependent,
        ReasonCode::DerivedSourceDuplication,
        ReasonCode::NegationAmbiguity,
        ReasonCode::ModalityMismatch,
        ReasonCode::UnitNormalizationRequired,
        ReasonCode::GranularityMismatch,
        ReasonCode::ModelDisagreement,
        ReasonCode::UserRejectionExists,
        ReasonCode::RetractionRequired,
        ReasonCode::InsufficientProvenance,
        ReasonCode::GenericIdentifier,
        ReasonCode::HubDegreeExceeded,
        ReasonCode::ComponentTooLarge,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReasonCode::ChainedSimilarity => "CHAINED_SIMILARITY",
            ReasonCode::PairwiseEvidenceGap => "PAIRWISE_EVIDENCE_GAP",
            ReasonCode::EntityTypeMismatch => "ENTITY_TYPE_MISMATCH",
            ReasonCode::EntityTypeUncertain => "ENTITY_TYPE_UNCERTAIN",
            ReasonCode::ContextScopeMismatch => "CONTEXT_SCOPE_MISMATCH",
            ReasonCode::ContextScopeUnknown => "CONTEXT_SCOPE_UNKNOWN",
            ReasonCode::TimeScopeUnknown => "TIME_SCOPE_UNKNOWN",
            ReasonCode::TimeWindowsNonOverlapping => "TIME_WINDOWS_NON_OVERLAPPING",
            ReasonCode::TemporalSuccession => "TEMPORAL_SUCCESSION",
            ReasonCode::SourceNotIndependent => "SOURCE_NOT_INDEPENDENT",
            ReasonCode::DerivedSourceDuplication => "DERIVED_SOURCE_DUPLICATION",
            ReasonCode::NegationAmbiguity => "NEGATION_AMBIGUITY",
            ReasonCode::ModalityMismatch => "MODALITY_MISMATCH",
            ReasonCode::UnitNormalizationRequired => "UNIT_NORMALIZATION_REQUIRED",
            ReasonCode::GranularityMismatch => "GRANULARITY_MISMATCH",
            ReasonCode::ModelDisagreement => "MODEL_DISAGREEMENT",
            ReasonCode::UserRejectionExists => "USER_REJECTION_EXISTS",
            ReasonCode::RetractionRequired => "RETRACTION_REQUIRED",
            ReasonCode::InsufficientProvenance => "INSUFFICIENT_PROVENANCE",
            ReasonCode::GenericIdentifier => "GENERIC_IDENTIFIER",
            ReasonCode::HubDegreeExceeded => "HUB_DEGREE_EXCEEDED",
            ReasonCode::ComponentTooLarge => "COMPONENT_TOO_LARGE",
        }
    }

    pub fn parse(s: &str) -> Option<ReasonCode> {
        ReasonCode::ALL.iter().copied().find(|c| c.as_str() == s)
    }

    /// One sentence a person can read without knowing the implementation.
    pub fn plain_language(self) -> &'static str {
        match self {
            ReasonCode::ChainedSimilarity => {
                "These items are each similar to a third record, but not clearly to each other."
            }
            ReasonCode::PairwiseEvidenceGap => {
                "There isn't direct evidence that every item in the group is the same thing."
            }
            ReasonCode::EntityTypeMismatch => "They are different kinds of thing.",
            ReasonCode::EntityTypeUncertain => {
                "One of them has no known type, so they may be different kinds of thing."
            }
            ReasonCode::ContextScopeMismatch => {
                "Their details differ (for example employer, place or scope)."
            }
            ReasonCode::ContextScopeUnknown => {
                "One statement names a scope the other doesn't, so they may measure different things."
            }
            ReasonCode::TimeScopeUnknown => "It isn't clear the claims refer to the same time.",
            ReasonCode::TimeWindowsNonOverlapping => "These claims refer to different time periods.",
            ReasonCode::TemporalSuccession => {
                "The newer statement describes a later state; the older one is kept as history."
            }
            ReasonCode::SourceNotIndependent => {
                "Confidence not increased: these files come from the same original source."
            }
            ReasonCode::DerivedSourceDuplication => {
                "This file is a copy or summary of another, so it counts once."
            }
            ReasonCode::NegationAmbiguity => {
                "The wording makes it unclear whether this is stated or denied."
            }
            ReasonCode::ModalityMismatch => {
                "One describes something done, the other something planned, possible or hypothetical."
            }
            ReasonCode::UnitNormalizationRequired => {
                "The values use units that can't be compared automatically."
            }
            ReasonCode::GranularityMismatch => {
                "One is more specific than the other (for example a city inside a state)."
            }
            ReasonCode::ModelDisagreement => "Two versions of the extractor read this differently.",
            ReasonCode::UserRejectionExists => "You already said these are different.",
            ReasonCode::RetractionRequired => {
                "An earlier automatic decision no longer holds and needs your attention to undo."
            }
            ReasonCode::InsufficientProvenance => "No source file backs this conclusion.",
            ReasonCode::GenericIdentifier => {
                "The name or image is too generic to identify one specific thing."
            }
            ReasonCode::HubDegreeExceeded => {
                "It resembles many different items, so resemblance alone says little."
            }
            ReasonCode::ComponentTooLarge => "Too many items are linked to decide automatically.",
        }
    }
}

impl std::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_through_strings_and_serde() {
        for &code in ReasonCode::ALL {
            assert_eq!(ReasonCode::parse(code.as_str()), Some(code));
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
            assert!(!code.plain_language().is_empty());
        }
        assert_eq!(ReasonCode::parse("NOPE"), None);
    }
}
