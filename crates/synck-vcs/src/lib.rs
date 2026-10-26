//! Synck VCS core library.
//!
//! This crate implements the low-level engine for Synck:
//! - content-addressed commits with canonical JSON encoding,
//! - items/events/commits tables backed by pluggable storage backends,
//! - structural invariants (parent existence/type, acyclicity, name uniqueness,
//!   tombstone finality, sequencing mode rules),
//! - sequencing for local and federated items.
//!
//! The primary entry point is [`repo::Repo`], which orchestrates invariants and
//! storage behind a small async API. Storage interfaces are defined in [`storage`];
//! concrete backends live in separate crates. Externally visible data structures
//! are defined in [`types`]. Errors
//! are surfaced via [`errors::CoreError`]. Name validation and canonicalization
//! rules live in [`naming`]; the database enforces uniqueness only over the
//! derived `name_key`.

pub mod bundle;
pub mod config;
pub mod envelope;
pub mod errors;
pub mod invariants;
pub mod naming;
pub mod repo;
pub mod sequencer;
pub mod state;
pub mod storage;
pub mod transition;
pub mod types;
pub mod verify;

pub use repo::validate_payload_limits;
