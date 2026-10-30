use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticErrorCode {
    InvalidIdentifier,
    InvalidName,
    InvalidContent,
    EmptyIntent,
    TooManyIntents,
    TooManyEvents,
    MessageTooLarge,
    ItemNotFound,
    ItemAlreadyExists,
    ItemTombstoned,
    BaseSeqMismatch,
    ParentNotFound,
    ParentNotFolder,
    ParentNotLive,
    NameConflict,
    CycleDetected,
    NoOp,
    ConflictingIntent,
    LifecycleMismatch,
    SubtreeChanged,
    ProjectedGroupTooLarge,
    MalformedProfileEvent,
    UnknownProfileEvent,
}

impl SemanticErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidIdentifier => "INVALID_IDENTIFIER",
            Self::InvalidName => "INVALID_NAME",
            Self::InvalidContent => "INVALID_CONTENT",
            Self::EmptyIntent => "EMPTY_INTENT",
            Self::TooManyIntents => "TOO_MANY_INTENTS",
            Self::TooManyEvents => "TOO_MANY_EVENTS",
            Self::MessageTooLarge => "MESSAGE_TOO_LARGE",
            Self::ItemNotFound => "ITEM_NOT_FOUND",
            Self::ItemAlreadyExists => "ITEM_ALREADY_EXISTS",
            Self::ItemTombstoned => "ITEM_TOMBSTONED",
            Self::BaseSeqMismatch => "BASE_SEQ_MISMATCH",
            Self::ParentNotFound => "PARENT_NOT_FOUND",
            Self::ParentNotFolder => "PARENT_NOT_FOLDER",
            Self::ParentNotLive => "PARENT_NOT_LIVE",
            Self::NameConflict => "NAME_CONFLICT",
            Self::CycleDetected => "CYCLE_DETECTED",
            Self::NoOp => "NO_OP",
            Self::ConflictingIntent => "CONFLICTING_INTENT",
            Self::LifecycleMismatch => "LIFECYCLE_MISMATCH",
            Self::SubtreeChanged => "SUBTREE_CHANGED",
            Self::ProjectedGroupTooLarge => "PROJECTED_GROUP_TOO_LARGE",
            Self::MalformedProfileEvent => "MALFORMED_PROFILE_EVENT",
            Self::UnknownProfileEvent => "UNKNOWN_PROFILE_EVENT",
        }
    }
}

impl std::fmt::Display for SemanticErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{code}: {detail}")]
pub struct SemanticError {
    pub code: SemanticErrorCode,
    pub detail: String,
}

impl SemanticError {
    pub(crate) fn new(code: SemanticErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}
