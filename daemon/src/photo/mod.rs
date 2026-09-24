//! Photo pipeline (autonomous pipeline, Phase D).
//!
//! Photos are organised the way units are — grouped, never deleted:
//! - [`phash`]: 64-bit perceptual hashes and near-duplicate grouping, with one
//!   representative (sharpest, then earliest) per group.
//! - [`albums`]: EXIF capture-time/GPS sessions.
//! - [`worker`]: hashes new photos, re-derives duplicate groups and albums as
//!   photos arrive, and — only with a local vision model configured — captions
//!   photos and joins each to the topic of its nearest visual neighbour.
//!
//! Everything but captions is pure Rust and runs fully offline.

pub mod albums;
pub mod phash;
pub mod worker;
