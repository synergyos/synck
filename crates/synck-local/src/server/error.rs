use std::path::PathBuf;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("INVALID: {0}")]
    Invalid(String),
    #[error("STATE_INVALID: {0}")]
    State(String),
    #[error("SERVER_LOCKED: another server owns {0}")]
    Locked(PathBuf),
    #[error("UNAUTHENTICATED")]
    Unauthenticated,
    /// Also every repository the token was not granted, so IDs do not leak.
    #[error("NOT_FOUND")]
    NotFound,
    #[error("FORBIDDEN")]
    Forbidden,
    #[error("SERVER_OVERLOADED")]
    Overloaded,
    #[error("{0}")]
    Protocol(#[from] synck_sync::ProtocolError),
    /// The operation ID was submitted with another request digest.
    #[error("OPERATION_REUSED")]
    OperationReused,
    /// A submission names content the repository does not hold.
    #[error("OBJECTS_MISSING")]
    ObjectsMissing(Vec<synck_vcs::types::ObjectId>),
    #[error("OBJECT_MISSING")]
    ObjectMissing,
    #[error("DATABASE_ERROR: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("REPOSITORY_ERROR: {0}")]
    Repository(#[from] crate::LocalError),
    #[error("FILESYSTEM_ERROR: {action} {path}: {source}")]
    Filesystem {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub fn filesystem(
        action: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Filesystem {
            action,
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        use synck_sync::ProtocolError;
        let (status, code) = match &self {
            Self::Unauthenticated => (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
            Self::NotFound => (StatusCode::NOT_FOUND, "NOT_FOUND"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            Self::Invalid(_)
            | Self::Repository(crate::LocalError::Invalid(_))
            // Core refused the proposal itself; storage failures stay internal.
            | Self::Repository(crate::LocalError::Core(
                synck_vcs::errors::CoreError::InvalidProposal(_)
                | synck_vcs::errors::CoreError::InvalidId(_)
                | synck_vcs::errors::CoreError::InvalidTimestamp(_),
            ))
            | Self::Protocol(
                ProtocolError::Invalid(_)
                | ProtocolError::Limit(_)
                | ProtocolError::Core(_),
            ) => (StatusCode::BAD_REQUEST, "INVALID"),
            Self::Protocol(ProtocolError::Checkpoint) => (StatusCode::CONFLICT, "CHECKPOINT"),
            Self::Repository(crate::LocalError::Locked { .. }) => (StatusCode::CONFLICT, "LOCKED"),
            Self::Repository(crate::LocalError::LockLost) => (StatusCode::CONFLICT, "LOCK_LOST"),
            Self::Protocol(ProtocolError::UploadExpired) => (StatusCode::GONE, "UPLOAD_EXPIRED"),
            Self::Protocol(ProtocolError::Generation) => (StatusCode::CONFLICT, "GENERATION"),
            Self::OperationReused => (StatusCode::CONFLICT, "OPERATION_REUSED"),
            Self::ObjectsMissing(_) => (StatusCode::CONFLICT, "OBJECTS_MISSING"),
            Self::ObjectMissing => (StatusCode::NOT_FOUND, "OBJECT_MISSING"),
            Self::Overloaded => (StatusCode::SERVICE_UNAVAILABLE, "SERVER_OVERLOADED"),
            _ => {
                eprintln!("synck serve: {self}");
                (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL")
            }
        };
        let body = match &self {
            Self::ObjectsMissing(objects) => {
                serde_json::json!({ "status": code, "objects": objects })
            }
            Self::Repository(crate::LocalError::Locked { path, owner }) => {
                serde_json::json!({ "status": code, "path": path, "owner": owner })
            }
            _ => serde_json::json!({ "status": code }),
        };
        let mut response = (status, axum::Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
