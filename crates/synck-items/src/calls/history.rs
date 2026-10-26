//! VCS envelope validation for canonical call events.
use super::{Call, Event, Invalid, apply};
use synck_vcs::types::{EventProposal, ItemId};

pub const ITEM_TYPE: &str = "CALL";
pub fn event_type(event: &Event) -> &'static str {
    match event {
        Event::Create { .. } => "CALL::CREATE",
        Event::Answer { .. } => "CALL::PARTICIPANT::ANSWER",
        Event::Join { .. } => "CALL::PARTICIPANT::JOIN",
        Event::End { .. } => "CALL::END",
    }
}
pub fn internal_name(id: &ItemId) -> String {
    format!("call-{id}")
}

pub fn decode(proposal: &EventProposal) -> Result<Event, Invalid> {
    let id = proposal
        .item_id
        .as_ref()
        .ok_or(Invalid("call requires item identity"))?;
    if proposal.set_parent.is_some()
        || proposal.set_name.is_some()
        || proposal.set_in_trash.is_some()
        || proposal.set_tombstoned.is_some()
        || proposal.set_is_federated.is_some()
        || proposal.set_federation_stream_id.is_some()
        || proposal.set_content.is_some()
    {
        return Err(Invalid("call forbids structural and content modifiers"));
    }
    let event: Event = serde_json::from_value(
        proposal
            .set_metadata
            .clone()
            .ok_or(Invalid("call event metadata missing"))?,
    )
    .map_err(|_| Invalid("invalid call event metadata"))?;
    if event_type(&event) != proposal.event_type {
        return Err(Invalid("call event type mismatch"));
    }
    if matches!(event, Event::Create { .. }) {
        let create = proposal
            .create_item
            .as_ref()
            .ok_or(Invalid("call create_item missing"))?;
        if create.item_type != ITEM_TYPE
            || create.parent_folder_id.is_some()
            || create.name != internal_name(id)
            || proposal.base_seq.is_some()
        {
            return Err(Invalid("invalid call creation envelope"));
        }
        if uuid::Uuid::parse_str(id.as_str())
            .map_err(|_| Invalid("invalid call ID"))?
            .get_version_num()
            != 7
        {
            return Err(Invalid("call ID must be UUIDv7"));
        }
    } else if proposal.create_item.is_some() || proposal.base_seq.is_none() {
        return Err(Invalid("call transition requires existing item version"));
    }
    Ok(event)
}

pub fn reduce(previous: Option<&Call>, proposal: &EventProposal) -> Result<Call, Invalid> {
    apply(previous, &decode(proposal)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synck_vcs::types::{CreateItem, EventContent};
    fn create() -> EventProposal {
        let id = ItemId::new();
        let event = Event::Create {
            creation: super::super::tests::start(false).creation,
        };
        EventProposal {
            item_id: Some(id.clone()),
            event_type: event_type(&event).into(),
            create_item: Some(CreateItem {
                item_type: ITEM_TYPE.into(),
                name: internal_name(&id),
                parent_folder_id: None,
            }),
            set_metadata: Some(serde_json::to_value(event).unwrap()),
            set_content: None,
            set_parent: None,
            set_name: None,
            set_tombstoned: None,
            set_in_trash: None,
            set_is_federated: None,
            set_federation_stream_id: None,
            base_seq: None,
        }
    }
    #[test]
    fn uuid_v7_non_tree_create_round_trips() {
        let proposal = create();
        assert!(reduce(None, &proposal).is_ok());
        assert!(proposal.create_item.unwrap().parent_folder_id.is_none());
        assert_eq!(
            uuid::Uuid::parse_str(proposal.item_id.unwrap().as_str())
                .unwrap()
                .get_version_num(),
            7
        );
    }
    #[test]
    fn rejects_tree_and_generic_content_mutations() {
        let mut proposal = create();
        proposal.create_item.as_mut().unwrap().parent_folder_id = Some(ItemId::new());
        assert!(decode(&proposal).is_err());
        let mut proposal = create();
        proposal.set_content = Some(EventContent::Json {
            json: serde_json::json!({}),
        });
        assert!(decode(&proposal).is_err());
        let mut proposal = create();
        proposal.event_type = "CALL::SET_METADATA".into();
        assert!(decode(&proposal).is_err());
    }
}
