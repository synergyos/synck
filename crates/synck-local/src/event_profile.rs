use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
use synck_file_folder::project_commit;
use synck_file_folder::{ImmutableItemKind, validate_projected_proposal, project_commit_events};
use synck_vcs::naming::compute_name_key;
use synck_vcs::types::{
    BlobId, CommitEventRow, CommitRow, EventContent, EventProposal, EventRow, ItemId,
};

pub(crate) enum LocalEvent {
    Create {
        item_type: &'static str,
    },
    Trash {
        item_type: &'static str,
    },
    Restore {
        item_type: &'static str,
    },
    SetParent {
        item_type: &'static str,
        parent_item_id: Option<ItemId>,
    },
    SetName {
        item_type: &'static str,
        name: String,
    },
    SetContent {
        blob_id: BlobId,
        size: i64,
    },
    VersionSetCreate,
    VersionSetMutation,
}

pub(crate) struct ClassifiedEvent {
    pub item_id: ItemId,
    pub item_seq: i64,
    pub proposal: EventProposal,
    pub profile: LocalEvent,
}

pub(crate) fn commit_item_kind_requirements(
    events: &[EventRow],
) -> std::result::Result<(BTreeSet<ItemId>, BTreeMap<ItemId, String>), String> {
    let decoded = decode_events(events)?;
    Ok(item_kind_requirements(events, &decoded))
}

