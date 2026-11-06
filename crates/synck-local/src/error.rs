use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, LocalError>;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum WorkingTreeEditError {
    #[error("the working tree changed; refresh before continuing")]
    Stale,
    #[error("item name must be one valid file or folder name")]
    InvalidName,
    #[error("item name is reserved")]
    ReservedName,
    #[error("an item with that name already exists")]
    DestinationExists,
    #[error("the destination is not a folder")]
    DestinationNotFolder,
    #[error("the item already has that name")]
    SameName,
    #[error("the item is already in that folder")]
    SameFolder,
    #[error("cannot move a folder below itself")]
    FolderBelowItself,
    #[error("no files were selected")]
    NoFilesSelected,
    #[error("an import source is not a regular file")]
    InvalidImportSource,
    #[error("selected files contain duplicate names")]
    DuplicateImportNames,
}

#[derive(Debug, Error)]
pub enum LocalError {
    #[error("path {path} is reserved by {owner}")]
    Locked { path: String, owner: String },
    #[error("agent lock token is missing, expired, released, or does not cover this path")]
    LockLost,
    #[error("repository busy")]
    Busy,
    /// Nothing was sent to or taken in from the sync server.
    #[error("cannot reach the sync server: {0}")]
    Unreachable(String),
    /// The sync server answered with a protocol refusal.
    #[error("the sync server refused the request: {status}")]
    Server { status: String },
    #[error("{0}")]
    Invalid(String),
    /// The checkout has names the portable rules refuse; repairing them
    /// renames those entries.
    #[error("{0}")]
    NonPortableNames(String),
    #[error("{0}")]
    WorkingTree(#[from] WorkingTreeEditError),
    #[error("integrity error: {0}")]
    Integrity(String),
    #[error("monitor unavailable: {0}")]
    MonitorUnavailable(String),
    #[error("commit {commit_id} was accepted; local recovery is pending: {detail}")]
    AcceptedNeedsRecovery { commit_id: String, detail: String },
    #[error("FILE {item_id} content is {blob_id}, not the expected base {expected}")]
    Conflict {
        item_id: String,
        head_seq: i64,
        blob_id: String,
        size: u64,
        expected: String,
    },
    /// History and this working tree changed the same items since its baseline.
    #[error("changed here and in the repository since this working tree was last updated: {}; run synck pull to merge them", .paths.join(", "))]
    ChangedOnBothSides { paths: Vec<String> },
    /// Changes on both sides that pull cannot merge: anything but content.
    #[error("changed here and in the repository in ways pull cannot merge (moved, renamed, deleted, or a file against a folder): {}; make the repository's change here too, or synck restore them, then pull", .paths.join(", "))]
    CannotMerge { paths: Vec<String> },
    /// A held commit changes items the repository changed too.
    #[error("held commits change items the repository changed too: {}; synck pull puts their changes back in the working files and merges them", .paths.join(", "))]
    HeldConflict { paths: Vec<String> },
    /// A pull withdrew conflicting held commits into the working files.
    #[error("held commits change items the repository changed too: {}; withdrawn as {} (synck withdrawn lists them) and their changes are back in the working files, merged with the repository's. Check them, synck resolve any conflicts, then commit and push", .paths.join(", "), .withdrawn.join(", "))]
    HeldMerged { paths: Vec<String>, withdrawn: Vec<String> },
    /// Files a pull merged with conflicts or kept both versions of.
    #[error("unresolved conflicts: {}; edit them, then run synck resolve with their paths", .paths.join(", "))]
    Unresolved { paths: Vec<String> },
    #[error("move from {from_path} to {to_path} is pending recovery: {detail}")]
    MoveNeedsRecovery {
        from_path: String,
        to_path: String,
        detail: String,
    },
    #[error(
        "{operation} was published at {path}, but parent directory durability is uncertain: {detail}",
        path = .path.display()
    )]
    PublishedNeedsDurability {
        operation: &'static str,
        path: PathBuf,
        detail: String,
    },
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("VCS error: {0}")]
    Core(#[from] synck_vcs::errors::CoreError),
    #[error("FILE/FOLDER semantic error: {0}")]
    FileFolder(#[from] synck_file_folder::SemanticError),
    #[error("invalid repository config: {0}")]
    ConfigJson(#[from] serde_json::Error),
}

pub(crate) trait IoContext<T> {
    fn at(self, action: &'static str, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, action: &'static str, path: impl Into<PathBuf>) -> Result<T> {
        let path = path.into();
        self.map_err(|source| LocalError::Io {
            action,
            path,
            source,
        })
    }
}
