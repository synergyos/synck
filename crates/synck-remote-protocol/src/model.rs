use std::fmt;

use chrono::{DateTime, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use synck_file_folder::{BlobRef, ChangeGroup, FileFolderKind, Intent};
use synck_vcs::types::{CommitId, ItemId, RepoId, Sha256Digest};

use crate::{MAX_OPAQUE_BYTES, PROTOCOL_VERSION};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProtocolVersion;

impl Serialize for ProtocolVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = u64::deserialize(deserializer)?;
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(D::Error::custom("unsupported protocol version"));
        }
        Ok(Self)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Profile;

impl Serialize for Profile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(synck_file_folder::PROFILE_ID)
    }
}

impl<'de> Deserialize<'de> for Profile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let profile = String::deserialize(deserializer)?;
        if profile != synck_file_folder::PROFILE_ID {
            return Err(D::Error::custom("unsupported profile"));
        }
        Ok(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpaqueValue(String);

impl OpaqueValue {
    pub fn parse(value: impl Into<String>) -> Result<Self, crate::ValidationError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_OPAQUE_BYTES {
            return Err(crate::ValidationError::new(
                crate::ErrorCode::InvalidRequest,
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OpaqueValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for OpaqueValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OpaqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtcTimestamp(String);

impl UtcTimestamp {
    pub fn parse(value: impl Into<String>) -> Result<Self, crate::ValidationError> {
        let value = value.into();
        if !value.ends_with('Z')
            || DateTime::parse_from_rfc3339(&value)
                .map(|parsed| parsed.with_timezone(&Utc))
                .is_err()
        {
            return Err(crate::ValidationError::new(
                crate::ErrorCode::InvalidRequest,
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for UtcTimestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryLimits {
    pub max_json_bytes: u64,
    pub max_intents_per_commit: u64,
    pub max_events_per_commit: u64,
    pub max_lifecycle_events_per_commit: u64,
    pub max_lifecycle_descendants: u64,
    pub max_event_payload_bytes: u64,
    pub max_commit_event_payload_bytes: u64,
    pub max_manifest_entries_per_page: u64,
    pub max_commits_per_change_page: u64,
    pub max_projected_group_bytes: u64,
    pub max_blob_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferCapabilities {
    pub upload_resume: bool,
    pub download_single_range: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryResponse {
    pub protocol_version: ProtocolVersion,
    pub server_id: String,
    pub profile: Profile,
    pub hash: String,
    pub limits: DiscoveryLimits,
    pub transfer: TransferCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySummary {
    pub repo_id: RepoId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPage {
    pub protocol_version: ProtocolVersion,
    pub repositories: Vec<RepositorySummary>,
    pub next_cursor: Option<OpaqueValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryListQuery {
    pub limit: Option<u16>,
    pub cursor: Option<OpaqueValue>,
}

impl RepositoryListQuery {
    pub fn effective_limit(&self) -> usize {
        self.limit
            .map_or(crate::MAX_REPOSITORIES_PER_PAGE, usize::from)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub repo_seq: u64,
    pub tip_commit_id: Option<CommitId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Clone,
    Pull,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRequest {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    pub kind: CaptureKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_checkpoint: Option<Checkpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureResponse {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    pub kind: CaptureKind,
    pub target_checkpoint: Checkpoint,
    pub page_cursor: OpaqueValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    pub cursor: OpaqueValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ManifestLifecycle {
    Live,
    Trash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub item_id: ItemId,
    pub item_type: FileFolderKind,
    pub parent_item_id: Option<ItemId>,
    pub name: String,
    pub lifecycle: ManifestLifecycle,
    pub head_seq: i64,
    pub trash_commit_id: Option<CommitId>,
    pub content: Option<BlobRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPageResponse {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    pub target_checkpoint: Checkpoint,
    pub entries: Vec<ManifestEntry>,
    pub next_cursor: Option<OpaqueValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangePageResponse {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    pub target_checkpoint: Checkpoint,
    pub page_through_checkpoint: Checkpoint,
    pub groups: Vec<ChangeGroup>,
    pub next_cursor: Option<OpaqueValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaveNeedRequest {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    pub operation_id: Sha256Digest,
    pub blobs: Vec<BlobRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaveNeedState {
    Have,
    Need,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadDescriptor {
    pub upload_id: OpaqueValue,
    pub method: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaveNeedResult {
    pub blob_id: synck_vcs::types::BlobId,
    pub size: u64,
    pub state: HaveNeedState,
    pub proof: OpaqueValue,
    pub expires_at: UtcTimestamp,
    pub upload: Option<UploadDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaveNeedResponse {
    pub protocol_version: ProtocolVersion,
    pub results: Vec<HaveNeedResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub intents: Vec<Intent>,
    pub blob_proofs: Vec<OpaqueValue>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationReceipt {
    pub protocol_version: ProtocolVersion,
    pub profile: Profile,
    pub repo_id: RepoId,
    pub operation_id: Sha256Digest,
    pub group: ChangeGroup,
}
