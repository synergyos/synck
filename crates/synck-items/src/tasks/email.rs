//! Immutable selected-message context. These are task-owned references, never
//! mailbox permission or arbitrary metadata paths to scan for blob hashes.
use super::TaskError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use synck_vcs::types::{BlobId, EventProposal, ItemId};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Identity {
    Personal {
        owner_id: String,
        account_id: Uuid,
        message_id: String,
    },
    Team {
        task_list_id: ItemId,
        account_id: Uuid,
        message_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    ProviderOnly {
        identity: Identity,
        generation: i64,
    },
    Saved {
        identity: Identity,
        generation: i64,
        snapshot: Snapshot,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub blob_id: BlobId,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub content: Blob,
    pub is_inline: bool,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub captured_at: DateTime<Utc>,
    pub raw_rfc822: Blob,
    pub subject: Option<String>,
    pub internet_message_id: Option<String>,
    pub from: Option<Address>,
    pub sender: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub sent_at: Option<String>,
    pub received_at: Option<String>,
    pub body_markdown: Blob,
    pub body_html: Option<Blob>,
    pub attachments: Vec<Attachment>,
}

impl Source {
    pub fn identity(&self) -> &Identity {
        match self {
            Self::ProviderOnly { identity, .. } | Self::Saved { identity, .. } => identity,
        }
    }
    /// Monotonic source incarnation, independent of task UUIDs and wall clocks.
    pub fn generation(&self) -> i64 {
        match self {
            Self::ProviderOnly { generation, .. } | Self::Saved { generation, .. } => *generation,
        }
    }
    pub fn snapshot(&self) -> Option<&Snapshot> {
        match self {
            Self::ProviderOnly { .. } => None,
            Self::Saved { snapshot, .. } => Some(snapshot),
        }
    }
    pub fn validate(&self) -> Result<(), TaskError> {
        if self.generation() <= 0 {
            return Err(invalid("email source generation must be positive"));
        }
        let (account, message) = match self.identity() {
            Identity::Personal {
                owner_id,
                account_id,
                message_id,
            } => {
                if !valid_identity(owner_id) {
                    return Err(invalid("invalid email source owner"));
                }
                (account_id, message_id)
            }
            Identity::Team {
                account_id,
                message_id,
                ..
            } => (account_id, message_id),
        };
        if account.is_nil() || !valid_identity(message) {
            return Err(invalid("invalid email source account or message"));
        }
        if let Some(snapshot) = self.snapshot() {
            if snapshot.raw_rfc822.size_bytes == 0 {
                return Err(invalid("saved email source is empty"));
            }
            let mut ids = HashSet::new();
            for part in &snapshot.attachments {
                if !valid_identity(&part.attachment_id)
                    || !ids.insert(&part.attachment_id)
                    || part.filename.is_empty()
                    || part.mime_type.is_empty()
                {
                    return Err(invalid("invalid or duplicate saved email attachment"));
                }
            }
        }
        Ok(())
    }
}

impl Snapshot {
    /// Exact closure shared by retention, verification, transfer and downloads.
    pub fn blobs(&self) -> impl Iterator<Item = &Blob> {
        std::iter::once(&self.raw_rfc822)
            .chain(std::iter::once(&self.body_markdown))
            .chain(self.body_html.iter())
            .chain(self.attachments.iter().map(|part| &part.content))
    }
}

pub fn from_metadata(meta: &Value) -> Result<Option<Source>, TaskError> {
    let Some(value) = meta.get("email_source") else {
        return Ok(None);
    };
    let source: Source =
        serde_json::from_value(value.clone()).map_err(|_| invalid("invalid task email source"))?;
    source.validate()?;
    Ok(Some(source))
}

/// Exact saved-email closure for retention and federation. Do not scan arbitrary
/// task metadata: only these events can introduce an immutable email snapshot.
pub fn event_blob_references(event: &EventProposal) -> Result<Vec<String>, TaskError> {
    if !matches!(
        event.event_type.as_str(),
        "TASK::CREATE" | "TASK::IMPORT" | "TASK::CAPTURE_EMAIL"
    ) {
        return Ok(Vec::new());
    }
    super::history::validate_envelope(event)?;
    let source = from_metadata(&event.set_metadata.as_ref().expect("validated metadata")["meta"])?;
    let Some(snapshot) = source.as_ref().and_then(Source::snapshot) else {
        return Ok(Vec::new());
    };
    let mut refs: Vec<_> = snapshot
        .blobs()
        .map(|blob| blob.blob_id.to_string())
        .collect();
    refs.sort_unstable();
    refs.dedup();
    Ok(refs)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.trim() == value && !value.contains('\0')
}
fn invalid(message: &str) -> TaskError {
    TaskError::Invalid(message.into())
}

#[cfg(test)]
mod tests;