#[cfg(test)]
pub(crate) fn classify_commit(
    commit: &CommitRow,
    events: &[EventRow],
    item_kinds: &BTreeMap<ItemId, String>,
) -> std::result::Result<Vec<ClassifiedEvent>, String> {
    let decoded = decode_events(events)?;
    let (target_ids, created_types) = item_kind_requirements(events, &decoded);
    let immutable_kinds = target_ids
        .iter()
        .map(|item_id| {
            item_kinds
                .get(item_id)
                .cloned()
                .map(|item_type| ImmutableItemKind {
                    item_id: item_id.clone(),
                    item_type,
                })
                .ok_or_else(|| format!("missing immutable item kind for {item_id}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let group =
        project_commit(commit, events, &immutable_kinds).map_err(|error| error.to_string())?;
    let mut visible = group
        .into_iter()
        .flat_map(|group| group.changes)
        .map(|change| ((change.item_id, change.item_seq), change.event))
        .collect::<HashMap<_, _>>();
    let mut output = Vec::with_capacity(events.len());
    for (row, original) in events.iter().zip(decoded) {
        if let Some(proposal) = visible.remove(&(row.item_id.clone(), row.item_seq)) {
            output.push(ClassifiedEvent {
                item_id: row.item_id.clone(),
                item_seq: row.item_seq,
                profile: classify_projected_event(&proposal)?,
                proposal,
            });
            continue;
        }
        let item_type = original
            .create_item
            .as_ref()
            .map(|create| create.item_type.as_str())
            .or_else(|| created_types.get(&row.item_id).map(String::as_str))
            .or_else(|| item_kinds.get(&row.item_id).map(String::as_str))
            .ok_or_else(|| format!("missing immutable item kind for {}", row.item_id))?;
        if item_type == synck_file_folder::file_version_set::ITEM_TYPE {
            output.push(ClassifiedEvent {
                item_id: row.item_id.clone(),
                item_seq: row.item_seq,
                profile: classify_version_event(&original)?,
                proposal: original,
            });
        } else {
            return Err(format!(
                "unsupported local event {} targeting {item_type}",
                original.event_type
            ));
        }
    }
    if !visible.is_empty() {
        return Err("projected FILE/FOLDER changes do not match commit events".to_string());
    }
    Ok(output)
}

pub(crate) fn classify_complete_commit(
    commit: &CommitRow,
    events: &[CommitEventRow],
    item_kinds: &BTreeMap<ItemId, String>,
) -> std::result::Result<Vec<ClassifiedEvent>, String> {
    let immutable_kinds = item_kinds
        .iter()
        .map(|(item_id, item_type)| ImmutableItemKind {
            item_id: item_id.clone(),
            item_type: item_type.clone(),
        })
        .collect::<Vec<_>>();
    let group = project_commit_events(commit, events, &immutable_kinds)
        .map_err(|error| error.to_string())?;
    group
        .into_iter()
        .flat_map(|group| group.changes)
        .map(|change| {
            let profile = classify_projected_event(&change.event)?;
            Ok(ClassifiedEvent {
                item_id: change.item_id,
                item_seq: change.item_seq,
                proposal: change.event,
                profile,
            })
        })
        .collect()
}

fn decode_events(events: &[EventRow]) -> std::result::Result<Vec<EventProposal>, String> {
    events
        .iter()
        .map(|event| {
            serde_json::from_slice::<EventProposal>(&event.payload)
                .map_err(|error| format!("invalid canonical event payload: {error}"))
        })
        .collect()
}

fn item_kind_requirements(
    events: &[EventRow],
    decoded: &[EventProposal],
) -> (BTreeSet<ItemId>, BTreeMap<ItemId, String>) {
    let created_types = events
        .iter()
        .zip(decoded)
        .filter_map(|(event, proposal)| {
            proposal
                .create_item
                .as_ref()
                .map(|create| (event.item_id.clone(), create.item_type.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let target_ids = events
        .iter()
        .map(|event| event.item_id.clone())
        .filter(|item_id| !created_types.contains_key(item_id))
        .collect();
    (target_ids, created_types)
}

pub(crate) fn classify_proposal_commit(
    message: Option<&str>,
    events: &[EventProposal],
    item_kinds: &BTreeMap<ItemId, String>,
) -> std::result::Result<Vec<LocalEvent>, String> {
    let (target_ids, created_types) = proposal_item_kind_requirements(events);
    let immutable_kinds = target_ids
        .iter()
        .map(|item_id| {
            item_kinds
                .get(item_id)
                .cloned()
                .map(|item_type| ImmutableItemKind {
                    item_id: item_id.clone(),
                    item_type,
                })
                .ok_or_else(|| format!("missing immutable item kind for {item_id}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_projected_proposal(message, events, &immutable_kinds)
        .map_err(|error| error.to_string())?;
    events
        .iter()
        .map(|event| {
            let item_id = event
                .item_id
                .as_ref()
                .ok_or_else(|| "local event has no item ID".to_string())?;
            let item_type = event
                .create_item
                .as_ref()
                .map(|create| create.item_type.as_str())
                .or_else(|| created_types.get(item_id).map(String::as_str))
                .or_else(|| item_kinds.get(item_id).map(String::as_str))
                .ok_or_else(|| format!("missing immutable item kind for {item_id}"))?;
            match item_type {
                "FILE" | "FOLDER" => classify_projected_event(event),
                other if other == synck_file_folder::file_version_set::ITEM_TYPE => {
                    classify_version_event(event)
                }
                other => Err(format!("unsupported local item type {other}")),
            }
        })
        .collect()
}

pub(crate) fn proposal_item_kind_requirements(
    events: &[EventProposal],
) -> (BTreeSet<ItemId>, BTreeMap<ItemId, String>) {
    let created_types = events
        .iter()
        .filter_map(|event| event.create_item.as_ref().zip(event.item_id.as_ref()))
        .map(|(create, item_id)| (item_id.clone(), create.item_type.clone()))
        .collect::<BTreeMap<_, _>>();
    let target_ids = events
        .iter()
        .filter_map(|event| event.item_id.clone())
        .filter(|item_id| !created_types.contains_key(item_id))
        .collect();
    (target_ids, created_types)
}

fn classify_projected_event(event: &EventProposal) -> std::result::Result<LocalEvent, String> {
    let item_type = match event.event_type.split_once("::") {
        Some(("FILE", _)) => "FILE",
        Some(("FOLDER", _)) => "FOLDER",
        _ => return Err(format!("unsupported projected event {}", event.event_type)),
    };
    match event.event_type.as_str() {
        "FOLDER::CREATE" | "FILE::CREATE" => Ok(LocalEvent::Create { item_type }),
        "FOLDER::SET_PARENT" | "FILE::SET_PARENT" => Ok(LocalEvent::SetParent {
            item_type,
            parent_item_id: event
                .set_parent
                .clone()
                .expect("shared projector validated SET_PARENT"),
        }),
        "FOLDER::SET_NAME" | "FILE::SET_NAME" => Ok(LocalEvent::SetName {
            item_type,
            name: event
                .set_name
                .clone()
                .expect("shared projector validated SET_NAME"),
        }),
        "FOLDER::TRASH" | "FILE::TRASH" => Ok(LocalEvent::Trash { item_type }),
        "FOLDER::RESTORE" | "FILE::RESTORE" => Ok(LocalEvent::Restore { item_type }),
        "FILE::SET_CONTENT" => {
            let (blob_id, size) = parse_content(event)?;
            Ok(LocalEvent::SetContent { blob_id, size })
        }
        _ => Err(format!("unsupported projected event {}", event.event_type)),
    }
}

fn classify_version_event(event: &EventProposal) -> std::result::Result<LocalEvent, String> {
    let common_modifiers = event.set_parent.is_some()
        || event.set_name.is_some()
        || event.set_tombstoned.is_some()
        || event.set_is_federated.is_some()
        || event.set_federation_stream_id.is_some();
    match event.event_type.as_str() {
        synck_file_folder::file_version_set::CREATE_EVENT => {
            let valid = event.item_id.is_some()
                && event.create_item.as_ref().is_some_and(|create| {
                    create.item_type == synck_file_folder::file_version_set::ITEM_TYPE
                        && create.parent_folder_id.is_none()
                        && synck_file_folder::file_version_set::file_item_id_from_name(&create.name)
                            .is_ok()
                })
                && event.set_metadata.is_none()
                && event.set_content.is_none()
                && event.set_in_trash.is_none()
                && event.base_seq.is_none()
                && !common_modifiers;
            valid
                .then_some(LocalEvent::VersionSetCreate)
                .ok_or_else(|| "invalid local FILE_VERSION_SET::CREATE event shape".to_string())
        }
        other
            if synck_file_folder::file_version_set::is_event_type(other)
                && other != synck_file_folder::file_version_set::CREATE_EVENT =>
        {
            let has_content = event.set_content.is_some();
            let valid_content = if other
                == synck_file_folder::file_version_set::VERSION_CREATE_EVENT
                || other == synck_file_folder::file_version_set::VERSION_REPLACE_CONTENT_EVENT
            {
                matches!(event.set_content, Some(EventContent::Blob { .. }))
            } else {
                !has_content
            };
            let valid = event.item_id.is_some()
                && event.create_item.is_none()
                && event
                    .set_metadata
                    .as_ref()
                    .is_some_and(serde_json::Value::is_object)
                && valid_content
                && event.set_in_trash.is_none()
                && event.base_seq.is_none_or(|seq| seq > 0)
                && !common_modifiers;
            valid
                .then_some(LocalEvent::VersionSetMutation)
                .ok_or_else(|| format!("invalid local {other} event shape"))
        }
        other => Err(format!(
            "unsupported local FILE_VERSION_SET event type {other}"
        )),
    }
}

pub(crate) fn is_reserved_root_name(name: &str) -> bool {
    compute_name_key(name).is_ok_and(|name_key| {
        synck_file_folder::file_version_set::is_reserved_namespace_name_key(&name_key)
    })
}

pub(crate) fn parse_content(event: &EventProposal) -> std::result::Result<(BlobId, i64), String> {
    let blob_id = match event.set_content.as_ref() {
        Some(EventContent::Blob { blob_id }) => {
            BlobId::parse(blob_id).map_err(|error| error.to_string())?
        }
        _ => return Err("file content event has no blob".to_string()),
    };
    let content = event
        .set_metadata
        .as_ref()
        .and_then(|value| value.as_object())
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("content"))
        .and_then(|value| value.as_object())
        .filter(|object| object.len() == 1);
    let size = content
        .and_then(|object| object.get("size"))
        .and_then(serde_json::Value::as_i64)
        .filter(|size| *size >= 0)
        .ok_or_else(|| "file content event has invalid size metadata".to_string())?;
    Ok((blob_id, size))
}

pub(crate) fn parse_version_content(
    event: &EventProposal,
) -> std::result::Result<(BlobId, Option<i64>), String> {
    let blob_id = match event.set_content.as_ref() {
        Some(EventContent::Blob { blob_id }) => {
            BlobId::parse(blob_id).map_err(|error| error.to_string())?
        }
        _ => return Err("version content event has no blob".to_string()),
    };
    let size = match event
        .set_metadata
        .as_ref()
        .and_then(|value| value.pointer("/content/size"))
    {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|size| *size >= 0)
                .ok_or_else(|| "version content event has an invalid size".to_string())?,
        ),
    };
    Ok((blob_id, size))
}
