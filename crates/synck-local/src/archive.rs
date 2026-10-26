use crate::{Repository, Result};
use std::path::Path;

impl Repository {
    /// Create a verified complete copy, preserving the storage mode.
    pub async fn archive(&self, destination: &Path) -> Result<()> {
        self.archive_complete(destination, None).await
    }
}
