use serde::{Deserialize, Deserializer, Serialize};
use synck_vcs::types::{BlobId, CommitId, EventProposal, ItemId, RepoId, Timestamp};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum FileFolderKind {
    File,
    Folder,
}

impl FileFolderKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::File => "FILE",
            Self::Folder => "FOLDER",
        }
    }

    pub(crate) fn from_item_type(item_type: &str) -> Option<Self> {
        match item_type {
            "FILE" => Some(Self::File),
            "FOLDER" => Some(Self::Folder),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ProfileLifecycleState {
    Live,
    Trash,
    Tombstoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    #[serde(deserialize_with = "deserialize_nullable_item_id")]
    pub parent_item_id: Option<ItemId>,
    pub name: String,
}

fn deserialize_nullable_item_id<'de, D>(deserializer: D) -> Result<Option<ItemId>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<ItemId>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub blob_id: BlobId,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemBase {
    pub item_id: ItemId,
    pub base_seq: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Intent {
    CreateFolder {
        item_id: ItemId,
        location: Location,
    },
    CreateFile {
        item_id: ItemId,
        location: Location,
        content: BlobRef,
    },
    UpdateFolder {
        item_id: ItemId,
        base_seq: i64,
        location: Location,
    },
    UpdateFile {
        item_id: ItemId,
        base_seq: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<Location>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<BlobRef>,
    },
    Trash {
        root: ItemBase,
        descendants: Vec<ItemBase>,
    },
    Restore {
        root: ItemBase,
        trash_commit_id: CommitId,
        descendants: Vec<ItemBase>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<Location>,
    },
}

impl Intent {
    pub(crate) fn root_id(&self) -> &ItemId {
        match self {
            Self::CreateFolder { item_id, .. }
            | Self::CreateFile { item_id, .. }
            | Self::UpdateFolder { item_id, .. }
            | Self::UpdateFile { item_id, .. } => item_id,
            Self::Trash { root, .. } | Self::Restore { root, .. } => &root.item_id,
        }
    }

    pub(crate) fn discriminant(&self) -> &'static str {
        match self {
            Self::CreateFolder { .. } => "create_folder",
            Self::CreateFile { .. } => "create_file",
            Self::UpdateFolder { .. } => "update_folder",
            Self::UpdateFile { .. } => "update_file",
            Self::Trash { .. } => "trash",
            Self::Restore { .. } => "restore",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentProposal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub intents: Vec<Intent>,
}

/// Explicit current state supplied to the storage-free validator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentItem {
    pub item_id: ItemId,
    pub item_type: String,
    #[serde(deserialize_with = "deserialize_nullable_item_id")]
    pub parent_item_id: Option<ItemId>,
    pub name: String,
    pub name_key: String,
    pub head_seq: i64,
    pub lifecycle_state: ProfileLifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trash_commit_id: Option<CommitId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<BlobRef>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImmutableItemKind {
    pub item_id: ItemId,
    pub item_type: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemChange {
    pub item_id: ItemId,
    pub item_type: FileFolderKind,
    pub item_seq: i64,
    pub event: EventProposal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemHead {
    pub item_id: ItemId,
    pub item_type: FileFolderKind,
    pub head_seq: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeGroup {
    pub repo_id: RepoId,
    pub repo_seq: i64,
    pub commit_id: CommitId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub accepted_at: Timestamp,
    pub changes: Vec<ItemChange>,
    pub item_heads: Vec<ItemHead>,
}
