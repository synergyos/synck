//! Standalone, permanently local Synck VCS.

mod agent_locks;
mod archive;
pub use agent_locks::{AgentLease, AgentLock};
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
mod name_repairs;
mod name_repair_journal;
mod objects;
mod raw_object;
mod path_warnings;
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
pub use name_repairs::NameRepair;
pub use repository::{HiddenFilesMode, Repository, create_default_synckignore};
pub use versions::NamedVersionEntry;
pub use working_tree_edit::{WorkingTreeEntry, WorkingTreeEntryKind, WorkingTreeSnapshot};
pub use worktree::{Change, ChangeKind};

pub use chunk_store::StorageMode;

/// Create a complete format-7 repository with chunked storage and a checkout.
pub use portable::init_repository as init;

/// Entry points for `synck-fuzz`. Not a supported API.
#[doc(hidden)]
pub mod fuzz_entry {
    pub fn decode_chunk(codec: &str, raw_size: i64, payload: &[u8]) {
        let _ = crate::chunk_store::decode_chunk(codec, raw_size, payload);
    }

    pub fn ooxml_regions(file: std::fs::File, size: u64) {
        let _ = crate::ooxml::chunk_region_lengths(file, size, crate::ooxml::OoxmlKind::Docx);
    }
}
