//! Core behavior configuration. Repository validity is fixed by the format.
use crate::types::SequencingMode;

#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub default_sequencing_mode: SequencingMode,
    /// Fixed at creation. A commit message in an encrypted repository is null.
    pub encrypted: bool,
    /// Whether domain cursors mean this replica follows those domains. A portable
    /// repository ingests no domain and keeps its cursors only as provenance.
    pub follows_domains: bool,
}

impl CoreConfig {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            default_sequencing_mode: SequencingMode::Local,
            encrypted: false,
            follows_domains: true,
        }
    }
}
