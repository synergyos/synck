use std::collections::{BTreeMap, BTreeSet};

use synck_file_folder::{
    BlobRef, ChangeGroup, FileFolderKind, ImmutableItemKind, Intent, compute_operation_id,
    is_profile_event_type, normalize_intents, project_proposal_events,
};
use synck_vcs::naming::compute_name_key;
use synck_vcs::types::{FILE_FOLDER_MAX_CLOSURE_DESCENDANTS, ItemId, RepoId, Sha256Digest};

use crate::{
    CaptureKind, CaptureRequest, CaptureResponse, ChangePageResponse, Checkpoint,
    DiscoveryResponse, ErrorCode, HaveNeedRequest, HaveNeedResponse, HaveNeedState,
    MAX_BLOB_DECLARATIONS, MAX_COMMITS_PER_CHANGE_PAGE, MAX_JSON_BYTES,
    MAX_MANIFEST_ENTRIES_PER_PAGE, MAX_REPOSITORIES_PER_PAGE, MAX_REPOSITORY_NAME_BYTES,
    ManifestLifecycle, ManifestPageResponse, OperationReceipt, OperationRequest,
    RepositoryListQuery, RepositoryPage, ValidationError, encode_json,
};

pub fn validate_discovery(response: &DiscoveryResponse) -> Result<(), ValidationError> {
    let server_id = RepoId::parse(&response.server_id)
        .map_err(|_| ValidationError::new(ErrorCode::InvalidIdentifier))?;
    if server_id.as_str() != response.server_id
        || response.hash != "sha256"
        || response.limits.max_json_bytes != MAX_JSON_BYTES as u64
        || response.limits.max_intents_per_commit != synck_file_folder::MAX_INTENTS as u64
        || response.limits.max_events_per_commit != synck_file_folder::MAX_EVENTS as u64
        || response.limits.max_lifecycle_events_per_commit != synck_file_folder::MAX_EVENTS as u64
        || response.limits.max_lifecycle_descendants != FILE_FOLDER_MAX_CLOSURE_DESCENDANTS as u64
        || response.limits.max_event_payload_bytes
            != synck_file_folder::MAX_EVENT_PAYLOAD_BYTES as u64
        || response.limits.max_commit_event_payload_bytes
            != synck_file_folder::MAX_COMMIT_PAYLOAD_BYTES as u64
        || response.limits.max_manifest_entries_per_page != MAX_MANIFEST_ENTRIES_PER_PAGE as u64
        || response.limits.max_commits_per_change_page != MAX_COMMITS_PER_CHANGE_PAGE as u64
        || response.limits.max_projected_group_bytes
            != synck_file_folder::MAX_PROJECTED_GROUP_BYTES as u64
        || !(1..=synck_file_folder::MAX_SAFE_JSON_INTEGER).contains(&response.limits.max_blob_bytes)
        || response.transfer.upload_resume
        || !response.transfer.download_single_range
    {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    Ok(())
}

pub fn validate_repository_page(page: &RepositoryPage) -> Result<(), ValidationError> {
    if page.repositories.len() > MAX_REPOSITORIES_PER_PAGE {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    let mut ids = BTreeSet::new();
    for repository in &page.repositories {
        if repository.name.len() > MAX_REPOSITORY_NAME_BYTES {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
        if !ids.insert(repository.repo_id.clone()) {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
    }
    Ok(())
}

pub fn validate_repository_list_query(query: &RepositoryListQuery) -> Result<(), ValidationError> {
    if query.effective_limit() == 0 || query.effective_limit() > MAX_REPOSITORIES_PER_PAGE {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    Ok(())
}

pub fn parse_single_byte_range(
    header: &str,
    object_size: u64,
) -> Result<crate::ByteRange, ValidationError> {
    if object_size == 0 || object_size > synck_file_folder::MAX_SAFE_JSON_INTEGER {
        return Err(ValidationError::new(ErrorCode::InvalidRange));
    }
    let Some(range) = header.strip_prefix("bytes=") else {
        return Err(ValidationError::new(ErrorCode::InvalidRange));
    };
    if range.contains(',') {
        return Err(ValidationError::new(ErrorCode::InvalidRange));
    }
    let Some((start, end)) = range.split_once('-') else {
        return Err(ValidationError::new(ErrorCode::InvalidRange));
    };
    if start.is_empty() || end.is_empty() || end.contains('-') {
        return Err(ValidationError::new(ErrorCode::InvalidRange));
    }
    if !start.bytes().all(|byte| byte.is_ascii_digit())
        || !end.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ValidationError::new(ErrorCode::InvalidRange));
    }
    let start = start
        .parse::<u64>()
        .map_err(|_| ValidationError::new(ErrorCode::InvalidRange))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| ValidationError::new(ErrorCode::InvalidRange))?;
    if start > end || end >= object_size {
        return Err(ValidationError::new(ErrorCode::InvalidRange));
    }
    Ok(crate::ByteRange { start, end })
}

pub fn parse_canonical_repo_id(raw: &str) -> Result<RepoId, ValidationError> {
    let parsed =
        RepoId::parse(raw).map_err(|_| ValidationError::new(ErrorCode::InvalidIdentifier))?;
    if parsed.as_str() != raw {
        return Err(ValidationError::new(ErrorCode::InvalidIdentifier));
    }
    Ok(parsed)
}

pub fn parse_canonical_sha256(raw: &str) -> Result<Sha256Digest, ValidationError> {
    let parsed =
        Sha256Digest::parse(raw).map_err(|_| ValidationError::new(ErrorCode::InvalidIdentifier))?;
    if parsed.as_str() != raw {
        return Err(ValidationError::new(ErrorCode::InvalidIdentifier));
    }
    Ok(parsed)
}

pub fn validate_checkpoint(checkpoint: &Checkpoint) -> Result<(), ValidationError> {
    if (checkpoint.repo_seq == 0) != checkpoint.tip_commit_id.is_none() {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    Ok(())
}

pub fn validate_capture_request(request: &CaptureRequest) -> Result<(), ValidationError> {
    match (&request.kind, &request.from_checkpoint) {
        (CaptureKind::Clone, None) => Ok(()),
        (CaptureKind::Pull, Some(checkpoint)) => validate_checkpoint(checkpoint),
        (CaptureKind::Clone, Some(_)) | (CaptureKind::Pull, None) => {
            Err(ValidationError::new(ErrorCode::InvalidRequest))
        }
    }
}

pub fn validate_capture_response(
    request: &CaptureRequest,
    response: &CaptureResponse,
) -> Result<(), ValidationError> {
    validate_capture_request(request)?;
    validate_checkpoint(&response.target_checkpoint)?;
    if response.kind != request.kind
        || request
            .from_checkpoint
            .as_ref()
            .is_some_and(|from| response.target_checkpoint.repo_seq < from.repo_seq)
    {
        return Err(ValidationError::new(ErrorCode::CursorBindingMismatch));
    }
    Ok(())
}

pub fn validate_manifest_page(
    expected_target: &Checkpoint,
    page: &ManifestPageResponse,
) -> Result<(), ValidationError> {
    validate_checkpoint(expected_target)?;
    validate_checkpoint(&page.target_checkpoint)?;
    if &page.target_checkpoint != expected_target {
        return Err(ValidationError::new(ErrorCode::CursorBindingMismatch));
    }
    if page.entries.len() > MAX_MANIFEST_ENTRIES_PER_PAGE {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    let mut item_ids = BTreeSet::new();
    for entry in &page.entries {
        if !item_ids.insert(entry.item_id.clone()) || entry.head_seq <= 0 {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
        compute_name_key(&entry.name).map_err(|_| ValidationError::new(ErrorCode::InvalidName))?;
        match (entry.item_type, &entry.content) {
            (FileFolderKind::File, Some(content)) => validate_blob(content)?,
            (FileFolderKind::Folder, None) => {}
            (FileFolderKind::File, None) | (FileFolderKind::Folder, Some(_)) => {
                return Err(ValidationError::new(ErrorCode::InvalidContent));
            }
        }
        match (entry.lifecycle, &entry.trash_commit_id) {
            (ManifestLifecycle::Live, None) | (ManifestLifecycle::Trash, Some(_)) => {}
            (ManifestLifecycle::Live, Some(_)) | (ManifestLifecycle::Trash, None) => {
                return Err(ValidationError::new(ErrorCode::InvalidRequest));
            }
        }
    }
    Ok(())
}

pub fn validate_change_page(
    expected_repo_id: &RepoId,
    from_checkpoint: &Checkpoint,
    target_checkpoint: &Checkpoint,
    page: &ChangePageResponse,
) -> Result<(), ValidationError> {
    encode_json(page)?;
    validate_checkpoint(from_checkpoint)?;
    validate_checkpoint(target_checkpoint)?;
    validate_checkpoint(&page.target_checkpoint)?;
    validate_checkpoint(&page.page_through_checkpoint)?;
    let empty_interval = from_checkpoint == target_checkpoint;
    if &page.target_checkpoint != target_checkpoint
        || (!empty_interval && page.page_through_checkpoint.repo_seq <= from_checkpoint.repo_seq)
        || (empty_interval
            && (&page.page_through_checkpoint != target_checkpoint
                || !page.groups.is_empty()
                || page.next_cursor.is_some()))
        || page.page_through_checkpoint.repo_seq > target_checkpoint.repo_seq
        || (page.next_cursor.is_none() && &page.page_through_checkpoint != target_checkpoint)
        || (page.next_cursor.is_some()
            && page.page_through_checkpoint.repo_seq >= target_checkpoint.repo_seq)
    {
        return Err(ValidationError::new(ErrorCode::CursorBindingMismatch));
    }

    if page.groups.len() > MAX_COMMITS_PER_CHANGE_PAGE {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    let mut previous_seq = from_checkpoint.repo_seq;
    for group in &page.groups {
        validate_change_group(expected_repo_id, group)?;
        let repo_seq = u64::try_from(group.repo_seq)
            .map_err(|_| ValidationError::new(ErrorCode::InvalidRequest))?;
        if repo_seq <= previous_seq || repo_seq > page.page_through_checkpoint.repo_seq {
            return Err(ValidationError::new(ErrorCode::CursorBindingMismatch));
        }
        if repo_seq == page.page_through_checkpoint.repo_seq
            && page.page_through_checkpoint.tip_commit_id.as_ref() != Some(&group.commit_id)
        {
            return Err(ValidationError::new(ErrorCode::CursorBindingMismatch));
        }
        previous_seq = repo_seq;
    }
    Ok(())
}

pub fn validate_have_need_request(request: &HaveNeedRequest) -> Result<(), ValidationError> {
    if request.blobs.len() > MAX_BLOB_DECLARATIONS {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    let mut ids = BTreeSet::new();
    for blob in &request.blobs {
        validate_blob(blob)?;
        if !ids.insert(blob.blob_id.clone()) {
            return Err(ValidationError::new(ErrorCode::InvalidContent));
        }
    }
    Ok(())
}

pub fn validate_have_need_response(
    repo_id: &RepoId,
    request: &HaveNeedRequest,
    response: &HaveNeedResponse,
) -> Result<(), ValidationError> {
    validate_have_need_request(request)?;
    if response.results.len() != request.blobs.len() {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    for (declared, result) in request.blobs.iter().zip(&response.results) {
        if result.blob_id != declared.blob_id || result.size != declared.size {
            return Err(ValidationError::new(ErrorCode::InvalidBlobProof));
        }
        match (result.state, &result.upload) {
            (HaveNeedState::Have, None) => {}
            (HaveNeedState::Need, Some(upload))
                if upload.method == "PUT"
                    && upload.path
                        == format!(
                            "/synck/v1/repositories/{repo_id}/blob-uploads/{}",
                            upload.upload_id
                        ) => {}
            (HaveNeedState::Have, Some(_)) | (HaveNeedState::Need, None | Some(_)) => {
                return Err(ValidationError::new(ErrorCode::InvalidRequest));
            }
        }
    }
    Ok(())
}

pub fn validate_operation_request(
    repo_id: &RepoId,
    path_operation_id: &Sha256Digest,
    request: &OperationRequest,
) -> Result<Vec<Intent>, ValidationError> {
    let intents = normalize_intents(&request.intents)?;
    let actual = compute_operation_id(repo_id, request.message.as_deref(), &intents)?;
    if &actual != path_operation_id {
        return Err(ValidationError::new(ErrorCode::OperationIdMismatch));
    }

    let mut blobs = BTreeMap::new();
    for intent in &intents {
        let content = match intent {
            Intent::CreateFile { content, .. }
            | Intent::UpdateFile {
                content: Some(content),
                ..
            } => Some(content),
            _ => None,
        };
        if let Some(content) = content {
            validate_blob(content)?;
            if blobs
                .insert(content.blob_id.clone(), content.size)
                .is_some_and(|size| size != content.size)
            {
                return Err(ValidationError::new(ErrorCode::InvalidContent));
            }
        }
    }
    let unique_proofs: BTreeSet<_> = request.blob_proofs.iter().collect();
    if request.blob_proofs.len() != blobs.len() || unique_proofs.len() != request.blob_proofs.len()
    {
        return Err(ValidationError::new(ErrorCode::InvalidBlobProof));
    }
    Ok(intents)
}

pub fn validate_operation_receipt(
    expected_repo_id: &RepoId,
    expected_operation_id: &Sha256Digest,
    receipt: &OperationReceipt,
) -> Result<(), ValidationError> {
    if &receipt.repo_id != expected_repo_id
        || &receipt.operation_id != expected_operation_id
        || &receipt.group.repo_id != expected_repo_id
    {
        return Err(ValidationError::new(ErrorCode::CursorBindingMismatch));
    }
    validate_change_group(expected_repo_id, &receipt.group)
}

pub fn validate_change_group(
    expected_repo_id: &RepoId,
    group: &ChangeGroup,
) -> Result<(), ValidationError> {
    if &group.repo_id != expected_repo_id || group.repo_seq <= 0 {
        return Err(ValidationError::new(ErrorCode::CursorBindingMismatch));
    }
    if group.item_heads.is_empty() {
        return Err(ValidationError::new(ErrorCode::InvalidRequest));
    }
    if group
        .message
        .as_ref()
        .is_some_and(|message| message.len() > synck_file_folder::MAX_MESSAGE_BYTES)
    {
        return Err(ValidationError::new(ErrorCode::MessageTooLarge));
    }

    let mut heads = BTreeMap::new();
    let mut previous_head: Option<&ItemId> = None;
    for head in &group.item_heads {
        if head.head_seq <= 0
            || previous_head.is_some_and(|item_id| item_id >= &head.item_id)
            || heads
                .insert(head.item_id.clone(), (head.item_type, head.head_seq))
                .is_some()
        {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
        previous_head = Some(&head.item_id);
    }

    let mut previous_item_seq = BTreeMap::new();
    for change in &group.changes {
        let Some((head_kind, head_seq)) = heads.get(&change.item_id) else {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        };
        if change.item_seq <= 0
            || change.item_type != *head_kind
            || change.item_seq > *head_seq
            || change.event.item_id.as_ref() != Some(&change.item_id)
            || !is_profile_event_type(change.item_type, &change.event.event_type)
            || previous_item_seq
                .insert(change.item_id.clone(), change.item_seq)
                .is_some_and(|previous| previous >= change.item_seq)
        {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
    }

    let events: Vec<_> = group
        .changes
        .iter()
        .map(|change| change.event.clone())
        .collect();
    let created = group
        .changes
        .iter()
        .filter(|change| change.event.create_item.is_some())
        .map(|change| change.item_id.clone())
        .collect::<BTreeSet<_>>();
    let mut kinds = BTreeMap::new();
    for change in &group.changes {
        if !created.contains(&change.item_id) {
            kinds.insert(
                change.item_id.clone(),
                match change.item_type {
                    FileFolderKind::File => "FILE",
                    FileFolderKind::Folder => "FOLDER",
                }
                .to_string(),
            );
        }
    }
    let kinds: Vec<_> = kinds
        .into_iter()
        .map(|(item_id, item_type)| ImmutableItemKind { item_id, item_type })
        .collect();
    if !events.is_empty() {
        let projected = project_proposal_events(&events, &kinds)?;
        if serde_json::to_value(&projected)
            .map_err(|_| ValidationError::new(ErrorCode::InternalError))?
            != serde_json::to_value(&events)
                .map_err(|_| ValidationError::new(ErrorCode::InternalError))?
        {
            return Err(ValidationError::new(ErrorCode::InvalidRequest));
        }
    }

    validate_projected_group_size(group)
}

pub fn validate_projected_group_size(group: &ChangeGroup) -> Result<(), ValidationError> {
    let value =
        serde_json::to_value(group).map_err(|_| ValidationError::new(ErrorCode::InternalError))?;
    let bytes = serde_json_canonicalizer::to_vec(&value)
        .map_err(|_| ValidationError::new(ErrorCode::InternalError))?;
    if bytes.len() > synck_file_folder::MAX_PROJECTED_GROUP_BYTES {
        return Err(ValidationError::new(ErrorCode::ProjectedGroupTooLarge));
    }
    Ok(())
}

fn validate_blob(blob: &BlobRef) -> Result<(), ValidationError> {
    if blob.size > synck_file_folder::MAX_SAFE_JSON_INTEGER {
        return Err(ValidationError::new(ErrorCode::BlobTooLarge));
    }
    Ok(())
}
