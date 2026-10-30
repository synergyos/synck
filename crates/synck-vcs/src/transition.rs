//! Pure item-state transition shared by acceptance, verification, and state projection.

use crate::naming::{NameError, compute_name_key};
use crate::state::ItemValue;
use crate::types::{CreateItem, EventProposal, ItemId, ItemRow, LifecycleState, SequencingMode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemState {
    pub item_type: String,
    pub parent_folder_id: Option<ItemId>,
    pub name: Option<String>,
    pub name_key: Option<String>,
    pub record_key: Option<String>,
    pub link_folder_id: Option<ItemId>,
    pub lifecycle_state: LifecycleState,
    pub sequencing_mode: SequencingMode,
    pub federation_stream_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error("{0}")]
    Invalid(&'static str),
}

impl ItemState {
    pub fn from_create(
        create: &CreateItem,
        sequencing_mode: SequencingMode,
        federation_stream_id: Option<String>,
    ) -> Result<Self, NameError> {
        Ok(Self {
            item_type: create.item_type.clone(),
            parent_folder_id: create.parent_folder_id.clone(),
            name: create.name.clone(),
            name_key: create.name.as_deref().map(compute_name_key).transpose()?,
            record_key: create.key.clone(),
            link_folder_id: create.link.clone(),
            lifecycle_state: LifecycleState::Live,
            sequencing_mode,
            federation_stream_id,
        })
    }

    pub fn is_tree(&self) -> bool {
        self.name.is_some()
    }

    pub fn write_to(&self, row: &mut ItemRow) {
        row.item_type.clone_from(&self.item_type);
        row.parent_folder_id.clone_from(&self.parent_folder_id);
        row.name.clone_from(&self.name);
        row.name_key.clone_from(&self.name_key);
        row.record_key.clone_from(&self.record_key);
        row.link_folder_id.clone_from(&self.link_folder_id);
        row.lifecycle_state = self.lifecycle_state;
        row.sequencing_mode = self.sequencing_mode;
        row.federation_stream_id
            .clone_from(&self.federation_stream_id);
    }
}

impl From<&ItemRow> for ItemState {
    fn from(row: &ItemRow) -> Self {
        Self {
            item_type: row.item_type.clone(),
            parent_folder_id: row.parent_folder_id.clone(),
            name: row.name.clone(),
            name_key: row.name_key.clone(),
            record_key: row.record_key.clone(),
            link_folder_id: row.link_folder_id.clone(),
            lifecycle_state: row.lifecycle_state,
            sequencing_mode: row.sequencing_mode,
            federation_stream_id: row.federation_stream_id.clone(),
        }
    }
}

impl From<&ItemValue> for ItemState {
    fn from(value: &ItemValue) -> Self {
        Self {
            item_type: value.item_type.clone(),
            parent_folder_id: value.parent_folder_id.clone(),
            name: value.name.clone(),
            name_key: value.name_key.clone(),
            record_key: value.record_key.clone(),
            link_folder_id: value.link_folder_id.clone(),
            lifecycle_state: value.lifecycle_state,
            sequencing_mode: value.sequencing_mode,
            federation_stream_id: value.federation_stream_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedTransition {
    pub parent_folder_id_before: Option<ItemId>,
    pub parent_folder_id_after: Option<ItemId>,
}

pub fn apply_event(
    state: &mut ItemState,
    proposal: &EventProposal,
) -> Result<AppliedTransition, TransitionError> {
    let original_parent = state.parent_folder_id.clone();

    let family = proposal.event_type.split("::").next();
    if family != Some("ITEM") && family != Some(state.item_type.as_str()) {
        return Err(TransitionError::Invalid("event of another family"));
    }
    if state.is_tree() {
        if proposal.set_link.is_some() {
            return Err(TransitionError::Invalid("set_link on a tree item"));
        }
    } else if proposal.set_name.is_some() || proposal.set_parent.is_some() {
        return Err(TransitionError::Invalid("set_name or set_parent on a record"));
    }
    if proposal.replaces_content() {
        let content = proposal.set_content.as_ref().expect("event replaces content");
        match state.item_type.as_str() {
            "FILE" if content.only("bytes").is_none() => {
                return Err(TransitionError::Invalid("a FILE's content is exactly its bytes"));
            }
            "FOLDER" => return Err(TransitionError::Invalid("a FOLDER has no content")),
            _ => {}
        }
    }
    if proposal.set_tombstoned == Some(false) {
        return Err(TransitionError::Invalid("a tombstone is permanent"));
    }
    if let Some(parent) = proposal.set_parent.as_ref() {
        state.parent_folder_id.clone_from(parent);
    }
    if let Some(name) = proposal.set_name.as_ref() {
        state.name_key = Some(compute_name_key(name)?);
        state.name = Some(name.clone());
    }
    if let Some(link) = proposal.set_link.as_ref() {
        state.link_folder_id.clone_from(link);
    }
    if proposal.set_tombstoned.is_some() {
        state.lifecycle_state = LifecycleState::Tombstoned;
    }
    if let Some(in_trash) = proposal.set_in_trash {
        if in_trash && state.lifecycle_state != LifecycleState::Tombstoned {
            state.lifecycle_state = LifecycleState::Trash;
        } else if !in_trash && state.lifecycle_state == LifecycleState::Trash {
            state.lifecycle_state = LifecycleState::Live;
        }
    }
    if let Some(is_federated) = proposal.set_is_federated {
        state.sequencing_mode = if is_federated {
            SequencingMode::Federated
        } else {
            state.federation_stream_id = None;
            SequencingMode::Local
        };
    }
    if let Some(stream_id) = proposal.set_federation_stream_id.as_ref() {
        state.federation_stream_id = Some(stream_id.clone());
    }

    Ok(AppliedTransition {
        parent_folder_id_before: proposal
            .set_parent
            .is_some()
            .then_some(original_parent)
            .flatten(),
        parent_folder_id_after: state.parent_folder_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EventProposal;

    fn event() -> EventProposal {
        EventProposal {
            item_id: None,
            event_type: "FILE::UPDATE".to_string(),
            set_metadata: None,
            set_content: None,
            create_item: None,
            set_parent: None,
            set_name: None,
            set_link: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: Some(1),
        }
    }

    #[test]
    fn transition_applies_lifecycle_name_parent_and_federation() {
        let parent = ItemId::new();
        let mut proposal = event();
        proposal.set_parent = Some(Some(parent.clone()));
        proposal.set_name = Some("Report.txt".to_string());
        proposal.set_in_trash = Some(true);
        proposal.set_is_federated = Some(true);
        proposal.set_federation_stream_id = Some("stream".to_string());
        let mut state = ItemState {
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: Some("old.txt".to_string()),
            name_key: Some("old.txt".to_string()),
            record_key: None,
            link_folder_id: None,
            lifecycle_state: LifecycleState::Live,
            sequencing_mode: SequencingMode::Local,
            federation_stream_id: None,
        };
        let applied = apply_event(&mut state, &proposal).unwrap();
        assert_eq!(applied.parent_folder_id_before, None);
        assert_eq!(applied.parent_folder_id_after, Some(parent));
        assert_eq!(state.name_key.as_deref(), Some("report.txt"));
        assert_eq!(state.lifecycle_state, LifecycleState::Trash);
        assert_eq!(state.sequencing_mode, SequencingMode::Federated);
        assert_eq!(state.federation_stream_id.as_deref(), Some("stream"));
    }

    #[test]
    fn unfederation_clears_the_stream() {
        let mut proposal = event();
        proposal.set_is_federated = Some(false);
        let mut state = ItemState {
            item_type: "FILE".to_string(),
            parent_folder_id: None,
            name: Some("file.txt".to_string()),
            name_key: Some("file.txt".to_string()),
            record_key: None,
            link_folder_id: None,
            lifecycle_state: LifecycleState::Live,
            sequencing_mode: SequencingMode::Federated,
            federation_stream_id: Some("stream".to_string()),
        };
        apply_event(&mut state, &proposal).unwrap();
        assert_eq!(state.sequencing_mode, SequencingMode::Local);
        assert_eq!(state.federation_stream_id, None);
    }
}
