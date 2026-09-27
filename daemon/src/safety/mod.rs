//! Semantic safety: inference certificates and the rules that decide when a
//! local similarity may become a global conclusion.
//!
//! The core policy: **similarity is not identity, and local evidence does not
//! justify global closure.** Every consequential automatic conclusion is
//! produced by a named, versioned rule that evaluates explicit predicates and
//! returns an [`certificate::InferenceDecision`] carrying its certificate.
//! Missing or conflicting information never authorizes an automatic action:
//! it routes to review (or blocks), fail-closed.
//!
//! Rules are pure (no I/O) so they are deterministic and testable in
//! isolation; `store` persists certificates and `service` implements the
//! database-backed operations (retraction, user decisions, support).

pub mod certificate;
pub mod contradiction;
pub mod drift;
pub mod eval;
pub mod explained;
pub mod identity;
pub mod modality;
pub mod photo;
pub mod provenance;
pub mod reason;
pub mod retraction;
pub mod service;
pub mod store;
pub mod temporal;

pub use certificate::{
    ConclusionKind, Decision, EvidenceClass, EvidenceRef, InferenceCertificate, InferenceDecision,
    Outcome, Predicate, RuleId,
};
pub use reason::ReasonCode;
