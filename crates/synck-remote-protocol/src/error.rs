use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    UnsupportedProtocolVersion,
    InvalidRequest,
    InvalidIdentifier,
    InvalidName,
    InvalidContent,
    EmptyIntent,
    MessageTooLarge,
    OperationIdMismatch,
    InvalidCursor,
    CursorBindingMismatch,
    InvalidBlobProof,
    InvalidRange,
    Unauthenticated,
    RepositoryForbidden,
    BlobNotFound,
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
    ReservedWorktreePath,
    BlobProofRequired,
    OperationOwnershipMismatch,
    CursorExpired,
    JsonTooLarge,
    TooManyIntents,
    TooManyEvents,
    SubtreeTooLarge,
    ProjectedGroupTooLarge,
    BlobTooLarge,
    RepositoryIneligible,
    RepositoryCorrupt,
    InternalError,
    ServerUnavailable,
    OperationPending,
}

impl ErrorCode {
    pub const fn http_status(self) -> u16 {
        match self {
            Self::UnsupportedProtocolVersion
            | Self::InvalidRequest
            | Self::InvalidIdentifier
            | Self::InvalidName
            | Self::InvalidContent
            | Self::EmptyIntent
            | Self::MessageTooLarge
            | Self::OperationIdMismatch
            | Self::InvalidCursor
            | Self::CursorBindingMismatch
            | Self::InvalidBlobProof
            | Self::InvalidRange => 400,
            Self::Unauthenticated => 401,
            Self::RepositoryForbidden => 403,
            Self::BlobNotFound => 404,
            Self::ItemNotFound
            | Self::ItemAlreadyExists
            | Self::ItemTombstoned
            | Self::BaseSeqMismatch
            | Self::ParentNotFound
            | Self::ParentNotFolder
            | Self::ParentNotLive
            | Self::NameConflict
            | Self::CycleDetected
            | Self::NoOp
            | Self::ConflictingIntent
            | Self::LifecycleMismatch
            | Self::SubtreeChanged
            | Self::ReservedWorktreePath
            | Self::BlobProofRequired
            | Self::OperationOwnershipMismatch => 409,
            Self::CursorExpired => 410,
            Self::JsonTooLarge
            | Self::TooManyIntents
            | Self::TooManyEvents
            | Self::SubtreeTooLarge
            | Self::ProjectedGroupTooLarge
            | Self::BlobTooLarge => 413,
            Self::RepositoryIneligible => 422,
            Self::RepositoryCorrupt | Self::InternalError => 500,
            Self::ServerUnavailable | Self::OperationPending => 503,
        }
    }

