//! Standalone, permanently local Synck VCS.

mod archive;
mod checkout;
mod chunk_store;
mod commands;
mod comparison;
mod email_import;
mod error;
mod event_profile;
mod inspection;
mod integrity;
mod local_database;
mod monitor;
mod objects;
mod ooxml;
pub mod portable;
mod repository;
mod tree_materialization;
mod versions;
mod working_tree_edit;
mod worktree;

pub use commands::{
    CommitEvent, CommitEventPage, CommitOutcome, DeletedEntry,
    DetailedEventLogEntry, DetailedLogEntry, EventHistoryCommit, HistoryCommit, LogEntry,
    VersionEntry,
};
pub use comparison::{
    ComparedRevision, StructuralComparison, TEXT_DIFF_MAX_BYTES, TextDiffEntry, TextDiffKind,
};
pub use error::{LocalError, Result, WorkingTreeEditError};
pub use inspection::{
    HistoricalCursor, HistoricalFile, HistoricalItem, HistoricalItemKind, HistoricalPage,
    HistoricalView, LogPage, RepositoryInfo, TrackedItem, TrackedItemKind,
    cleanup_historical_cache,
};
pub use integrity::FsckReport;
pub use monitor::{MonitorStatus, run_monitor_server};
pub use objects::{GcReport, StorageReport};
pub use repository::{HiddenFilesMode, Repository, create_default_synckignore};
pub use versions::NamedVersionEntry;
pub use working_tree_edit::{WorkingTreeEntry, WorkingTreeEntryKind, WorkingTreeSnapshot};
pub use worktree::{Change, ChangeKind};

pub use chunk_store::StorageMode;

/// Create a complete format-6 repository with chunked storage and a checkout.
pub use portable::init_repository as init;
