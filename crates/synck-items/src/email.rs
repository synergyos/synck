use super::MutationPolicy;
use serde_json::{Value, json};
use synck_vcs::{
    metadata::MetadataRef,
    types::{CreateItem, EventContent, EventProposal, ItemId},
};

pub const ITEM_TYPE: &str = "EMAIL";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("EMAIL::")?;

    if !rest.contains("::") {
        return match rest {
            "CREATE" | "SET_TITLE" | "SET_LINK" | "SET_METADATA" | "FEDERATE" | "TRASH"
            | "RESTORE" | "TOMBSTONE" => Some(MutationPolicy::Optimistic),
            _ => None,
        };
    }

    match event_type {
        "EMAIL::ATTACHMENT::CREATE" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::SET_METADATA" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::TRASH" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::RESTORE" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::TOMBSTONE" => Some(MutationPolicy::Append),
        _ => None,
    }
}

/// Storage-free EMAIL commands. Callers authorize the item/target, retain the
/// operation identity and publish the materialized event with their own codec.
/// The immutable message has no content-replacement command.
pub enum Intent {
    Create {
        link: Option<ItemId>,
        metadata: Value,
        content: Option<EventContent>,
    },
    SetTitle(String),
    SetLink(Option<ItemId>),
    SetMetadata(Value),
    Trash,
    Restore,
    Tombstone,
    CreateAttachment {
        metadata: Value,
        content: EventContent,
    },
    SetAttachmentMetadata {
        item_seq: i64,
        metadata: Value,
    },
    TrashAttachment(i64),
    RestoreAttachment(i64),
    TombstoneAttachment(i64),
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Metadata(#[from] synck_vcs::metadata::MetadataError),
}

pub fn materialize(
    item_id: &ItemId,
    base_seq: Option<i64>,
    intent: Intent,
) -> Result<EventProposal, PlanError> {
    if base_seq.is_some_and(|seq| seq < 1) {
        return Err(PlanError::Invalid("EMAIL base sequence must be positive"));
    }
    let mut event = EventProposal {
        item_id: Some(item_id.clone()),
        event_type: String::new(),
        set_metadata: None,
        set_content: None,
        create_item: None,
        set_parent: None,
        set_name: None,
        set_link: None,
        merged_from: None,
        set_tombstoned: None,
        set_in_trash: None,
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq,
    };
    let mut metadata = None;
    let verb = match intent {
        Intent::Create {
            link,
            metadata: value,
            content,
        } => {
            if base_seq.is_some() {
                return Err(PlanError::Invalid("a new EMAIL has no base sequence"));
            }
            event.create_item = Some(CreateItem::record(ITEM_TYPE, None, link));
            event.set_content = content;
            metadata = Some(value);
            "CREATE"
        }
        Intent::SetTitle(title) => {
            if title.is_empty() {
                return Err(PlanError::Invalid("an EMAIL title is nonempty"));
            }
            metadata = Some(json!({"title":title}));
            "SET_TITLE"
        }
        Intent::SetLink(link) => {
            event.set_link = Some(link);
            "SET_LINK"
        }
        Intent::SetMetadata(value) => {
            metadata = Some(value);
            "SET_METADATA"
        }
        Intent::Trash => {
            event.set_in_trash = Some(true);
            "TRASH"
        }
        Intent::Restore => {
            event.set_in_trash = Some(false);
            "RESTORE"
        }
        Intent::Tombstone => {
            event.set_tombstoned = Some(true);
            "TOMBSTONE"
        }
        Intent::CreateAttachment {
            metadata: value,
            content,
        } => {
            metadata = Some(json!({"meta":value}));
            event.set_content = Some(content);
            "ATTACHMENT::CREATE"
        }
        Intent::SetAttachmentMetadata {
            item_seq,
            metadata: value,
        } => {
            if item_seq < 1 {
                return Err(PlanError::Invalid(
                    "an EMAIL attachment sequence must be positive",
                ));
            }
            metadata = Some(json!({"item_seq":item_seq,"meta":value}));
            "ATTACHMENT::SET_METADATA"
        }
        Intent::TrashAttachment(seq)
        | Intent::RestoreAttachment(seq)
        | Intent::TombstoneAttachment(seq) => {
            if seq < 1 {
                return Err(PlanError::Invalid(
                    "an EMAIL attachment sequence must be positive",
                ));
            }
            metadata = Some(json!({"item_seq":seq}));
            match intent {
                Intent::TrashAttachment(_) => "ATTACHMENT::TRASH",
                Intent::RestoreAttachment(_) => "ATTACHMENT::RESTORE",
                _ => "ATTACHMENT::TOMBSTONE",
            }
        }
    };
    event.event_type = format!("EMAIL::{verb}");
    event.set_metadata = metadata.map(MetadataRef::new).transpose()?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::{ItemReader, ReadContext, SubitemLifecycle};

    #[test]
    fn planned_email_commands_replay_through_the_public_reader() {
        let id = ItemId::new();
        let source = synck_vcs::types::ObjectId::from_digest([7; 32]);
        let mut reader = ItemReader::new(id.clone());
        let intents = [
            Intent::Create {
                link: None,
                metadata: json!({"title":"Original","mailcal":{"subject":"Subject"}}),
                content: Some(EventContent::source(source.clone())),
            },
            Intent::CreateAttachment {
                metadata: json!({"filename":"one.txt","size_bytes":3}),
                content: EventContent::source(source),
            },
            Intent::SetAttachmentMetadata {
                item_seq: 2,
                metadata: json!({"filename":"renamed.txt"}),
            },
            Intent::TrashAttachment(2),
            Intent::RestoreAttachment(2),
            Intent::SetMetadata(json!({"note":"retained"})),
            Intent::SetTitle("Renamed".into()),
            Intent::SetLink(Some(ItemId::new())),
            Intent::Trash,
            Intent::Restore,
            Intent::TombstoneAttachment(2),
        ];
        for (index, intent) in intents.into_iter().enumerate() {
            let event = materialize(&id, (index > 1).then_some(index as i64), intent).unwrap();
            reader
                .apply(
                    &event,
                    ReadContext {
                        item_seq: index as i64 + 1,
                        created_at: chrono::Utc::now(),
                        occurred_at: None,
                        author: "author",
                        committer: "committer",
                    },
                )
                .unwrap();
        }
        let state = reader.snapshot().unwrap();
        assert_eq!(state.projection.name, "Renamed");
        assert_eq!(
            state.projection.metadata_json["mailcal"]["subject"],
            "Subject"
        );
        assert_eq!(state.projection.metadata_json["note"], "retained");
        assert!(state.projection.parent_folder_id.is_some());
        assert!(matches!(
            state.subitems[0].lifecycle,
            SubitemLifecycle::Tombstoned
        ));
        assert_eq!(state.subitems[0].metadata["filename"], "renamed.txt");
        let restored = ItemReader::from_cache(id, &reader.export_cache().unwrap()).unwrap();
        assert_eq!(restored.snapshot().unwrap().projection.latest_item_seq, 11);
    }
}
