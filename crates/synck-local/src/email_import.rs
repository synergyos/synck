//! Local materialization of the shared imported-email contract.
use std::io::Read;
use std::path::Path;

use serde_json::json;
use sha2::{Digest, Sha256};
use synck_items::imported_email::{
    IMPORTED_EMAIL_PARSER_VERSION, ImportedEmailFormat, ImportedEmailLimits,
    imported_email_item_name, parse_imported_email_owned,
};
use synck_vcs::types::{EventProposal, ItemId};

use crate::error::{IoContext, LocalError, Result};
use crate::repository::open_regular_file_nofollow;

// Local imports are synchronous and bounded independently of platform upload limits.
pub(crate) const SOURCE_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) struct PreparedEmail {
    pub source_plain_digest: String,
    pub events: Vec<EventProposal>,
    pub objects: Vec<(String, Vec<u8>)>,
}

pub(crate) fn prepare(
    id: &ItemId,
    parent: Option<ItemId>,
    filename: &str,
    requested_name: &str,
    source: &Path,
) -> Result<PreparedEmail> {
    let format = ImportedEmailFormat::from_name(filename)
        .or_else(|| ImportedEmailFormat::from_name(requested_name))
        .ok_or_else(|| {
            LocalError::Invalid("EMAIL_IMPORT_INVALID_FORMAT: expected .eml or .msg".into())
        })?;
    let name = if ImportedEmailFormat::from_name(requested_name).is_some() {
        imported_email_item_name(requested_name).map_err(LocalError::Invalid)?
    } else {
        synck_vcs::naming::validate_display_name(requested_name)
            .map_err(|error| LocalError::Invalid(error.to_string()))?;
        requested_name.to_string()
    };
    let mut bytes = Vec::new();
    open_regular_file_nofollow(source)?
        .take(SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .at("read email source", source)?;
    if bytes.len() as u64 > SOURCE_BYTES {
        return Err(LocalError::Invalid(format!(
            "EMAIL_IMPORT_LIMIT_EXCEEDED: source exceeds {SOURCE_BYTES} bytes"
        )));
    }
    let mut limits = ImportedEmailLimits::for_source_limit_and_parser_concurrency(SOURCE_BYTES, 1);
    limits.header_bytes = 1024 * 1024;
    limits.header_count = 10_000;
    limits.part_count = 2_000;
    limits.attachment_count = 1_000;
    limits.decoded_attachment_bytes = SOURCE_BYTES;
    limits.normalized_body_bytes = SOURCE_BYTES;
    limits.generated_rfc822_bytes = 2 * SOURCE_BYTES;
    let parsed = parse_imported_email_owned(format, bytes.clone(), &limits)
        .map_err(|error| LocalError::Invalid(error.to_string()))?;
    let mut objects = Vec::new();
    let mut store = |bytes: Vec<u8>| -> Result<String> {
        let object = synck_vcs::object::Object::new(synck_vcs::object::Codec::Raw, &bytes).map_err(|error| LocalError::Invalid(error.to_string()))?;
        let id = object.id().to_string();
        objects.push((id.clone(), object.payload().to_vec()));
        Ok(id)
    };
    let source_plain_digest = hex::encode(Sha256::digest(&bytes));
    let source_size = bytes.len();
    let source_blob = store(bytes)?;
    let mut attachments = Vec::new();
    for attachment in parsed.attachments {
        let size = attachment.bytes.len();
        let blob = store(attachment.bytes)?;
        let mut meta = json!({"filename":attachment.filename,"mime_type":attachment.mime_type,
            "size_bytes":size,"is_inline":attachment.is_inline,"content_id":attachment.content_id,
            "provider_attachment_id":null});
        if attachment.embedded_data_image {
            meta["origin"] = json!({"kind":"html_data_image","version":1});
        }
        attachments.push(synck_vcs::types::EventProposal::from_fields(
            json!({"item_id":id,"event_type":"EMAIL::ATTACHMENT::CREATE",
            "set_metadata":{"meta":meta},"set_content":{"source":blob}}),
        )?);
    }
    let metadata = json!({"mailcal": {
        "v":2,"parser_version":IMPORTED_EMAIL_PARSER_VERSION,"parser_limits":limits,
        "origin":{"kind":"local_import"},
        "source":{"format":format.as_str(),"filename":filename,"mime_type":format.source_mime_type(),
            "size_bytes":source_size},
        "provider":null,"account_id":null,"provider_message_id":null,"provider_thread_id":null,
        "internet_message_id":parsed.internet_message_id,"subject":parsed.subject,
        "from":parsed.from,"sender":parsed.sender,"to":parsed.to,"cc":parsed.cc,"bcc":parsed.bcc,
        "reply_to":parsed.reply_to,"sent_at":parsed.sent_at,"received_at":parsed.received_at,
        "has_attachments":!attachments.is_empty(),"attachment_count":attachments.len(),
        "imported_at":chrono::Utc::now().to_rfc3339()
    }});
    let mut events = vec![synck_vcs::types::EventProposal::from_fields(
        json!({"item_id":id,"event_type":"EMAIL::CREATE",
        "create_item":{"item_type":"EMAIL","parent_folder_id":parent,"name":name},"set_metadata":metadata,"set_content":{"source":source_blob}}),
    )?];
    events.extend(attachments);
    Ok(PreparedEmail { events, objects, source_plain_digest })
}

pub(crate) fn reject_file_email_update(filename: &str) -> Result<()> {
    if ImportedEmailFormat::from_name(filename).is_some() {
        return Err(LocalError::Invalid("EMAIL_IMPORT_REQUIRES_NEW_ITEM: EML/MSG sources must be imported as a new EMAIL item; an existing FILE cannot be renamed to an email source or receive email content".into()));
    }
    Ok(())
}

impl crate::repository::Repository {
    pub(crate) fn store_imported_email_objects(&self, objects: &[(String, Vec<u8>)]) -> Result<()> {
        use crate::repository::{rename_noreplace, sync_directory};
        use std::io::Write;
        for (blob, bytes) in objects {
            if self.storage_mode() == crate::StorageMode::Chunked {
                let digest = self.object_store().store_bytes(bytes)?;
                if *blob != format!("sha256:{digest}") {
                    return Err(LocalError::Integrity("email object digest mismatch".into()));
                }
                continue;
            }
            let id = synck_vcs::types::ObjectId::parse(blob)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            let destination = self.object_path(&id);
            if destination
                .try_exists()
                .at("inspect email object", &destination)?
            {
                let mut existing = open_regular_file_nofollow(&destination)?;
                let mut hasher = Sha256::new();
                let mut buffer = [0; 64 * 1024];
                loop {
                    let count = existing
                        .read(&mut buffer)
                        .at("verify email object", &destination)?;
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                }
                if format!("sha256:{}", hex::encode(hasher.finalize())) != *blob {
                    return Err(LocalError::Integrity(
                        "existing email object is corrupt".into(),
                    ));
                }
                continue;
            }
            let temporary = self
                .metadata_path()
                .join("tmp")
                .join(format!("email-object-{}", uuid::Uuid::new_v4()));
            let mut output = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .at("create email object", &temporary)?;
            output
                .write_all(bytes)
                .at("write email object", &temporary)?;
            output.sync_all().at("sync email object", &temporary)?;
            drop(output);
            rename_noreplace(&temporary, &destination).at("publish email object", &destination)?;
        }
        if !objects.is_empty() {
            sync_directory(&self.metadata_path().join("objects"))?;
            sync_directory(&self.metadata_path().join("tmp"))?;
        }
        Ok(())
    }

    /// An unchanged physical import source is retained for its owner, without
    /// becoming another FILE on every status/commit. Canonical EMAIL metadata
    /// and the exact source digest are the authority; changing the bytes makes
    /// the path an addition again. Existing FILE identities are never filtered.
    /// Return retained ancestor directories so checkout restore preserves them.
    pub(crate) async fn filter_imported_email_sources(
        &self,
        committed: &std::collections::BTreeMap<String, crate::worktree::CommittedNode>,
        working: &mut std::collections::BTreeMap<String, crate::worktree::WorkingNode>,
    ) -> Result<std::collections::BTreeSet<String>> {
        use crate::worktree::{NodeKind, load_identity_records, reconcile_identities};
        let sources = {
            let connection = crate::local_database::open_readonly(self.storage())
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            let mut statement = connection.prepare(
                "SELECT s.path,s.source_blob_id,e.parent_folder_id FROM local_email_import_sources s \
                 JOIN vcs_items i ON i.repo_id=s.repo_id AND i.item_id=s.item_id \
                 JOIN vcs_events e ON e.repo_id=i.repo_id AND e.item_id=i.item_id AND e.item_seq=1 AND e.event_type='EMAIL::CREATE' \
                 WHERE s.repo_id=?1 AND i.item_type='EMAIL' AND i.lifecycle_state='LIVE'",
            )?;
            statement
                .query_map([self.repository_id().as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if sources.is_empty() {
            return Ok(std::collections::BTreeSet::new());
        }
        // Sparse transitions still observe source containers outside the new view.
        // Their canonical folder identities must remain available for recognition.
        let complete;
        let source_tree = if self.checkout_paths().is_some_and(|paths| !paths.is_empty()) {
            complete = self.complete_file_tree().await?;
            &complete
        } else {
            committed
        };
        let records = load_identity_records(self.storage(), self.repository_id().as_str())?;
        let matched = reconcile_identities(source_tree, working, &records);
        let tracked_paths = matched.values().collect::<std::collections::HashSet<_>>();
        let folder_ids = source_tree
            .values()
            .filter(|node| node.kind == NodeKind::Folder)
            .map(|node| &node.item_id)
            .collect::<std::collections::HashSet<_>>();
        let tracked_directories = committed
            .values()
            .filter(|node| node.kind == NodeKind::Folder)
            .filter_map(|node| matched.get(&node.item_id))
            .collect::<std::collections::HashSet<_>>();
        let mut retained = Vec::new();
        for (original_path, blob, parent) in sources {
            let parent = parent.map(ItemId::parse).transpose().map_err(|error| {
                LocalError::Integrity(format!("invalid email source parent: {error}"))
            })?;
            let filename = original_path
                .rsplit('/')
                .next()
                .expect("nonempty source path");
            let path = match parent {
                None => filename.to_string(),
                Some(parent) if folder_ids.contains(&parent) => {
                    let Some(folder) = matched.get(&parent) else {
                        continue;
                    };
                    format!("{folder}/{filename}")
                }
                Some(_) => continue,
            };
            // Bind the observation to its original parent identity and filename.
            // An ancestor rename carries the source; an unrelated copy does not.
            // Protect current tracked ownership, not a vacated canonical path.
            if tracked_paths.contains(&path) {
                continue;
            }
            if let Some(node) = working.get(&path) {
                if node.kind == NodeKind::File {
                    let id = synck_vcs::types::ObjectId::parse(blob).map_err(|e| LocalError::Integrity(e.to_string()))?;
                    let expected = self.plain_digest(&id)?;
                    if node.digest_hex.as_deref() == Some(expected.as_str()) { retained.push(path); }
                }
            }
        }
        let mut directories = std::collections::BTreeSet::new();
        for path in retained {
            working.remove(&path);
            let mut parent = path.as_str();
            while let Some((directory, _)) = parent.rsplit_once('/') {
                directories.insert(directory.to_string());
                parent = directory;
            }
        }
        // A narrowed checkout leaves the originals and their directories in
        // place. Only empty observational containers can disappear from the
        // FILE/FOLDER view; other children still need their parents for restore.
        for path in directories.iter().rev() {
            if !tracked_directories.contains(path)
                && working
                    .get(path)
                    .is_some_and(|node| node.kind == crate::worktree::NodeKind::Folder)
                && !working
                    .range(format!("{path}/")..)
                    .next()
                    .is_some_and(|(child, _)| {
                        child
                            .strip_prefix(path)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                    })
            {
                working.remove(path);
            }
        }
        Ok(directories)
    }

 }

/// Rebuild observations from accepted source identity, never from a scan that
/// could accidentally bless changed working bytes as already imported.
pub(crate) fn project_source_observation(
    transaction: &rusqlite::Transaction<'_>,
    event: &synck_vcs::types::EventRow,
) -> Result<()> {
    let proposal = event.proposal()?;
    let Some(metadata) = proposal.set_metadata.as_ref().and_then(synck_vcs::metadata::MetadataRef::value) else {
        // Erased metadata cannot authorize hiding a working file.
        return Ok(());
    };
    if metadata.pointer("/mailcal/origin/kind").and_then(serde_json::Value::as_str) != Some("local_import") {
        return Ok(());
    }
    let filename = metadata.pointer("/mailcal/source/filename").and_then(serde_json::Value::as_str)
        .ok_or_else(|| LocalError::Integrity("local EMAIL source filename is missing".into()))?;
    if synck_vcs::naming::validate_display_name(filename).is_err() {
        // External source filenames are original metadata, not checkout paths.
        return Ok(());
    }
    let source = proposal.set_content.as_ref().and_then(|roles| roles.get("source"))
        .ok_or_else(|| LocalError::Integrity("local EMAIL source role is missing".into()))?;
    transaction.execute("INSERT INTO local_email_import_sources(repo_id,path,item_id,source_blob_id) VALUES(?1,?2,?3,?4) ON CONFLICT(repo_id,path,item_id) DO UPDATE SET source_blob_id=excluded.source_blob_id",
        rusqlite::params![event.repo_id.as_str(),filename,event.item_id.as_str(),source.as_str()])?;
    Ok(())
}
