//! Local materialization of the shared imported-email contract.
use std::io::Read;
use std::path::Path;

use serde_json::json;
use synck_items::imported_email::{
    IMPORTED_EMAIL_PARSER_VERSION, ImportedEmailFormat, ImportedEmailLimits,
    parse_imported_email_owned,
};
use synck_vcs::types::{EventProposal, ItemId};

use crate::error::{IoContext, LocalError, Result};
use crate::repository::open_regular_file_nofollow;

// Local imports are synchronous and bounded independently of platform upload limits.
pub(crate) const SOURCE_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) struct PreparedEmail {
    pub events: Vec<EventProposal>,
    pub objects: Vec<(String, Vec<u8>)>,
}

pub(crate) fn prepare(
    id: &ItemId,
    link: Option<ItemId>,
    filename: &str,
    title: Option<&str>,
    source: &Path,
) -> Result<PreparedEmail> {
    let format = ImportedEmailFormat::from_name(filename).ok_or_else(|| {
        LocalError::Invalid("EMAIL_IMPORT_INVALID_FORMAT: expected .eml or .msg".into())
    })?;
    if title.is_some_and(|title| title.trim().is_empty()) {
        return Err(LocalError::Invalid("email title must not be empty".into()));
    }
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
        let id = synck_vcs::object::plain_id(&bytes).to_string();
        objects.push((id.clone(), bytes));
        Ok(id)
    };
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
    let mut metadata = json!({"mailcal": {
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
    // An email's title is its subject unless the importer names it.
    if let Some(title) = title {
        metadata["title"] = json!(title);
    }
    let mut events = vec![synck_vcs::types::EventProposal::from_fields(
        json!({"item_id":id,"event_type":"EMAIL::CREATE",
        "create_item":{"item_type":"EMAIL","link":link},"set_metadata":metadata,"set_content":{"source":source_blob}}),
    )?];
    events.extend(attachments);
    Ok(PreparedEmail { events, objects })
}
