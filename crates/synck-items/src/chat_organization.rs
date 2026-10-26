//! Repository-local canonical CHAT organization, independent of shared CHAT streams.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synck_vcs::types::{EventProposal, ItemId, RepoId};
use uuid::Uuid;

use crate::MutationPolicy;

pub const ITEM_TYPE: &str = "CHAT_ORGANIZATION";
pub const CREATE_EVENT: &str = "CHAT_ORGANIZATION::CREATE";
pub const SET_METADATA_EVENT: &str = "CHAT_ORGANIZATION::SET_METADATA";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChatAclMode {
    ItemOnly,
    Combine,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChatOrganization {
    pub chat_item_id: ItemId,
    pub workspace_item_id: Option<ItemId>,
    pub item_acl_mode: ChatAclMode,
    /// Organization CAS revision, independent of this owner's item sequence.
    pub revision: i64,
}

/// Derive one local organization owner for a CHAT in this repository.
pub fn item_id(repo_id: &RepoId, chat_item_id: &ItemId) -> Result<ItemId, String> {
    let repo = Uuid::parse_str(repo_id.as_str()).map_err(|error| error.to_string())?;
    let chat = Uuid::parse_str(chat_item_id.as_str()).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(b"synck:chat-organization:item-id:v1");
    hasher.update(repo.as_bytes());
    hasher.update(chat.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    ItemId::parse(Uuid::from_bytes(bytes).to_string()).map_err(|error| error.to_string())
}

pub fn internal_name(chat_item_id: &ItemId) -> String {
    format!("chat-organization-{chat_item_id}")
}

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    matches!(event_type, CREATE_EVENT | SET_METADATA_EVENT).then_some(MutationPolicy::Optimistic)
}

pub fn parse_metadata(value: &serde_json::Value) -> Result<ChatOrganization, String> {
    if !value
        .as_object()
        .is_some_and(|metadata| metadata.contains_key("workspace_item_id"))
    {
        return Err("CHAT organization requires an explicit workspace_item_id or null".into());
    }
    let state: ChatOrganization =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    if state.revision < 1 {
        return Err("CHAT organization revision must be positive".into());
    }
    Ok(state)
}

pub fn validate_identity(
    repo_id: &RepoId,
    owner_item_id: &ItemId,
    organization: &ChatOrganization,
) -> Result<(), String> {
    if item_id(repo_id, &organization.chat_item_id)? != *owner_item_id {
        return Err("CHAT organization owner differs from its deterministic identity".into());
    }
    Ok(())
}

/// Validate the closed metadata-only vocabulary. Target CHAT lifecycle and
/// sequencing do not apply to this separate local owner.
pub fn validate_event(event: &EventProposal) -> Result<ChatOrganization, String> {
    if event.item_id.is_none()
        || event.set_parent.is_some()
        || event.set_name.is_some()
        || event.set_content.is_some()
        || event.set_tombstoned.is_some()
        || event.set_in_trash.is_some()
        || event.set_is_federated.is_some()
        || event.set_federation_stream_id.is_some()
    {
        return Err("CHAT organization events require explicit identity and metadata only".into());
    }
    let organization = parse_metadata(
        event
            .set_metadata
            .as_ref()
            .ok_or("CHAT organization metadata is required")?,
    )?;
    match event.event_type.as_str() {
        CREATE_EVENT => {
            let create = event
                .create_item
                .as_ref()
                .ok_or("CHAT organization CREATE requires create_item")?;
            if event.base_seq.is_some()
                || create.item_type != ITEM_TYPE
                || create.parent_folder_id.is_some()
                || create.name != internal_name(&organization.chat_item_id)
            {
                return Err("invalid CHAT organization creation envelope".into());
            }
        }
        SET_METADATA_EVENT => {
            if event.create_item.is_some() || event.base_seq.is_none_or(|seq| seq < 1) {
                return Err("CHAT organization update requires an existing owner base".into());
            }
        }
        _ => return Err("unsupported CHAT organization event".into()),
    }
    Ok(organization)
}

pub fn reduce(
    previous: Option<&ChatOrganization>,
    event: &EventProposal,
) -> Result<ChatOrganization, String> {
    let next = validate_event(event)?;
    match previous {
        None if event.event_type != CREATE_EVENT => {
            return Err("first CHAT organization event must create its owner".into());
        }
        Some(previous) => {
            if event.event_type != SET_METADATA_EVENT || previous.chat_item_id != next.chat_item_id
            {
                return Err("CHAT organization identity is immutable".into());
            }
            if previous.revision.checked_add(1) != Some(next.revision) {
                return Err("CHAT organization revision must advance by one".into());
            }
        }
        None => {}
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn organization_has_one_identity_and_a_closed_monotonic_history() {
        let repo = RepoId::new();
        let chat = ItemId::new();
        let owner = item_id(&repo, &chat).unwrap();
        assert_ne!(item_id(&RepoId::new(), &chat).unwrap(), owner);
        let create = json!({"item_id":owner,"event_type":CREATE_EVENT,
            "create_item":{"item_type":ITEM_TYPE,"name":internal_name(&chat),"parent_folder_id":null},
            "set_metadata":{"chat_item_id":chat,"workspace_item_id":null,"item_acl_mode":"ITEM_ONLY","revision":7}});
        let initial: EventProposal = serde_json::from_value(create).unwrap();
        let state = reduce(None, &initial).unwrap();
        validate_identity(&repo, &owner, &state).unwrap();
        assert!(validate_identity(&repo, &ItemId::new(), &state).is_err());
        let update = json!({"item_id":owner,"event_type":SET_METADATA_EVENT,"base_seq":1,
            "set_metadata":{"chat_item_id":chat,"workspace_item_id":ItemId::new(),"item_acl_mode":"COMBINE","revision":8}});
        let next: EventProposal = serde_json::from_value(update.clone()).unwrap();
        assert_eq!(reduce(Some(&state), &next).unwrap().revision, 8);
        assert!(reduce(None, &next).is_err());
        assert!(reduce(Some(&state), &initial).is_err());
        for (key, value) in [
            ("set_parent", json!(null)),
            ("set_name", json!("renamed")),
            ("set_content", json!({"text":"body"})),
            ("set_tombstoned", json!(true)),
            ("set_in_trash", json!(true)),
            ("set_is_federated", json!(true)),
            ("set_federation_stream_id", json!("other")),
        ] {
            let mut invalid = update.clone();
            invalid[key] = value;
            assert!(
                validate_event(&serde_json::from_value(invalid).unwrap()).is_err(),
                "{key}"
            );
        }
        for (key, value) in [
            ("chat_item_id", json!(ItemId::new())),
            ("revision", json!(7)),
            ("revision", json!(0)),
            ("item_acl_mode", json!("UNKNOWN")),
            ("unrecognized", json!(true)),
        ] {
            let mut invalid = update.clone();
            invalid["set_metadata"][key] = value;
            assert!(
                reduce(Some(&state), &serde_json::from_value(invalid).unwrap()).is_err(),
                "{key}"
            );
        }
        let mut missing = update;
        missing["set_metadata"]
            .as_object_mut()
            .unwrap()
            .remove("workspace_item_id");
        assert!(validate_event(&serde_json::from_value(missing).unwrap()).is_err());
    }
}
