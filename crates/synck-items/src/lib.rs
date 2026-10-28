//! Shared persisted-item vocabulary, event-derived state and body encoding.
//!
//! This crate owns storage-free semantics shared by Synck and SynckHub. It does
//! not contain permission evaluation, platform services or storage adapters.
//! The incremental reader reconstructs supported item/subitem state using these
//! shared decoders and native reducers. Unknown domain semantics fail explicitly;
//! finite supplemental application annotations remain inspectable with a warning.
//! Reference discovery preserves direct canonical roots independently of typed
//! reader support. Callers own verified storage, bounded history paging, body
//! hydration and complete-history transfer.

pub mod body_events;
pub mod chat;
pub mod email;
pub mod email_data_images;
pub mod file;
pub mod imported_email;
pub mod metadata;
pub mod reader;
pub mod references;
pub mod replica_projection;
pub mod subitems;

/// Concurrency discipline declared by a persisted mutation vocabulary.
/// This does not evaluate a caller's permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationPolicy {
    Optimistic,
    Lock,
    Append,
}