    pub const fn retryable(self) -> bool {
        matches!(
            self,
            Self::BlobProofRequired | Self::ServerUnavailable | Self::OperationPending
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub retryable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    pub protocol_version: crate::ProtocolVersion,
    pub error: ErrorBody,
}

impl ErrorEnvelope {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            protocol_version: crate::ProtocolVersion,
            error: ErrorBody {
                code,
                retryable: code.retryable(),
            },
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.error.retryable != self.error.code.retryable() {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("{code:?}")]
pub struct ValidationError {
    pub code: ErrorCode,
}

impl ValidationError {
    pub const fn new(code: ErrorCode) -> Self {
        Self { code }
    }
}

impl From<synck_file_folder::SemanticError> for ValidationError {
    fn from(error: synck_file_folder::SemanticError) -> Self {
        use synck_file_folder::SemanticErrorCode as Semantic;
        let code = match error.code {
            Semantic::InvalidIdentifier => ErrorCode::InvalidIdentifier,
            Semantic::InvalidName => ErrorCode::InvalidName,
            Semantic::InvalidContent => ErrorCode::InvalidContent,
            Semantic::EmptyIntent => ErrorCode::EmptyIntent,
            Semantic::TooManyIntents => ErrorCode::TooManyIntents,
            Semantic::TooManyEvents => ErrorCode::TooManyEvents,
            Semantic::MessageTooLarge => ErrorCode::MessageTooLarge,
            Semantic::ItemNotFound => ErrorCode::ItemNotFound,
            Semantic::ItemAlreadyExists => ErrorCode::ItemAlreadyExists,
            Semantic::ItemTombstoned => ErrorCode::ItemTombstoned,
            Semantic::BaseSeqMismatch => ErrorCode::BaseSeqMismatch,
            Semantic::ParentNotFound => ErrorCode::ParentNotFound,
            Semantic::ParentNotFolder => ErrorCode::ParentNotFolder,
            Semantic::ParentNotLive => ErrorCode::ParentNotLive,
            Semantic::NameConflict => ErrorCode::NameConflict,
            Semantic::CycleDetected => ErrorCode::CycleDetected,
            Semantic::NoOp => ErrorCode::NoOp,
            Semantic::ConflictingIntent => ErrorCode::ConflictingIntent,
            Semantic::LifecycleMismatch => ErrorCode::LifecycleMismatch,
            Semantic::SubtreeTooLarge => ErrorCode::SubtreeTooLarge,
            Semantic::SubtreeChanged => ErrorCode::SubtreeChanged,
            Semantic::ReservedWorktreePath => ErrorCode::ReservedWorktreePath,
            Semantic::ProjectedGroupTooLarge => ErrorCode::ProjectedGroupTooLarge,
            Semantic::MalformedProfileEvent | Semantic::UnknownProfileEvent => {
                ErrorCode::InvalidRequest
            }
        };
        Self::new(code)
    }
}

/// Map checked Core append failures without inspecting display text or storage
/// backend detail.
pub fn core_error_code(error: &synck_vcs::errors::CoreError) -> ErrorCode {
    use synck_vcs::errors::{CoreError as Core, StorageError as Storage};

    match error {
        Core::ItemNotFound { .. } => ErrorCode::ItemNotFound,
        Core::ItemTombstoned { .. } => ErrorCode::ItemTombstoned,
        Core::ItemAlreadyExists { .. } => ErrorCode::ItemAlreadyExists,
        Core::ParentNotFound { .. } => ErrorCode::ParentNotFound,
        Core::ParentNotFolder { .. } => ErrorCode::ParentNotFolder,
        Core::ParentNotLive { .. } => ErrorCode::ParentNotLive,
        Core::NameConflict { .. } => ErrorCode::NameConflict,
        Core::NameInvalid { .. } | Core::NameTooLong { .. } => ErrorCode::InvalidName,
        Core::CycleDetected { .. } => ErrorCode::CycleDetected,
        Core::BaseSeqMismatch { .. } | Core::RefBaseSeqMismatch { .. } => {
            ErrorCode::BaseSeqMismatch
        }
        Core::SubtreeTooLarge { .. } => ErrorCode::SubtreeTooLarge,
        Core::SubtreeChanged { .. } => ErrorCode::SubtreeChanged,
        Core::ItemIsFederated { .. } => ErrorCode::RepositoryIneligible,
        Core::RepoSeqConflict { .. } => ErrorCode::ServerUnavailable,
        Core::StorageError(
            Storage::Io { .. } | Storage::Overloaded { .. } | Storage::Other { .. },
        ) => ErrorCode::ServerUnavailable,
        Core::EmptyCommit
        | Core::ItemNotFederated { .. }
        | Core::FederatedIdMismatch { .. }
        | Core::ItemSeqMismatch { .. }
        | Core::DomainSeqMismatch { .. }
        | Core::DomainSeqOverlap { .. }
        | Core::CommitIdMismatch { .. }
        | Core::MultipleItemsInFederatedProposal
        | Core::EventCountMismatch { .. }
        | Core::CommitEventIdxMismatch
        | Core::IllegalFederationTransition
        | Core::DuplicateCommit { .. }
        | Core::OperationAlreadyApplied { .. }
        | Core::StorageError(Storage::UniqueViolation { .. })
        | Core::InvalidId(_)
        | Core::InvalidTimestamp(_)
        | Core::InvalidProposal(_) => ErrorCode::InternalError,
    }
}

impl From<synck_vcs::errors::CoreError> for ValidationError {
    fn from(error: synck_vcs::errors::CoreError) -> Self {
        Self::new(core_error_code(&error))
    }
}
