//! Core configuration for the Synck VCS engine.
//!
//! Keep this deliberately small and focused on knobs that meaningfully affect
//! engine behavior. Policy stays out of here.

use crate::types::SequencingMode;

/// Configuration for a `Repo` instance.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// Default sequencing mode for newly created items.
    pub default_sequencing_mode: SequencingMode,
    /// Optional upper bound on events per commit. `None` means no engine-level
    /// limit beyond what storage can handle.
    pub max_events_per_commit: Option<usize>,
    /// Optional upper bound on canonical bytes for a single event payload.
    pub max_event_payload_bytes: Option<usize>,
    /// Optional upper bound on total canonical event payload bytes per commit.
    pub max_commit_payload_bytes: Option<usize>,
    /// Optional upper bound on the UTF-8 bytes in a commit author identifier.
    pub max_author_id_bytes: Option<usize>,
    /// Optional upper bound on the UTF-8 bytes in a commit message.
    pub max_commit_message_bytes: Option<usize>,
    /// Optional upper bound on the UTF-8 bytes in an append operation identifier.
    pub max_operation_id_bytes: Option<usize>,
}

impl CoreConfig {
    /// Construct a configuration with sensible defaults.
    pub fn new() -> Self {
        Self::default()
    }
}

pub const SYNCKHUB_MAX_EVENTS_PER_COMMIT: usize = 10_000;
pub const SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub const SYNCKHUB_MAX_AUTHOR_ID_BYTES: usize = 256;
pub const SYNCKHUB_MAX_COMMIT_MESSAGE_BYTES: usize = 4 * 1024;
pub const SYNCKHUB_MAX_OPERATION_ID_BYTES: usize = 256;

impl Default for CoreConfig {
    fn default() -> Self {
        CoreConfig {
            default_sequencing_mode: SequencingMode::Local,
            max_events_per_commit: Some(SYNCKHUB_MAX_EVENTS_PER_COMMIT),
            max_event_payload_bytes: Some(SYNCKHUB_MAX_EVENT_PAYLOAD_BYTES),
            max_commit_payload_bytes: Some(SYNCKHUB_MAX_COMMIT_PAYLOAD_BYTES),
            max_author_id_bytes: Some(SYNCKHUB_MAX_AUTHOR_ID_BYTES),
            max_commit_message_bytes: Some(SYNCKHUB_MAX_COMMIT_MESSAGE_BYTES),
            max_operation_id_bytes: Some(SYNCKHUB_MAX_OPERATION_ID_BYTES),
        }
    }
}
