//! Storage-free Synck remote protocol v1 types and validation.
//!
//! This crate owns the strict JSON contract and pure binding checks. It owns no
//! HTTP transport, authentication, persistence, paging state, or checkout
//! behavior.

mod error;
mod json;
mod model;
mod validate;

pub use error::{ErrorBody, ErrorCode, ErrorEnvelope, ValidationError, core_error_code};
pub use json::{decode_json, encode_json};
pub use model::*;
pub use synck_file_folder::{BlobRef, ChangeGroup, FileFolderKind, Intent, ItemChange, ItemHead};
pub use validate::*;

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_JSON_BYTES: usize = 20_971_520;
pub const MAX_OPAQUE_BYTES: usize = 4_096;
pub const MAX_REPOSITORIES_PER_PAGE: usize = 256;
pub const MAX_REPOSITORY_NAME_BYTES: usize = 255;
pub const MAX_MANIFEST_ENTRIES_PER_PAGE: usize = 512;
pub const MAX_COMMITS_PER_CHANGE_PAGE: usize = 128;
pub const MAX_BLOB_DECLARATIONS: usize = 1_024;
pub const MAX_SEQUENCING_ATTEMPTS: usize = 1_024;
