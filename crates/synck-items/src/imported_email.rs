//! Bounded, storage-free EML/MSG decoding shared by local and platform imports.
use base64::Engine as _;
use mailparse::{DispositionType, MailAddr, MailHeader, MailHeaderMap, ParsedMail};
use msg_parser::{Outlook, Person};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read, Write};
use std::path::Path;

mod msg_rtf;

pub const IMPORTED_EMAIL_PARSER_VERSION: u32 = 12;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportedEmailLimits {
    pub source_bytes: u64,
    pub header_bytes: u64,
    pub header_count: u64,
    pub mime_nesting: u64,
    pub part_count: u64,
    pub attachment_count: u64,
    pub decoded_attachment_bytes: u64,
    pub normalized_body_bytes: u64,
    pub generated_rfc822_bytes: u64,
    pub parser_concurrency: u64,
    pub wall_clock_seconds: u64,
}

impl ImportedEmailLimits {
    pub fn for_source_limit_and_parser_concurrency(
        source_bytes: u64,
        parser_concurrency: usize,
    ) -> Self {
        Self {
            source_bytes,
            header_bytes: 16 * 1024 * 1024,
            header_count: 100_000,
            mime_nesting: 64,
            part_count: 20_000,
            attachment_count: 10_000,
            decoded_attachment_bytes: 2 * 1024 * 1024 * 1024,
            normalized_body_bytes: 256 * 1024 * 1024,
            generated_rfc822_bytes: 4 * 1024 * 1024 * 1024,
            parser_concurrency: parser_concurrency.max(1) as u64,
            wall_clock_seconds: 120,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportedEmailFormat {
    Eml,
    Msg,
}

impl ImportedEmailFormat {
    pub fn from_name(name: &str) -> Option<Self> {
        let extension = name.rsplit_once('.')?.1;
        if extension.eq_ignore_ascii_case("eml") {
            Some(Self::Eml)
        } else if extension.eq_ignore_ascii_case("msg") {
            Some(Self::Msg)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eml => "eml",
            Self::Msg => "msg",
        }
    }

    pub fn source_mime_type(self) -> &'static str {
        match self {
            Self::Eml => "message/rfc822",
            Self::Msg => "application/vnd.ms-outlook",
        }
    }
}

pub fn imported_email_item_name(source_filename: &str) -> Result<String, String> {
    if ImportedEmailFormat::from_name(source_filename).is_none() {
        return Err("imported email source filename is not an EML/MSG name".to_string());
    }
    let Some((item_name, _extension)) = source_filename.rsplit_once('.') else {
        return Err("imported email source filename has no extension".to_string());
    };
    let item_name = item_name.trim_end_matches(['.', ' ']);
    synck_vcs::naming::validate_display_name(item_name).map_err(|error| {
        format!(
            "normalizing the email-file name `{source_filename}` leaves an invalid item name: {error}"
        )
    })?;
    Ok(item_name.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportedEmailAddress {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedEmailAttachment {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub is_inline: bool,
    pub content_id: Option<String>,
    pub embedded_data_image: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedImportedEmail {
    pub format: ImportedEmailFormat,
    pub internet_message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<ImportedEmailAddress>,
    pub sender: Option<ImportedEmailAddress>,
    pub to: Vec<ImportedEmailAddress>,
    pub cc: Vec<ImportedEmailAddress>,
    pub bcc: Vec<ImportedEmailAddress>,
    pub reply_to: Vec<ImportedEmailAddress>,
    pub sent_at: Option<String>,
    pub received_at: Option<String>,
    pub body_markdown: Vec<u8>,
    pub body_html: Option<Vec<u8>>,
    pub attachments: Vec<ImportedEmailAttachment>,
    pub raw_rfc822: Vec<u8>,
    pub raw_rfc822_generated: bool,
}

fn strip_parser_text_nuls(parsed: &mut ParsedImportedEmail) {
    fn strip(value: &mut String) {
        value.retain(|character| character != '\0');
    }

    fn strip_address(address: &mut ImportedEmailAddress) {
        if let Some(name) = address.name.as_mut() {
            strip(name);
        }
        if let Some(email) = address.email.as_mut() {
            strip(email);
        }
    }

    for value in [
        &mut parsed.internet_message_id,
        &mut parsed.subject,
        &mut parsed.sent_at,
        &mut parsed.received_at,
    ] {
        if let Some(value) = value.as_mut() {
            strip(value);
        }
    }
    for address in parsed
        .from
        .iter_mut()
        .chain(parsed.sender.iter_mut())
        .chain(parsed.to.iter_mut())
        .chain(parsed.cc.iter_mut())
        .chain(parsed.bcc.iter_mut())
        .chain(parsed.reply_to.iter_mut())
    {
        strip_address(address);
    }
    parsed.body_markdown.retain(|byte| *byte != b'\0');
    if let Some(body_html) = parsed.body_html.as_mut() {
        body_html.retain(|byte| *byte != b'\0');
    }
    for attachment in &mut parsed.attachments {
        strip(&mut attachment.filename);
        strip(&mut attachment.mime_type);
        if let Some(content_id) = attachment.content_id.as_mut() {
            strip(content_id);
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImportedEmailParseError {
    #[error("EMAIL_IMPORT_INVALID_FORMAT: {0}")]
    InvalidFormat(String),
    #[error("EMAIL_IMPORT_PARSE_FAILED: {0}")]
    ParseFailed(String),
    #[error("EMAIL_IMPORT_ATTACHMENT_FAILED: {0}")]
    AttachmentFailed(String),
    #[error("EMAIL_IMPORT_LIMIT_EXCEEDED: {0}")]
    LimitExceeded(String),
    #[error("EMAIL_IMPORT_RFC822_GENERATION_FAILED: {0}")]
    Rfc822GenerationFailed(String),
}

impl ImportedEmailParseError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidFormat(_) => "EMAIL_IMPORT_INVALID_FORMAT",
            Self::ParseFailed(_) => "EMAIL_IMPORT_PARSE_FAILED",
            Self::AttachmentFailed(_) => "EMAIL_IMPORT_ATTACHMENT_FAILED",
            Self::LimitExceeded(_) => "EMAIL_IMPORT_LIMIT_EXCEEDED",
            Self::Rfc822GenerationFailed(_) => "EMAIL_IMPORT_RFC822_GENERATION_FAILED",
        }
    }
}

#[cfg(test)]
pub fn parse_imported_email(
    format: ImportedEmailFormat,
    source: &[u8],
    limits: &ImportedEmailLimits,
) -> Result<ParsedImportedEmail, ImportedEmailParseError> {
    parse_imported_email_owned(format, source.to_vec(), limits)
}

pub fn parse_imported_email_owned(
    format: ImportedEmailFormat,
    source: Vec<u8>,
    limits: &ImportedEmailLimits,
) -> Result<ParsedImportedEmail, ImportedEmailParseError> {
    if source.is_empty() {
        return Err(ImportedEmailParseError::InvalidFormat(
            "source is empty".to_string(),
        ));
    }
    require_limit("source bytes", source.len() as u64, limits.source_bytes)?;
    match format {
        ImportedEmailFormat::Eml => parse_eml(source.as_slice(), limits),
        ImportedEmailFormat::Msg => parse_msg(source, limits),
    }
}

fn require_limit(label: &str, actual: u64, maximum: u64) -> Result<(), ImportedEmailParseError> {
    if actual > maximum {
        Err(ImportedEmailParseError::LimitExceeded(format!(
            "{label} {actual} exceeds maximum {maximum}"
        )))
    } else {
        Ok(())
    }
}

fn validate_top_level_eml_headers(
    source: &[u8],
    limits: &ImportedEmailLimits,
) -> Result<(), ImportedEmailParseError> {
    let header_end = source
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| source.windows(2).position(|window| window == b"\n\n"))
        .unwrap_or(source.len());
    require_limit(
        "top-level header bytes",
        header_end as u64,
        limits.header_bytes,
    )?;
    let header_count = source[..header_end]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty() && !matches!(line.first(), Some(b' ' | b'\t' | b'\r')))
        .count() as u64;
    require_limit("top-level header count", header_count, limits.header_count)
}

// mailparse recursively builds the MIME tree before exposing its nodes. Enforce
// depth/part/header limits iteratively first, using the same boundary semantics,
// so a deeply nested input cannot overflow its parser stack before our checks.
fn preflight_eml_structure(
    source: &[u8],
    limits: &ImportedEmailLimits,
) -> Result<(), ImportedEmailParseError> {
    fn boundary_at(data: &[u8], start: usize, boundary: &[u8]) -> Option<usize> {
        let mut offset = start;
        for line in data.get(start..)?.split_inclusive(|byte| *byte == b'\n') {
            if line.starts_with(boundary) {
                return Some(offset);
            }
            offset += line.len();
        }
        None
    }
    let mut pending = vec![(source, 0u64)];
    let (mut parts, mut headers_total, mut header_bytes) = (0u64, 0u64, 0u64);
    while let Some((part, depth)) = pending.pop() {
        require_limit("MIME nesting", depth, limits.mime_nesting)?;
        parts = parts.saturating_add(1);
        require_limit("MIME part count", parts, limits.part_count)?;
        validate_top_level_eml_headers(part, limits)?;
        let (headers, body) = mailparse::parse_headers(part)
            .map_err(|error| ImportedEmailParseError::InvalidFormat(error.to_string()))?;
        headers_total = headers_total.saturating_add(headers.len() as u64);
        header_bytes = header_bytes.saturating_add(body as u64);
        require_limit("header count", headers_total, limits.header_count)?;
        require_limit("total header bytes", header_bytes, limits.header_bytes)?;
        let Some(content_type) = headers.get_first_value("Content-Type") else {
            continue;
        };
        let content_type = mailparse::parse_content_type(&content_type);
        if !content_type.mimetype.starts_with("multipart/") {
            continue;
        }
        let Some(boundary) = content_type.params.get("boundary") else {
            continue;
        };
        require_limit("MIME boundary bytes", boundary.len() as u64, 1024)?;
        let boundary = format!("--{boundary}");
        let Some(first) = boundary_at(part, body, boundary.as_bytes()) else {
            continue;
        };
        let mut boundary_end = first + boundary.len();
        while let Some(start) = part
            .get(boundary_end..)
            .and_then(|tail| tail.iter().position(|byte| *byte == b'\n'))
            .map(|offset| boundary_end + offset + 1)
        {
            let end = boundary_at(part, start, boundary.as_bytes()).unwrap_or(part.len());
            require_limit(
                "MIME part count",
                parts.saturating_add(pending.len() as u64).saturating_add(1),
                limits.part_count,
            )?;
            pending.push((&part[start..end], depth.saturating_add(1)));
            boundary_end = end + boundary.len();
            if boundary_end + 2 > part.len()
                || part.get(boundary_end..boundary_end + 2) == Some(b"--")
            {
                break;
            }
        }
    }
    Ok(())
}

fn parse_eml(
    source: &[u8],
    limits: &ImportedEmailLimits,
) -> Result<ParsedImportedEmail, ImportedEmailParseError> {
    preflight_eml_structure(source, limits)?;
    let parsed = mailparse::parse_mail(source)
        .map_err(|e| ImportedEmailParseError::InvalidFormat(e.to_string()))?;
    if parsed.headers.is_empty()
        || !parsed.headers.iter().any(|header| {
            matches!(
                header.get_key_ref().to_ascii_lowercase().as_str(),
                "from"
                    | "sender"
                    | "to"
                    | "cc"
                    | "bcc"
                    | "reply-to"
                    | "subject"
                    | "date"
                    | "message-id"
                    | "mime-version"
                    | "content-type"
                    | "received"
            )
        })
    {
        return Err(ImportedEmailParseError::InvalidFormat(
            "RFC822 message has no recognized message headers".to_string(),
        ));
    }

    let mut plain_body = None;
    let mut html_body = None;
    let mut leaves = Vec::new();
    let mut budget = EmlTraversalBudget::new(limits);
    collect_eml_leaves(
        &parsed,
        "0",
        0,
        &mut plain_body,
        &mut html_body,
        &mut leaves,
        &mut budget,
    )?;
    let normalized_html = html_body
        .as_deref()
        .map(|html| {
            super::email_data_images::extract_data_images(
                html,
                leaves.iter().filter_map(|leaf| leaf.content_id.clone()),
            )
            .map_err(|error| ImportedEmailParseError::ParseFailed(error.to_string()))
        })
        .transpose()?;
    let html_body = normalized_html
        .as_ref()
        .map(|normalized| normalized.html.clone());
    if let Some(normalized) = normalized_html.as_ref() {
        for image in &normalized.images {
            budget.add_attachment(image.bytes.len())?;
        }
    }
    let body_markdown = plain_body
        .or_else(|| html_body.as_deref().map(html_to_text))
        .unwrap_or_default()
        .into_bytes();
    require_limit(
        "normalized body bytes",
        body_markdown.len() as u64,
        limits.normalized_body_bytes,
    )?;

    let html_for_cid = html_body
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut attachments: Vec<_> = leaves
        .into_iter()
        .map(|leaf| {
            let cid_ref = leaf
                .content_id
                .as_deref()
                .map(|cid| format!("cid:{}", cid.to_ascii_lowercase()))
                .map(|needle| html_for_cid.contains(needle.as_str()))
                .unwrap_or(false);
            ImportedEmailAttachment {
                filename: leaf.filename,
                mime_type: leaf.mime_type,
                bytes: leaf.bytes,
                is_inline: leaf.explicit_inline || cid_ref,
                content_id: leaf.content_id,
                embedded_data_image: false,
            }
        })
        .collect();
    if let Some(normalized) = normalized_html {
        attachments.extend(
            normalized
                .images
                .into_iter()
                .map(|image| ImportedEmailAttachment {
                    filename: image.filename,
                    mime_type: image.mime_type,
                    bytes: image.bytes,
                    is_inline: true,
                    content_id: Some(image.content_id),
                    embedded_data_image: true,
                }),
        );
    }

    let mut parsed = ParsedImportedEmail {
        format: ImportedEmailFormat::Eml,
        internet_message_id: parsed
            .headers
            .get_first_value("Message-ID")
            .as_deref()
            .and_then(normalize_message_id),
        subject: clean_one_line(parsed.headers.get_first_value("Subject").as_deref()),
        from: addresses_from_header(&parsed.headers, "From")
            .into_iter()
            .next(),
        sender: distinct_sender(
            addresses_from_header(&parsed.headers, "From")
                .into_iter()
                .next(),
            addresses_from_header(&parsed.headers, "Sender")
                .into_iter()
                .next(),
        ),
        to: addresses_from_header(&parsed.headers, "To"),
        cc: addresses_from_header(&parsed.headers, "Cc"),
        bcc: addresses_from_header(&parsed.headers, "Bcc"),
        reply_to: addresses_from_header(&parsed.headers, "Reply-To"),
        sent_at: parsed
            .headers
            .get_first_value("Date")
            .as_deref()
            .and_then(parse_mail_date),
        received_at: newest_received_date(&parsed.headers),
        body_markdown,
        body_html: html_body.map(String::into_bytes),
        attachments,
        raw_rfc822: source.to_vec(),
        raw_rfc822_generated: false,
    };
    strip_parser_text_nuls(&mut parsed);
    Ok(parsed)
}

struct EmlLeaf {
    filename: String,
    mime_type: String,
    bytes: Vec<u8>,
    explicit_inline: bool,
    content_id: Option<String>,
}

struct EmlTraversalBudget<'a> {
    limits: &'a ImportedEmailLimits,
    parts: u64,
    headers: u64,
    header_bytes: u64,
    attachments: u64,
    attachment_bytes: u64,
}

impl<'a> EmlTraversalBudget<'a> {
    fn new(limits: &'a ImportedEmailLimits) -> Self {
        Self {
            limits,
            parts: 0,
            headers: 0,
            header_bytes: 0,
            attachments: 0,
            attachment_bytes: 0,
        }
    }

    fn enter_part(
        &mut self,
        part: &ParsedMail<'_>,
        depth: u64,
    ) -> Result<(), ImportedEmailParseError> {
        require_limit("MIME nesting", depth, self.limits.mime_nesting)?;
        self.parts = self.parts.saturating_add(1);
        require_limit("MIME part count", self.parts, self.limits.part_count)?;
        self.headers = self.headers.saturating_add(part.headers.len() as u64);
        require_limit("header count", self.headers, self.limits.header_count)?;
        let part_header_bytes = part
            .headers
            .iter()
            .map(|header| {
                header
                    .get_key_ref()
                    .len()
                    .saturating_add(header.get_value().len())
                    .saturating_add(2) as u64
            })
            .sum::<u64>();
        self.header_bytes = self.header_bytes.saturating_add(part_header_bytes);
        require_limit(
            "total header bytes",
            self.header_bytes,
            self.limits.header_bytes,
        )
    }

    fn add_attachment(&mut self, bytes: usize) -> Result<(), ImportedEmailParseError> {
        self.attachments = self.attachments.saturating_add(1);
        require_limit(
            "attachment count",
            self.attachments,
            self.limits.attachment_count,
        )?;
        self.attachment_bytes = self.attachment_bytes.saturating_add(bytes as u64);
        require_limit(
            "decoded attachment bytes",
            self.attachment_bytes,
            self.limits.decoded_attachment_bytes,
        )
    }
}

fn collect_eml_leaves(
    part: &ParsedMail<'_>,
    path: &str,
    depth: u64,
    plain_body: &mut Option<String>,
    html_body: &mut Option<String>,
    attachments: &mut Vec<EmlLeaf>,
    budget: &mut EmlTraversalBudget<'_>,
) -> Result<(), ImportedEmailParseError> {
    budget.enter_part(part, depth)?;
    if !part.subparts.is_empty() {
        for (index, child) in part.subparts.iter().enumerate() {
            collect_eml_leaves(
                child,
                format!("{path}.{index}").as_str(),
                depth.saturating_add(1),
                plain_body,
                html_body,
                attachments,
                budget,
            )?;
        }
        return Ok(());
    }

    let disposition = part.get_content_disposition();
    let filename = disposition
        .params
        .get("filename")
        .or_else(|| part.ctype.params.get("name"))
        .map(String::as_str)
        .and_then(|value| clean_one_line(Some(value)));
    let mime_type = nonempty(part.ctype.mimetype.as_str())
        .unwrap_or_else(|| "application/octet-stream".to_string())
        .to_ascii_lowercase();
    let is_attachment = matches!(disposition.disposition, DispositionType::Attachment)
        || filename.is_some()
        || (mime_type != "text/plain" && mime_type != "text/html");

    if !is_attachment && mime_type == "text/plain" {
        if plain_body.is_none() {
            let body = part
                .get_body()
                .map_err(|e| ImportedEmailParseError::ParseFailed(e.to_string()))?;
            require_limit(
                "decoded plain body bytes",
                body.len() as u64,
                budget.limits.normalized_body_bytes,
            )?;
            *plain_body = Some(body);
        }
        return Ok(());
    }
    if !is_attachment && mime_type == "text/html" {
        if html_body.is_none() {
            let body = part
                .get_body()
                .map_err(|e| ImportedEmailParseError::ParseFailed(e.to_string()))?;
            require_limit(
                "decoded HTML body bytes",
                body.len() as u64,
                budget.limits.normalized_body_bytes,
            )?;
            *html_body = Some(body);
        }
        return Ok(());
    }

    let bytes = part
        .get_body_raw()
        .map_err(|e| ImportedEmailParseError::AttachmentFailed(e.to_string()))?;
    budget.add_attachment(bytes.len())?;
    let content_id = part
        .headers
        .get_first_value("Content-ID")
        .as_deref()
        .and_then(normalize_content_id);
    attachments.push(EmlLeaf {
        filename: filename.unwrap_or_else(|| fallback_attachment_name(path, mime_type.as_str())),
        mime_type,
        bytes,
        explicit_inline: part
            .headers
            .get_first_header("Content-Disposition")
            .is_some()
            && matches!(disposition.disposition, DispositionType::Inline),
        content_id,
    });
    Ok(())
}

fn parse_msg(
    source: Vec<u8>,
    limits: &ImportedEmailLimits,
) -> Result<ParsedImportedEmail, ImportedEmailParseError> {
    let source_digest: [u8; 32] = Sha256::digest(source.as_slice()).into();
    let message = match cfb::CompoundFile::open(Cursor::new(source.as_slice())) {
        Ok(compound) => {
            for (index, entry) in compound.walk().enumerate() {
                require_limit(
                    "MSG compound entries",
                    index as u64,
                    limits.part_count.saturating_mul(64),
                )?;
                require_limit(
                    "MSG storage nesting",
                    entry.path().components().count() as u64,
                    limits.mime_nesting.saturating_add(2),
                )?;
            }
            drop(compound);
            let normalized_source = normalize_msg_strings(source)?;
            outlook_from_slice(normalized_source.as_slice())
                .map_err(ImportedEmailParseError::InvalidFormat)?
        }
        Err(cfb_error) if duplicate_minifat_reference(&cfb_error) => outlook_from_slice(
            source.as_slice(),
        )
        .map_err(|parse_error| {
            ImportedEmailParseError::InvalidFormat(format!(
                "open MSG compound file failed: {cfb_error}; direct MSG parse failed: {parse_error}"
            ))
        })?,
        Err(error) => {
            return Err(ImportedEmailParseError::InvalidFormat(format!(
                "open MSG compound file failed: {error}"
            )));
        }
    };
    require_limit(
        "MSG transport header bytes",
        message.headers.raw.len() as u64,
        limits.header_bytes,
    )?;
    let transport_header_count = message
        .headers
        .raw
        .lines()
        .filter(|line| !line.starts_with(' ') && !line.starts_with('\t') && !line.trim().is_empty())
        .count() as u64;
    require_limit(
        "MSG transport header count",
        transport_header_count,
        limits.header_count,
    )?;
    require_limit(
        "MSG part count",
        1u64.saturating_add(message.attachments.len() as u64),
        limits.part_count,
    )?;
    require_limit(
        "attachment count",
        message.attachments.len() as u64,
        limits.attachment_count,
    )?;
    let attachment_bytes = message
        .attachments
        .iter()
        .map(|attachment| attachment.payload_bytes.len() as u64)
        .fold(0u64, u64::saturating_add);
    require_limit(
        "decoded attachment bytes",
        attachment_bytes,
        limits.decoded_attachment_bytes,
    )?;
    let transport = if message.headers.raw.trim().is_empty() {
        None
    } else {
        mailparse::parse_headers(message.headers.raw.as_bytes())
            .ok()
            .map(|(headers, _)| headers)
    };
    let transport_from = transport
        .as_deref()
        .map(|headers| addresses_from_header(headers, "From"))
        .unwrap_or_default()
        .into_iter()
        .next();
    let sender = address_from_person(&message.sender);
    let visible_from = transport_from.or_else(|| sender.clone());
    let actual_sender = distinct_sender(visible_from.clone(), sender);
    let reply_to = transport
        .as_deref()
        .map(|headers| addresses_from_header(headers, "Reply-To"))
        .unwrap_or_else(|| addresses_from_text(message.headers.reply_to.as_str()));
    let internet_message_id = nonempty(message.headers.message_id.as_str())
        .as_deref()
        .and_then(normalize_message_id);

    let plain_body = normalize_msg_plain_body(message.body.as_str());
    let direct_html =
        decode_msg_html_property(message.html.as_str(), limits.normalized_body_bytes)?;
    let rtf_html = if direct_html.is_none() {
        if let Some(declared_bytes) =
            rtf_declared_uncompressed_bytes(message.rtf_compressed.as_str())
        {
            require_limit(
                "decompressed RTF bytes",
                declared_bytes,
                limits.normalized_body_bytes,
            )?;
        }
        let decompressed_rtf = message.rtf_decompressed();
        if let Some(rtf) = decompressed_rtf.as_deref() {
            require_limit(
                "decompressed RTF bytes",
                rtf.len() as u64,
                limits.normalized_body_bytes,
            )?;
        }
        let html = decompressed_rtf
            .as_deref()
            .and_then(msg_rtf::extract_encapsulated_html);
        if let Some(html) = html.as_deref() {
            require_limit(
                "RTF-derived HTML body bytes",
                html.len() as u64,
                limits.normalized_body_bytes,
            )?;
        }
        html
    } else {
        None
    };
    let preview_html = direct_html.or(rtf_html);
    let normalized_html = preview_html
        .as_deref()
        .map(|html| {
            super::email_data_images::extract_data_images(
                html,
                message
                    .attachments
                    .iter()
                    .filter_map(|attachment| normalize_content_id(attachment.content_id.as_str())),
            )
            .map_err(|error| ImportedEmailParseError::ParseFailed(error.to_string()))
        })
        .transpose()?;
    let preview_html = normalized_html
        .as_ref()
        .map(|normalized| normalized.html.clone());
    let embedded_image_count = normalized_html
        .as_ref()
        .map_or(0, |normalized| normalized.images.len() as u64);
    let embedded_image_bytes = normalized_html.as_ref().map_or(0, |normalized| {
        normalized
            .images
            .iter()
            .map(|image| image.bytes.len() as u64)
            .fold(0u64, u64::saturating_add)
    });
    require_limit(
        "attachment count",
        (message.attachments.len() as u64).saturating_add(embedded_image_count),
        limits.attachment_count,
    )?;
    require_limit(
        "decoded attachment bytes",
        attachment_bytes.saturating_add(embedded_image_bytes),
        limits.decoded_attachment_bytes,
    )?;
    let html_for_cid = preview_html
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let body = plain_body
        .or_else(|| preview_html.as_deref().map(html_to_text))
        .unwrap_or_default();
    require_limit(
        "normalized body bytes",
        body.len() as u64,
        limits.normalized_body_bytes,
    )?;
    let mut attachments: Vec<ImportedEmailAttachment> = message
        .attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            let content_id = normalize_content_id(attachment.content_id.as_str());
            let referenced_inline = content_id
                .as_deref()
                .map(|cid| {
                    html_for_cid.contains(format!("cid:{}", cid.to_ascii_lowercase()).as_str())
                })
                .unwrap_or(false);
            ImportedEmailAttachment {
                filename: msg_attachment_filename(attachment, index),
                mime_type: normalize_mime_type(attachment.mime_tag.as_str()).unwrap_or_else(|| {
                    if attachment.is_embedded_message() {
                        "application/vnd.ms-outlook".to_string()
                    } else {
                        "application/octet-stream".to_string()
                    }
                }),
                bytes: attachment.payload_bytes.clone(),
                is_inline: referenced_inline,
                content_id,
                embedded_data_image: false,
            }
        })
        .collect();
    if let Some(normalized) = normalized_html {
        attachments.extend(
            normalized
                .images
                .into_iter()
                .map(|image| ImportedEmailAttachment {
                    filename: image.filename,
                    mime_type: image.mime_type,
                    bytes: image.bytes,
                    is_inline: true,
                    content_id: Some(image.content_id),
                    embedded_data_image: true,
                }),
        );
    }

    let mut parsed = ParsedImportedEmail {
        format: ImportedEmailFormat::Msg,
        internet_message_id,
        subject: clean_one_line(Some(message.subject.as_str())),
        from: visible_from,
        sender: actual_sender,
        to: addresses_from_people(&message.to),
        cc: addresses_from_people(&message.cc),
        bcc: addresses_from_people(&message.bcc),
        reply_to,
        sent_at: parse_iso_date(message.client_submit_time.as_str()),
        received_at: parse_iso_date(message.message_delivery_time.as_str()),
        body_markdown: body.into_bytes(),
        body_html: preview_html.map(String::into_bytes),
        attachments,
        raw_rfc822: Vec::new(),
        raw_rfc822_generated: true,
    };
    strip_parser_text_nuls(&mut parsed);
    parsed.raw_rfc822 = generate_rfc822(&parsed, &source_digest, limits.generated_rfc822_bytes)?;
    require_limit(
        "generated RFC822 bytes",
        parsed.raw_rfc822.len() as u64,
        limits.generated_rfc822_bytes,
    )?;
    validate_generated_rfc822(&parsed, limits)?;
    Ok(parsed)
}

fn decode_msg_html_property(
    raw: &str,
    maximum_bytes: u64,
) -> Result<Option<String>, ImportedEmailParseError> {
    let raw = raw.trim_end_matches('\0');
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if raw.trim().is_empty() {
        return Ok(None);
    }
    if looks_like_html(raw) {
        require_limit("decoded HTML body bytes", raw.len() as u64, maximum_bytes)?;
        return Ok(Some(raw.to_string()));
    }
    if !raw.len().is_multiple_of(2) || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(None);
    }
    require_limit(
        "hex-encoded HTML property bytes",
        raw.len() as u64,
        maximum_bytes.saturating_mul(2),
    )?;
    let Some(bytes) = decode_ascii_hex(raw) else {
        return Ok(None);
    };
    let Some(html) = decode_msg_html_bytes(&bytes) else {
        return Ok(None);
    };
    if !looks_like_html(html.as_str()) {
        return Ok(None);
    }
    require_limit("decoded HTML body bytes", html.len() as u64, maximum_bytes)?;
    Ok(Some(html))
}

fn normalize_msg_strings(source: Vec<u8>) -> Result<Vec<u8>, ImportedEmailParseError> {
    let mut compound = cfb::CompoundFile::open(Cursor::new(source)).map_err(|error| {
        ImportedEmailParseError::InvalidFormat(format!("open MSG compound file failed: {error}"))
    })?;
    let string_paths = compound
        .walk()
        .filter(|entry| {
            entry.is_stream()
                && entry.name().starts_with("__substg1.0_")
                && (entry.name().ends_with("001E") || entry.name().ends_with("001F"))
        })
        .map(|entry| entry.path().to_path_buf())
        .collect::<Vec<_>>();

    for path in string_paths {
        let mut bytes = Vec::new();
        compound
            .open_stream(path.as_path())
            .and_then(|mut stream| stream.read_to_end(&mut bytes))
            .map_err(|error| {
                ImportedEmailParseError::ParseFailed(format!(
                    "read MSG string property {} failed: {error}",
                    path.display()
                ))
            })?;
        let normalized = if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("001E"))
        {
            let codepage = msg_codepage_for_path(&mut compound, path.as_path())?;
            decode_msg_string8_property(bytes.as_slice(), codepage)
                .ok_or_else(|| {
                    ImportedEmailParseError::ParseFailed(format!(
                        "unsupported MSG String8 codepage {} for property {}",
                        codepage.unwrap_or(1252),
                        path.display()
                    ))
                })?
                .into_bytes()
        } else {
            let end = bytes
                .chunks_exact(2)
                .position(|unit| unit == [0, 0])
                .map_or(bytes.len(), |index| index * 2);
            bytes[..end].to_vec()
        };
        if normalized == bytes {
            continue;
        }
        let mut stream = compound.open_stream(path.as_path()).map_err(|error| {
            ImportedEmailParseError::ParseFailed(format!(
                "open MSG string property {} for normalization failed: {error}",
                path.display()
            ))
        })?;
        stream.set_len(0).map_err(|error| {
            ImportedEmailParseError::ParseFailed(format!(
                "truncate MSG string property {} failed: {error}",
                path.display()
            ))
        })?;
        stream.write_all(normalized.as_slice()).map_err(|error| {
            ImportedEmailParseError::ParseFailed(format!(
                "write normalized MSG string property {} failed: {error}",
                path.display()
            ))
        })?;
    }

    if compound.version() == cfb::Version::V3 {
        return Ok(compound.into_inner().into_inner());
    }

    let entries = compound
        .walk()
        .skip(1)
        .map(|entry| {
            (
                entry.path().to_path_buf(),
                entry.is_storage(),
                entry.is_stream(),
            )
        })
        .collect::<Vec<_>>();
    let mut normalized =
        cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new())).map_err(
            |error| {
                ImportedEmailParseError::ParseFailed(format!(
                    "create MSG CFB v3 parser view failed: {error}"
                ))
            },
        )?;
    for (path, is_storage, is_stream) in entries {
        if is_storage {
            normalized
                .create_storage_all(path.as_path())
                .map_err(|error| {
                    ImportedEmailParseError::ParseFailed(format!(
                        "copy MSG storage {} to CFB v3 parser view failed: {error}",
                        path.display()
                    ))
                })?;
            continue;
        }
        if !is_stream {
            continue;
        }
        let mut bytes = Vec::new();
        compound
            .open_stream(path.as_path())
            .and_then(|mut stream| stream.read_to_end(&mut bytes))
            .map_err(|error| {
                ImportedEmailParseError::ParseFailed(format!(
                    "read MSG stream {} for CFB v3 parser view failed: {error}",
                    path.display()
                ))
            })?;
        if let Some(parent) = path.parent() {
            normalized.create_storage_all(parent).map_err(|error| {
                ImportedEmailParseError::ParseFailed(format!(
                    "copy MSG stream parent {} to CFB v3 parser view failed: {error}",
                    parent.display()
                ))
            })?;
        }
        normalized
            .create_stream(path.as_path())
            .and_then(|mut stream| stream.write_all(bytes.as_slice()))
            .map_err(|error| {
                ImportedEmailParseError::ParseFailed(format!(
                    "copy MSG stream {} to CFB v3 parser view failed: {error}",
                    path.display()
                ))
            })?;
    }
    Ok(normalized.into_inner().into_inner())
}

fn duplicate_minifat_reference(error: &std::io::Error) -> bool {
    let message = error.to_string();
    error.kind() == std::io::ErrorKind::InvalidData
        && message.starts_with("Malformed MiniFAT (mini sector ")
        && message.ends_with(" pointed to twice)")
}

fn decode_msg_string8_property(bytes: &[u8], codepage: Option<u32>) -> Option<String> {
    if codepage == Some(1200) {
        if bytes.ends_with(&[0, 0]) && bytes.len().is_multiple_of(2) {
            let units = bytes[..bytes.len() - 2]
                .chunks_exact(2)
                .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                .collect::<Vec<_>>();
            return String::from_utf16(units.as_slice()).ok();
        }
        let value_bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        if value_bytes.contains(&0) {
            return None;
        }
        return decode_msg_string8(value_bytes, Some(1252));
    }
    let value_bytes = bytes
        .iter()
        .position(|byte| *byte == 0)
        .map_or(bytes, |end| &bytes[..end]);
    decode_msg_string8(value_bytes, codepage)
}

fn msg_codepage_for_path(
    compound: &mut cfb::CompoundFile<Cursor<Vec<u8>>>,
    property_path: &Path,
) -> Result<Option<u32>, ImportedEmailParseError> {
    for storage_path in property_path.parent().into_iter().flat_map(Path::ancestors) {
        let properties_path = storage_path.join("__properties_version1.0");
        if !compound.is_stream(properties_path.as_path()) {
            continue;
        }
        let mut properties = Vec::new();
        compound
            .open_stream(properties_path.as_path())
            .and_then(|mut stream| stream.read_to_end(&mut properties))
            .map_err(|error| {
                ImportedEmailParseError::ParseFailed(format!(
                    "read MSG property table {} failed: {error}",
                    properties_path.display()
                ))
            })?;
        if let Some(codepage) = message_codepage_from_property_table(properties.as_slice()) {
            return Ok(Some(codepage));
        }
    }
    Ok(None)
}

fn message_codepage_from_property_table(properties: &[u8]) -> Option<u32> {
    const MESSAGE_CODEPAGE_TAG: u32 = 0x3ffd_0003;
    for header_bytes in [32usize, 8] {
        let Some(entries) = properties.get(header_bytes..) else {
            continue;
        };
        if entries.len() % 16 != 0 {
            continue;
        }
        for entry in entries.chunks_exact(16) {
            let tag = u32::from_le_bytes(entry[0..4].try_into().expect("property tag bytes"));
            let value = u32::from_le_bytes(entry[8..12].try_into().expect("property value bytes"));
            if tag == MESSAGE_CODEPAGE_TAG && value != 0 {
                return Some(value);
            }
        }
    }
    None
}

fn decode_msg_string8(bytes: &[u8], codepage: Option<u32>) -> Option<String> {
    if bytes.is_ascii() || (codepage.is_none() && std::str::from_utf8(bytes).is_ok()) {
        return std::str::from_utf8(bytes).ok().map(str::to_string);
    }
    let encoding = match codepage.unwrap_or(1252) {
        874 => encoding_rs::WINDOWS_874,
        932 => encoding_rs::SHIFT_JIS,
        936 => encoding_rs::GBK,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        1250 => encoding_rs::WINDOWS_1250,
        1251 => encoding_rs::WINDOWS_1251,
        1252 => encoding_rs::WINDOWS_1252,
        1253 => encoding_rs::WINDOWS_1253,
        1254 => encoding_rs::WINDOWS_1254,
        1255 => encoding_rs::WINDOWS_1255,
        1256 => encoding_rs::WINDOWS_1256,
        1257 => encoding_rs::WINDOWS_1257,
        1258 => encoding_rs::WINDOWS_1258,
        20127 => return None,
        20866 => encoding_rs::KOI8_R,
        21866 => encoding_rs::KOI8_U,
        28591 => encoding_rs::WINDOWS_1252,
        28592 => encoding_rs::ISO_8859_2,
        28594 => encoding_rs::ISO_8859_4,
        28595 => encoding_rs::ISO_8859_5,
        28596 => encoding_rs::ISO_8859_6,
        28597 => encoding_rs::ISO_8859_7,
        28598 => encoding_rs::ISO_8859_8,
        28599 => encoding_rs::WINDOWS_1254,
        28605 => encoding_rs::ISO_8859_15,
        65001 => encoding_rs::UTF_8,
        _ => return None,
    };
    Some(encoding.decode_without_bom_handling(bytes).0.into_owned())
}

fn decode_ascii_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?))
        .collect()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_msg_html_bytes(bytes: &[u8]) -> Option<String> {
    let (utf16_endian, utf16_bytes) = if bytes.starts_with(&[0xff, 0xfe]) {
        (Some(true), &bytes[2..])
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (Some(false), &bytes[2..])
    } else if bytes.len() >= 4 && bytes[0] == b'<' && bytes[1] == 0 {
        (Some(true), bytes)
    } else if bytes.len() >= 4 && bytes[0] == 0 && bytes[1] == b'<' {
        (Some(false), bytes)
    } else {
        (None, bytes)
    };
    if let Some(little_endian) = utf16_endian {
        if utf16_bytes.len() % 2 != 0 {
            return None;
        }
        let mut units = utf16_bytes
            .chunks_exact(2)
            .map(|pair| {
                if little_endian {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect::<Vec<_>>();
        while units.last() == Some(&0) {
            units.pop();
        }
        return String::from_utf16(&units).ok();
    }
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    let bytes = &bytes[..end];
    match String::from_utf8(bytes.to_vec()) {
        Ok(mut html) => {
            if html.starts_with('\u{feff}') {
                html.drain(..'\u{feff}'.len_utf8());
            }
            Some(html)
        }
        Err(_) if declares_windows_1252(bytes) => Some(decode_windows_1252(bytes)),
        Err(_) => None,
    }
}

fn declares_windows_1252(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(4096)];
    let ascii = String::from_utf8_lossy(prefix).to_ascii_lowercase();
    ascii.contains("windows-1252") || ascii.contains("iso-8859-1")
}

fn decode_windows_1252(bytes: &[u8]) -> String {
    const C1: [char; 32] = [
        '€', '\u{0081}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{008d}', 'Ž',
        '\u{008f}', '\u{0090}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ',
        '\u{009d}', 'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9f => C1[(byte - 0x80) as usize],
            _ => char::from(byte),
        })
        .collect()
}

fn looks_like_html(value: &str) -> bool {
    value
        .trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n'])
        .starts_with('<')
}

fn generate_rfc822(
    message: &ParsedImportedEmail,
    source_digest: &[u8; 32],
    maximum_bytes: u64,
) -> Result<Vec<u8>, ImportedEmailParseError> {
    let boundary = format!("synckhub-import-{}", hex_lower(&source_digest[..16]));
    let mut out = BoundedRfc822Writer::new(maximum_bytes);
    push_header(&mut out, "MIME-Version", "1.0")?;
    if let Some(value) = message.subject.as_deref() {
        push_header(&mut out, "Subject", value)?;
    }
    if let Some(value) = message.from.as_ref()
        && let Some(value) = format_address(value) {
            push_header(&mut out, "From", value.as_str())?;
        }
    if let Some(value) = message.sender.as_ref()
        && let Some(value) = format_address(value) {
            push_header(&mut out, "Sender", value.as_str())?;
        }
    push_address_list_header(&mut out, "Reply-To", &message.reply_to)?;
    push_address_list_header(&mut out, "To", &message.to)?;
    push_address_list_header(&mut out, "Cc", &message.cc)?;
    push_address_list_header(&mut out, "Bcc", &message.bcc)?;
    if let Some(value) = message.internet_message_id.as_deref() {
        push_header(&mut out, "Message-ID", format!("<{value}>").as_str())?;
    }
    if let Some(value) = message.sent_at.as_deref() {
        let date = chrono::DateTime::parse_from_rfc3339(value)
            .map(|dt| dt.to_rfc2822())
            .unwrap_or_else(|_| value.to_string());
        push_header(&mut out, "Date", date.as_str())?;
    }
    push_header(
        &mut out,
        "Content-Type",
        format!("multipart/mixed; boundary=\"{boundary}\"").as_str(),
    )?;
    out.push("\r\n")?;
    out.push(format!("--{boundary}\r\n").as_str())?;
    if let Some(body_html) = message.body_html.as_deref() {
        let alternative_boundary = format!("synckhub-alt-{}", hex_lower(&source_digest[16..]));
        push_header(
            &mut out,
            "Content-Type",
            format!("multipart/alternative; boundary=\"{alternative_boundary}\"").as_str(),
        )?;
        out.push("\r\n")?;
        out.push(format!("--{alternative_boundary}\r\n").as_str())?;
        push_rfc822_body_part(&mut out, "text/plain", &message.body_markdown)?;
        out.push(format!("--{alternative_boundary}\r\n").as_str())?;
        push_rfc822_body_part(&mut out, "text/html", body_html)?;
        out.push(format!("--{alternative_boundary}--\r\n").as_str())?;
    } else {
        push_rfc822_body_part(&mut out, "text/plain", &message.body_markdown)?;
    }
    for attachment in &message.attachments {
        out.push(format!("--{boundary}\r\n").as_str())?;
        push_header(
            &mut out,
            "Content-Type",
            format!(
                "{}; name=\"{}\"",
                safe_header_value(attachment.mime_type.as_str())?,
                quote_header_param(attachment.filename.as_str())
            )
            .as_str(),
        )?;
        out.push("Content-Transfer-Encoding: base64\r\n")?;
        push_header(
            &mut out,
            "Content-Disposition",
            format!(
                "{}; filename=\"{}\"",
                if attachment.is_inline {
                    "inline"
                } else {
                    "attachment"
                },
                quote_header_param(attachment.filename.as_str())
            )
            .as_str(),
        )?;
        if let Some(content_id) = attachment.content_id.as_deref() {
            push_header(&mut out, "Content-ID", format!("<{content_id}>").as_str())?;
        }
        out.push("\r\n")?;
        push_base64_lines(&mut out, &attachment.bytes)?;
    }
    out.push(format!("--{boundary}--\r\n").as_str())?;
    Ok(out.into_bytes())
}

fn push_rfc822_body_part(
    out: &mut BoundedRfc822Writer,
    mime_type: &str,
    body: &[u8],
) -> Result<(), ImportedEmailParseError> {
    out.push(format!("Content-Type: {mime_type}; charset=utf-8\r\n").as_str())?;
    out.push("Content-Transfer-Encoding: base64\r\n\r\n")?;
    push_base64_lines(out, body)
}

fn validate_generated_rfc822(
    expected: &ParsedImportedEmail,
    limits: &ImportedEmailLimits,
) -> Result<(), ImportedEmailParseError> {
    let parsed = mailparse::parse_mail(expected.raw_rfc822.as_slice()).map_err(|e| {
        ImportedEmailParseError::Rfc822GenerationFailed(format!(
            "generated message does not parse: {e}"
        ))
    })?;
    let mut plain = None;
    let mut html = None;
    let mut leaves = Vec::new();
    let mut budget = EmlTraversalBudget::new(limits);
    collect_eml_leaves(
        &parsed,
        "0",
        0,
        &mut plain,
        &mut html,
        &mut leaves,
        &mut budget,
    )?;
    if plain.as_ref().map(String::as_bytes) != Some(expected.body_markdown.as_slice()) {
        return Err(ImportedEmailParseError::Rfc822GenerationFailed(
            "generated body does not reproduce normalized body".to_string(),
        ));
    }
    if html.as_ref().map(String::as_bytes) != expected.body_html.as_deref() {
        return Err(ImportedEmailParseError::Rfc822GenerationFailed(
            "generated HTML does not reproduce preview HTML".to_string(),
        ));
    }
    if leaves.len() != expected.attachments.len() {
        return Err(ImportedEmailParseError::Rfc822GenerationFailed(format!(
            "generated attachment count {} does not reproduce expected {}: {}",
            leaves.len(),
            expected.attachments.len(),
            leaves
                .iter()
                .map(|leaf| format!("{} ({})", leaf.filename, leaf.mime_type))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if let Some((index, (actual, expected))) = leaves
        .iter()
        .zip(&expected.attachments)
        .enumerate()
        .find(|(_, (actual, expected))| actual.bytes != expected.bytes)
    {
        return Err(ImportedEmailParseError::Rfc822GenerationFailed(format!(
            "generated attachment {} has {} bytes, expected {}",
            index + 1,
            actual.bytes.len(),
            expected.bytes.len()
        )));
    }
    Ok(())
}

fn addresses_from_header(headers: &[MailHeader<'_>], name: &str) -> Vec<ImportedEmailAddress> {
    let Some(header) = headers.get_first_header(name) else {
        return Vec::new();
    };
    mailparse::addrparse_header(header)
        .map(|addresses| flatten_addresses(addresses.as_ref()))
        .unwrap_or_default()
}

fn addresses_from_text(raw: &str) -> Vec<ImportedEmailAddress> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    mailparse::addrparse(raw)
        .map(|addresses| flatten_addresses(addresses.as_ref()))
        .unwrap_or_default()
}

fn flatten_addresses(addresses: &[MailAddr]) -> Vec<ImportedEmailAddress> {
    let mut out = Vec::new();
    for address in addresses {
        match address {
            MailAddr::Single(single) => out.push(ImportedEmailAddress {
                name: clean_one_line(single.display_name.as_deref()),
                email: normalize_smtp_address(single.addr.as_str()),
            }),
            MailAddr::Group(group) => {
                out.extend(group.addrs.iter().map(|single| ImportedEmailAddress {
                    name: clean_one_line(single.display_name.as_deref()),
                    email: normalize_smtp_address(single.addr.as_str()),
                }));
            }
        }
    }
    out
}

fn addresses_from_people(people: &[Person]) -> Vec<ImportedEmailAddress> {
    people.iter().filter_map(address_from_person).collect()
}

fn address_from_person(person: &Person) -> Option<ImportedEmailAddress> {
    let name = clean_one_line(Some(person.name.as_str()));
    let email = normalize_smtp_address(person.email.as_str());
    if name.is_none() && email.is_none() {
        None
    } else {
        Some(ImportedEmailAddress { name, email })
    }
}

fn normalize_smtp_address(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty()
        || value.chars().any(|ch| {
            ch.is_control()
                || ch.is_whitespace()
                || matches!(
                    ch,
                    '<' | '>' | ',' | ';' | ':' | '"' | '\\' | '(' | ')' | '[' | ']'
                )
        })
    {
        return None;
    }
    let upper = value.to_ascii_uppercase();
    let (local, domain) = value.split_once('@')?;
    if upper.starts_with("/O=")
        || upper.starts_with("/CN=")
        || local.is_empty()
        || local.contains('@')
        || domain.is_empty()
        || domain.contains('@')
    {
        None
    } else {
        Some(value.to_string())
    }
}

fn normalize_mime_type(raw: &str) -> Option<String> {
    let value = raw.trim();
    let (top_level, subtype) = value.split_once('/')?;
    let valid_token = |token: &str| {
        !token.is_empty()
            && token.chars().all(|ch| {
                ch.is_ascii_alphanumeric()
                    || matches!(
                        ch,
                        '!' | '#'
                            | '$'
                            | '%'
                            | '&'
                            | '\''
                            | '*'
                            | '+'
                            | '-'
                            | '.'
                            | '^'
                            | '_'
                            | '`'
                            | '|'
                            | '~'
                    )
            })
    };
    if !valid_token(top_level) || !valid_token(subtype) {
        None
    } else {
        Some(value.to_string())
    }
}

fn distinct_sender(
    from: Option<ImportedEmailAddress>,
    sender: Option<ImportedEmailAddress>,
) -> Option<ImportedEmailAddress> {
    match sender {
        Some(sender) if Some(&sender) != from.as_ref() => Some(sender),
        _ => None,
    }
}

fn newest_received_date(headers: &[MailHeader<'_>]) -> Option<String> {
    headers
        .iter()
        .filter(|header| header.get_key_ref().eq_ignore_ascii_case("Received"))
        .filter_map(|header| {
            let value = header.get_value();
            value
                .rsplit_once(';')
                .and_then(|(_, date)| parse_mail_date(date))
        })
        .max()
}

fn parse_mail_date(raw: &str) -> Option<String> {
    let timestamp = mailparse::dateparse(raw.trim()).ok()?;
    chrono::DateTime::from_timestamp(timestamp, 0).map(|value| value.to_rfc3339())
}

fn parse_iso_date(raw: &str) -> Option<String> {
    let value = nonempty(raw)?;
    chrono::DateTime::parse_from_rfc3339(value.as_str())
        .ok()
        .map(|date| date.to_rfc3339())
}

/// Reply metadata is bounded independently of MIME bodies and attachment parsing.
/// Keep the root and recent ancestors when an imported chain exceeds our compose limit.
pub fn rfc822_thread_headers(raw: &[u8]) -> (Option<String>, Option<String>, Vec<String>) {
    use mailparse::MailHeaderMap;
    let raw = &raw[..raw.len().min(256 * 1024)];
    let Ok((headers, _)) = mailparse::parse_headers(raw) else {
        return (None, None, Vec::new());
    };
    let normalize = |value: &str| {
        let value = value.trim().trim_start_matches('<').trim_end_matches('>');
        (value.len() <= 254
            && value.split('@').count() == 2
            && !value.starts_with('@')
            && !value.ends_with('@')
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~@".contains(&b)))
        .then(|| value.to_ascii_lowercase())
    };
    let own = headers
        .get_first_value("Message-ID")
        .as_deref()
        .and_then(normalize);
    let parent = headers
        .get_first_value("In-Reply-To")
        .as_deref()
        .and_then(normalize);
    let mut references = Vec::new();
    if let Some(value) = headers.get_first_value("References") {
        for id in value.split_whitespace().filter_map(normalize) {
            if !references.contains(&id) {
                references.push(id);
                if references.len() > 100 {
                    references.remove(1);
                }
            }
        }
    }
    let conversation = references
        .first()
        .or(parent.as_ref())
        .or(own.as_ref())
        .map(|id| format!("rfc822:{id}"));
    (conversation, parent, references)
}

fn normalize_message_id(raw: &str) -> Option<String> {
    let value = raw.trim().trim_start_matches('<').trim_end_matches('>');
    clean_one_line(Some(value)).map(|value| value.to_ascii_lowercase())
}

fn normalize_content_id(raw: &str) -> Option<String> {
    let value = raw.trim().trim_start_matches('<').trim_end_matches('>');
    if value.is_empty()
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || matches!(ch, '<' | '>'))
    {
        None
    } else {
        Some(value.to_string())
    }
}

fn clean_one_line(raw: Option<&str>) -> Option<String> {
    let mut value = String::new();
    let mut separator = false;
    for ch in raw?.chars() {
        if ch.is_control() || ch.is_whitespace() {
            separator = !value.is_empty();
            continue;
        }
        if separator {
            value.push(' ');
            separator = false;
        }
        value.push(ch);
    }
    (!value.is_empty()).then_some(value)
}

fn nonempty(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn normalize_msg_plain_body(raw: &str) -> Option<String> {
    let body = nonempty(raw.split('\0').next().unwrap_or_default())?;
    let body = body.strip_prefix('\u{feff}').unwrap_or(body.as_str());
    (!body.trim().is_empty()).then(|| body.to_string())
}

fn rtf_declared_uncompressed_bytes(encoded: &str) -> Option<u64> {
    let encoded = encoded.as_bytes();
    if encoded.len() < 16 {
        return None;
    }
    let mut raw_size = [0u8; 4];
    for (index, byte) in raw_size.iter_mut().enumerate() {
        let start = 8 + index * 2;
        *byte = decode_hex_byte(encoded[start], encoded[start + 1])?;
    }
    Some(u32::from_le_bytes(raw_size) as u64)
}

fn decode_hex_byte(high: u8, low: u8) -> Option<u8> {
    fn nibble(value: u8) -> Option<u8> {
        match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            b'A'..=b'F' => Some(value - b'A' + 10),
            _ => None,
        }
    }
    Some((nibble(high)? << 4) | nibble(low)?)
}

fn fallback_attachment_name(path: &str, mime_type: &str) -> String {
    let extension = match mime_type {
        "message/rfc822" => ".eml",
        "text/plain" => ".txt",
        "text/html" => ".html",
        _ => ".bin",
    };
    format!("attachment-{path}{extension}")
}

/// `msg_parser` panics on hostile files, such as a non-ASCII stream name.
fn outlook_from_slice(source: &[u8]) -> Result<Outlook, String> {
    std::panic::catch_unwind(|| Outlook::from_slice(source))
        .map_err(|_| "MSG parser refused the file".to_string())?
        .map_err(|error| error.to_string())
}

fn msg_attachment_filename(attachment: &msg_parser::Attachment, index: usize) -> String {
    for candidate in [
        attachment.long_file_name.as_str(),
        attachment.file_name.as_str(),
        attachment.display_name.as_str(),
    ] {
        if let Some(value) = clean_one_line(Some(candidate)) {
            return value;
        }
    }
    let extension = clean_one_line(Some(attachment.extension.as_str())).unwrap_or_else(|| {
        if attachment.is_embedded_message() {
            ".msg".to_string()
        } else {
            ".bin".to_string()
        }
    });
    format!("attachment-{}{}", index + 1, extension)
}

fn push_address_list_header(
    out: &mut BoundedRfc822Writer,
    name: &str,
    addresses: &[ImportedEmailAddress],
) -> Result<(), ImportedEmailParseError> {
    if !addresses.is_empty() {
        let value = addresses
            .iter()
            .filter_map(format_address)
            .collect::<Vec<_>>()
            .join(", ");
        if !value.is_empty() {
            push_header(out, name, value.as_str())?;
        }
    }
    Ok(())
}

fn format_address(address: &ImportedEmailAddress) -> Option<String> {
    match (address.name.as_deref(), address.email.as_deref()) {
        (Some(name), Some(email)) => Some(format!(
            "\"{}\" <{}>",
            name.replace(['"', '\\'], " "),
            email
        )),
        (None, Some(email)) => Some(email.to_string()),
        (Some(_), None) | (None, None) => None,
    }
}

fn push_header(
    out: &mut BoundedRfc822Writer,
    name: &str,
    value: &str,
) -> Result<(), ImportedEmailParseError> {
    out.push(name)?;
    out.push(": ")?;
    out.push(safe_header_value(value)?)?;
    out.push("\r\n")?;
    Ok(())
}

fn safe_header_value(value: &str) -> Result<&str, ImportedEmailParseError> {
    if value.chars().any(char::is_control) {
        return Err(ImportedEmailParseError::Rfc822GenerationFailed(
            "header value contains a control character".to_string(),
        ));
    }
    Ok(value)
}

fn quote_header_param(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '"' | '\\') {
                '_'
            } else {
                ch
            }
        })
        .collect()
}

fn push_base64_lines(
    out: &mut BoundedRfc822Writer,
    bytes: &[u8],
) -> Result<(), ImportedEmailParseError> {
    if bytes.is_empty() {
        return out.push("\r\n");
    }
    for chunk in bytes.chunks(57) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
        out.push(encoded.as_str())?;
        out.push("\r\n")?;
    }
    Ok(())
}

struct BoundedRfc822Writer {
    out: String,
    maximum_bytes: u64,
}

impl BoundedRfc822Writer {
    fn new(maximum_bytes: u64) -> Self {
        Self {
            out: String::new(),
            maximum_bytes,
        }
    }

    fn push(&mut self, value: &str) -> Result<(), ImportedEmailParseError> {
        let next_len = self.out.len().checked_add(value.len()).ok_or_else(|| {
            ImportedEmailParseError::LimitExceeded(
                "generated RFC822 byte count overflow".to_string(),
            )
        })?;
        require_limit(
            "generated RFC822 bytes",
            next_len as u64,
            self.maximum_bytes,
        )?;
        self.out.push_str(value);
        Ok(())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.out.into_bytes()
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn html_to_text(input: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut hidden = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_tag {
            if ch == '>' {
                let raw = tag_buf.trim();
                let closing = raw.starts_with('/');
                let self_closing = raw.ends_with('/');
                let tag = raw
                    .trim_start_matches('/')
                    .split(|ch: char| ch.is_whitespace() || ch == '/')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let hides_text = matches!(
                    tag.as_str(),
                    "head" | "style" | "script" | "template" | "xml"
                );
                if closing {
                    if hidden.last().is_some_and(|open| open == &tag) {
                        hidden.pop();
                    } else if hidden.is_empty() && matches!(tag.as_str(), "br" | "p" | "div" | "li")
                    {
                        out.push('\n');
                    }
                } else if hides_text && !self_closing {
                    hidden.push(tag.clone());
                } else if hidden.is_empty() && matches!(tag.as_str(), "br" | "p" | "div" | "li") {
                    out.push('\n');
                }
                tag_buf.clear();
                in_tag = false;
            } else {
                tag_buf.push(ch);
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if !hidden.is_empty() {
            continue;
        }
        if ch == '&' {
            let mut entity = String::new();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == ';' {
                    break;
                }
                if entity.len() > 10 {
                    break;
                }
                entity.push(next);
            }
            let decoded = match entity.as_str() {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "quot" => "\"",
                "#39" => "'",
                "nbsp" => " ",
                _ => "",
            };
            if decoded.is_empty() {
                out.push('&');
                out.push_str(&entity);
            } else {
                out.push_str(decoded);
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPRESENTATIVE_EML: &[u8] =
        include_bytes!("../tests/fixtures/imported_email/representative.eml");
    const ASCII_MSG_BASE64: &str = include_str!("../tests/fixtures/imported_email/ascii.msg.b64");
    const ATTACHMENT_MSG_BASE64: &str =
        include_str!("../tests/fixtures/imported_email/test_email_3.msg.b64");
    const ASCII_MSG_SOURCE_SHA256: &str =
        "a6fc2f852e2ad1b89cf43db7b653d2df96aacca9daf1ee12250f1f3f56e211e2";
    const ASCII_MSG_RFC822_SHA256: &str =
        "ba1ca30dceb4bf51406094d4dc47f48a764bd8b03050ac58407b2053304e57b6";

    fn test_limits(source_len: usize) -> ImportedEmailLimits {
        ImportedEmailLimits::for_source_limit_and_parser_concurrency(source_len as u64, 2)
    }

    fn decode_msg_fixture(base64: &str) -> Vec<u8> {
        let encoded = base64
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect::<String>();
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("MSG fixture base64 must decode")
    }

    fn copy_msg_fixture_to_version(source: &[u8], version: cfb::Version) -> Vec<u8> {
        let mut input =
            cfb::CompoundFile::open(Cursor::new(source.to_vec())).expect("open MSG fixture");
        let entries = input
            .walk()
            .skip(1)
            .map(|entry| {
                (
                    entry.path().to_path_buf(),
                    entry.is_storage(),
                    entry.is_stream(),
                )
            })
            .collect::<Vec<_>>();
        let mut output = cfb::CompoundFile::create_with_version(version, Cursor::new(Vec::new()))
            .expect("create copied MSG fixture");
        for (path, is_storage, is_stream) in entries {
            if is_storage {
                output
                    .create_storage_all(path.as_path())
                    .expect("copy MSG fixture storage");
            } else if is_stream {
                let mut bytes = Vec::new();
                input
                    .open_stream(path.as_path())
                    .expect("open MSG fixture stream")
                    .read_to_end(&mut bytes)
                    .expect("read MSG fixture stream");
                if let Some(parent) = path.parent() {
                    output
                        .create_storage_all(parent)
                        .expect("copy MSG fixture stream parent");
                }
                output
                    .create_stream(path.as_path())
                    .expect("create MSG fixture stream")
                    .write_all(bytes.as_slice())
                    .expect("write MSG fixture stream");
            }
        }
        output.into_inner().into_inner()
    }

    #[test]
    fn deeply_nested_mime_is_rejected_before_recursive_decoding() {
        let mut message = String::new();
        for depth in 0..2_000 {
            message.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=b{depth}\r\n\r\n--b{depth}\r\n"
            ));
        }
        message.push_str("Content-Type: text/plain\r\n\r\nbody");
        let error = parse_imported_email(
            ImportedEmailFormat::Eml,
            message.as_bytes(),
            &test_limits(message.len()),
        )
        .unwrap_err();
        assert!(
            matches!(error,ImportedEmailParseError::LimitExceeded(message) if message.contains("MIME nesting"))
        );
    }

    #[test]
    fn parser_text_sanitization_preserves_source_and_attachment_bytes() {
        let mut parsed = ParsedImportedEmail {
            format: ImportedEmailFormat::Eml,
            internet_message_id: Some("id\0@example.test".to_string()),
            subject: Some("sub\0ject".to_string()),
            from: Some(ImportedEmailAddress {
                name: Some("sen\0der".to_string()),
                email: Some("sender\0@example.test".to_string()),
            }),
            sender: None,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            sent_at: None,
            received_at: None,
            body_markdown: b"plain\0text".to_vec(),
            body_html: Some(b"<p>html\0text</p>".to_vec()),
            attachments: vec![ImportedEmailAttachment {
                filename: "file\0.bin".to_string(),
                mime_type: "application\0/octet-stream".to_string(),
                bytes: vec![0, 1, 0, 2],
                is_inline: false,
                content_id: Some("content\0id".to_string()),
                embedded_data_image: false,
            }],
            raw_rfc822: b"original\0source".to_vec(),
            raw_rfc822_generated: false,
        };

        strip_parser_text_nuls(&mut parsed);

        assert_eq!(
            parsed.internet_message_id.as_deref(),
            Some("id@example.test")
        );
        assert_eq!(parsed.subject.as_deref(), Some("subject"));
        assert_eq!(parsed.body_markdown, b"plaintext");
        assert_eq!(
            parsed.body_html.as_deref(),
            Some(b"<p>htmltext</p>".as_slice())
        );
        assert_eq!(parsed.attachments[0].filename, "file.bin");
        assert_eq!(parsed.attachments[0].bytes, vec![0, 1, 0, 2]);
        assert_eq!(parsed.raw_rfc822, b"original\0source");
    }

    #[test]
    fn eml_parser_strips_derived_nuls_but_preserves_original_source() {
        let source = b"From: sender@example.test\r\nSubject: sub\0ject\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nplain\0text";
        let parsed =
            parse_imported_email(ImportedEmailFormat::Eml, source, &test_limits(source.len()))
                .expect("parse EML containing NUL characters");

        assert_eq!(parsed.subject.as_deref(), Some("sub ject"));
        assert_eq!(parsed.body_markdown, b"plaintext");
        assert_eq!(parsed.raw_rfc822, source);
    }

    #[test]
    fn extension_classification_is_case_insensitive_and_final() {
        assert_eq!(
            ImportedEmailFormat::from_name("message.EML"),
            Some(ImportedEmailFormat::Eml)
        );
        assert_eq!(
            ImportedEmailFormat::from_name("archive.tar.msg"),
            Some(ImportedEmailFormat::Msg)
        );
        assert_eq!(ImportedEmailFormat::from_name("message.eml.txt"), None);
    }

    #[test]
    fn msg_html_property_decodes_binary_hex_without_accepting_arbitrary_hex() {
        let html = "<meta charset=\"utf-8\"><p>Visible</p>";
        assert_eq!(
            decode_msg_html_property(html, 1024).unwrap().as_deref(),
            Some(html)
        );
        assert_eq!(
            decode_msg_html_property(format!("\u{feff}{html}").as_str(), 1024)
                .unwrap()
                .as_deref(),
            Some(html)
        );
        assert_eq!(
            decode_msg_html_property(hex::encode(format!("\u{feff}{html}")).as_str(), 1024)
                .unwrap()
                .as_deref(),
            Some(html)
        );
        assert_eq!(
            decode_msg_html_property(hex::encode(html).as_str(), 1024)
                .unwrap()
                .as_deref(),
            Some(html)
        );
        assert_eq!(decode_msg_html_property("deadbeef", 1024).unwrap(), None);

        let mut utf16_le = vec![0xff, 0xfe];
        for unit in "<p>Unicode ✓</p>\0".encode_utf16() {
            utf16_le.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            decode_msg_html_property(hex::encode(utf16_le).as_str(), 1024)
                .unwrap()
                .as_deref(),
            Some("<p>Unicode ✓</p>")
        );

        let windows_1252 = b"<meta charset=windows-1252><p>\x93quoted\x94</p>";
        assert_eq!(
            decode_msg_html_property(hex::encode(windows_1252).as_str(), 1024)
                .unwrap()
                .as_deref(),
            Some("<meta charset=windows-1252><p>“quoted”</p>")
        );
        assert_eq!(
            decode_msg_html_property(hex::encode(b"<p>\x93quoted\x94</p>").as_str(), 1024).unwrap(),
            None
        );
    }

    #[test]
    fn msg_strings_use_message_codepage_and_drop_terminators() {
        let mut compound =
            cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("create MSG compound file");
        let mut properties = vec![0u8; 32];
        properties.extend_from_slice(&0x3ffd_0003u32.to_le_bytes());
        properties.extend_from_slice(&0u32.to_le_bytes());
        properties.extend_from_slice(&1252u32.to_le_bytes());
        properties.extend_from_slice(&0u32.to_le_bytes());
        compound
            .create_stream("/__properties_version1.0")
            .expect("create MSG property table")
            .write_all(properties.as_slice())
            .expect("write MSG property table");
        compound
            .create_stream("/__substg1.0_0037001E")
            .expect("create MSG String8 subject")
            .write_all(b"Caf\xe9\0")
            .expect("write MSG String8 subject");
        compound
            .create_stream("/__substg1.0_1000001F")
            .expect("create MSG Unicode body")
            .write_all(b"B\0o\0d\0y\0\0\0")
            .expect("write MSG Unicode body");
        let source = compound.into_inner().into_inner();

        let normalized = normalize_msg_strings(source).expect("normalize MSG strings");
        let mut normalized_compound =
            cfb::CompoundFile::open(Cursor::new(normalized.clone())).expect("open normalized MSG");
        let mut subject = Vec::new();
        normalized_compound
            .open_stream("/__substg1.0_0037001E")
            .expect("open normalized subject")
            .read_to_end(&mut subject)
            .expect("read normalized subject");
        assert_eq!(subject, "Café".as_bytes());
        let mut body = Vec::new();
        normalized_compound
            .open_stream("/__substg1.0_1000001F")
            .expect("open normalized Unicode body")
            .read_to_end(&mut body)
            .expect("read normalized Unicode body");
        assert_eq!(body, b"B\0o\0d\0y\0");
    }

    #[test]
    fn msg_string8_codepage_1200_accepts_utf16_and_mislabeled_string8() {
        let mut compound =
            cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("create MSG compound file");
        let mut properties = vec![0u8; 32];
        properties.extend_from_slice(&0x3ffd_0003u32.to_le_bytes());
        properties.extend_from_slice(&0u32.to_le_bytes());
        properties.extend_from_slice(&1200u32.to_le_bytes());
        properties.extend_from_slice(&0u32.to_le_bytes());
        compound
            .create_stream("/__properties_version1.0")
            .expect("create MSG property table")
            .write_all(properties.as_slice())
            .expect("write MSG property table");
        let mut encoded = Vec::new();
        for unit in "José\0".encode_utf16() {
            encoded.extend_from_slice(&unit.to_le_bytes());
        }
        compound
            .create_stream("/__substg1.0_0040001E")
            .expect("create codepage 1200 String8 property")
            .write_all(encoded.as_slice())
            .expect("write codepage 1200 String8 property");

        let normalized = normalize_msg_strings(compound.into_inner().into_inner())
            .expect("normalize codepage 1200 MSG string");
        let mut normalized_compound =
            cfb::CompoundFile::open(Cursor::new(normalized)).expect("open normalized MSG");
        let mut value = Vec::new();
        normalized_compound
            .open_stream("/__substg1.0_0040001E")
            .expect("open normalized codepage 1200 property")
            .read_to_end(&mut value)
            .expect("read normalized codepage 1200 property");
        assert_eq!(value, "José".as_bytes());
        assert_eq!(normalized_compound.version(), cfb::Version::V3);

        assert_eq!(
            decode_msg_string8_property(b"Caf\xe9\0", Some(1200)).as_deref(),
            Some("Café")
        );
        assert_eq!(
            decode_msg_string8_property(b"ASCII", Some(1200)).as_deref(),
            Some("ASCII")
        );
        assert!(decode_msg_string8_property(&[0x00, 0xd8, 0, 0], Some(1200)).is_none());
        assert!(decode_msg_string8_property(&[0x00, 0xd8], Some(1200)).is_none());
    }

    #[test]
    fn cfb_v4_msg_is_normalized_for_the_v3_only_parser() {
        let source = decode_msg_fixture(ASCII_MSG_BASE64);
        let source = copy_msg_fixture_to_version(source.as_slice(), cfb::Version::V4);
        let parsed = parse_imported_email(
            ImportedEmailFormat::Msg,
            source.as_slice(),
            &test_limits(source.len()),
        )
        .expect("parse CFB v4 MSG through the normalized parser view");

        assert_eq!(
            parsed.subject.as_deref(),
            Some("creating an outlook message file")
        );
        assert_eq!(
            parsed.body_markdown,
            b"This message is created by Aspose.Email"
        );
    }

    #[test]
    fn duplicate_minifat_fallback_matches_only_the_observed_cfb_error() {
        assert!(duplicate_minifat_reference(&std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Malformed MiniFAT (mini sector 0 pointed to twice)",
        )));
        assert!(!duplicate_minifat_reference(&std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Malformed MiniFAT",
        )));
        assert!(!duplicate_minifat_reference(&std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid CFB file (wrong magic number)",
        )));
    }

    #[test]
    fn msg_strings_do_not_use_internet_body_codepage() {
        let mut compound =
            cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("create MSG compound file");
        let mut properties = vec![0u8; 32];
        properties.extend_from_slice(&0x3fde_0003u32.to_le_bytes());
        properties.extend_from_slice(&0u32.to_le_bytes());
        properties.extend_from_slice(&20127u32.to_le_bytes());
        properties.extend_from_slice(&0u32.to_le_bytes());
        compound
            .create_stream("/__properties_version1.0")
            .expect("create MSG property table")
            .write_all(properties.as_slice())
            .expect("write MSG property table");
        compound
            .create_stream("/__substg1.0_3FFA001E")
            .expect("create MSG String8 property")
            .write_all(b"Caf\xe9\0")
            .expect("write MSG String8 property");

        let normalized = normalize_msg_strings(compound.into_inner().into_inner())
            .expect("normalize MSG strings");
        let mut normalized_compound =
            cfb::CompoundFile::open(Cursor::new(normalized)).expect("open normalized MSG");
        let mut value = Vec::new();
        normalized_compound
            .open_stream("/__substg1.0_3FFA001E")
            .expect("open normalized String8 property")
            .read_to_end(&mut value)
            .expect("read normalized String8 property");
        assert_eq!(value, "Café".as_bytes());
    }

    #[test]
    fn msg_plain_body_drops_utf8_bom_before_rfc822_generation() {
        let body = normalize_msg_plain_body("\u{feff}Body text\0").expect("normalized body");
        assert_eq!(body, "Body text");

        let mut parsed = ParsedImportedEmail {
            format: ImportedEmailFormat::Msg,
            internet_message_id: None,
            subject: None,
            from: None,
            sender: None,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            sent_at: None,
            received_at: None,
            body_markdown: body.into_bytes(),
            body_html: None,
            attachments: Vec::new(),
            raw_rfc822: Vec::new(),
            raw_rfc822_generated: true,
        };
        let limits = test_limits(1);
        let source_digest: [u8; 32] = Sha256::digest(b"source").into();
        parsed.raw_rfc822 = generate_rfc822(&parsed, &source_digest, limits.generated_rfc822_bytes)
            .expect("generate RFC822");
        validate_generated_rfc822(&parsed, &limits).expect("round-trip generated RFC822");
    }

    #[test]
    fn eml_parser_preserves_source_and_materializes_attachments() {
        let source = concat!(
            "From: Visible <visible@example.com>\r\n",
            "Sender: Delegate <delegate@example.com>\r\n",
            "To: Person <person@example.com>\r\n",
            "Cc: cc@example.com\r\n",
            "Bcc: bcc@example.com\r\n",
            "Reply-To: replies@example.com\r\n",
            "Subject: Example\r\n",
            "Message-ID: <Example@ID>\r\n",
            "Date: Wed, 15 Jul 2026 11:00:00 +0000\r\n",
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/mixed; boundary=x\r\n",
            "\r\n",
            "--x\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "hello\r\n",
            "--x\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-ID: <ordinary-doc>\r\n",
            "Content-Disposition: attachment; filename=doc.pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "JVBERg==\r\n",
            "--x--\r\n"
        )
        .as_bytes();
        let parsed =
            parse_imported_email(ImportedEmailFormat::Eml, source, &test_limits(source.len()))
                .unwrap();
        assert_eq!(parsed.raw_rfc822, source);
        assert_eq!(parsed.internet_message_id.as_deref(), Some("example@id"));
        assert_eq!(parsed.body_markdown, b"hello\r\n");
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].bytes, b"%PDF");
        assert!(!parsed.attachments[0].is_inline);
        assert_eq!(
            parsed.sender.unwrap().email.as_deref(),
            Some("delegate@example.com")
        );
    }

    #[test]
    fn eml_parser_materializes_html_data_images_as_inline_attachments() {
        let png = b"\x89PNG\r\n\x1a\nsmall";
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        let source = format!(
            "From: sender@example.test\r\n\
             Subject: Embedded image\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <p><img src=\"data:image/png;base64,{encoded}\"></p>"
        );
        let parsed = parse_imported_email(
            ImportedEmailFormat::Eml,
            source.as_bytes(),
            &test_limits(source.len()),
        )
        .expect("parse embedded image EML");
        assert_eq!(parsed.raw_rfc822, source.as_bytes());
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].bytes, png);
        assert!(parsed.attachments[0].is_inline);
        assert!(parsed.attachments[0].embedded_data_image);
        let html = String::from_utf8(parsed.body_html.expect("HTML body")).unwrap();
        assert!(html.contains("src=\"cid:synckhub-data-"), "{html}");
        assert!(!html.contains("data:image"), "{html}");

        let mut limits = test_limits(source.len());
        limits.attachment_count = 0;
        assert!(matches!(
            parse_imported_email(ImportedEmailFormat::Eml, source.as_bytes(), &limits),
            Err(ImportedEmailParseError::LimitExceeded(_))
        ));
    }

    #[test]
    fn eml_parser_drops_malformed_data_image_without_losing_the_message() {
        let source = "From: sender@example.test\r\n\
                      Subject: Broken decorative image\r\n\
                      MIME-Version: 1.0\r\n\
                      Content-Type: text/html; charset=utf-8\r\n\
                      \r\n\
                      <p>Keep this body.</p><img src=\"data:image/png;base64,abcde\">";
        let parsed = parse_imported_email(
            ImportedEmailFormat::Eml,
            source.as_bytes(),
            &test_limits(source.len()),
        )
        .expect("parse EML with malformed data image");

        assert_eq!(parsed.raw_rfc822, source.as_bytes());
        assert!(parsed.attachments.is_empty());
        assert_eq!(
            String::from_utf8(parsed.body_markdown).unwrap().trim(),
            "Keep this body."
        );
        let html = String::from_utf8(parsed.body_html.expect("HTML body")).unwrap();
        assert!(html.contains("Keep this body."), "{html}");
        assert!(!html.contains("data:image"), "{html}");
    }

    #[test]
    fn representative_eml_fixture_has_golden_normalization() {
        let parsed = parse_imported_email(
            ImportedEmailFormat::Eml,
            REPRESENTATIVE_EML,
            &test_limits(REPRESENTATIVE_EML.len()),
        )
        .unwrap();

        assert_eq!(
            hex_lower(&Sha256::digest(REPRESENTATIVE_EML)).as_str(),
            "442ed125fc0aec4251d963c0cba5134a7e9047a1c0be3157218177a18511d5ec"
        );
        assert_eq!(parsed.raw_rfc822, REPRESENTATIVE_EML);
        assert!(!parsed.raw_rfc822_generated);
        assert_eq!(
            parsed.internet_message_id.as_deref(),
            Some("fixture-1@example.test")
        );
        assert_eq!(parsed.subject.as_deref(), Some("Imported fixture ✓"));
        assert_eq!(
            parsed
                .from
                .as_ref()
                .and_then(|value| value.email.as_deref()),
            Some("visible@example.test")
        );
        assert_eq!(
            parsed
                .sender
                .as_ref()
                .and_then(|value| value.email.as_deref()),
            Some("delegate@example.test")
        );
        assert_eq!(parsed.to[0].email.as_deref(), Some("to@example.test"));
        assert_eq!(parsed.cc[0].email.as_deref(), Some("cc@example.test"));
        assert_eq!(parsed.bcc[0].email.as_deref(), Some("bcc@example.test"));
        assert_eq!(
            parsed.reply_to[0].email.as_deref(),
            Some("reply@example.test")
        );
        assert_eq!(parsed.sent_at.as_deref(), Some("2026-07-15T11:00:00+00:00"));
        assert_eq!(
            parsed.received_at.as_deref(),
            Some("2026-07-15T11:01:00+00:00")
        );
        assert_eq!(parsed.body_markdown, b"Fixture body.\n");
        assert_eq!(
            parsed.body_html.as_deref(),
            Some(b"<p>Fixture body.</p><img src=\"cid:inline-image\">\n".as_slice())
        );
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].filename, "inline.png");
        assert_eq!(parsed.attachments[0].bytes, b"image");
        assert!(parsed.attachments[0].is_inline);
        assert_eq!(parsed.attachments[1].filename, "ordinary.pdf");
        assert_eq!(parsed.attachments[1].bytes, b"%PDF");
        assert!(!parsed.attachments[1].is_inline);
    }

    #[test]
    fn real_msg_fixture_has_deterministic_golden_normalization() {
        let source = decode_msg_fixture(ASCII_MSG_BASE64);
        let limits = test_limits(source.len());
        let first = parse_imported_email(ImportedEmailFormat::Msg, &source, &limits).unwrap();
        let second = parse_imported_email(ImportedEmailFormat::Msg, &source, &limits).unwrap();

        assert_eq!(hex_lower(&Sha256::digest(&source)), ASCII_MSG_SOURCE_SHA256);
        assert_eq!(
            first.subject.as_deref(),
            Some("creating an outlook message file")
        );
        assert_eq!(
            first.from.as_ref().and_then(|value| value.email.as_deref()),
            Some("from@domain.com")
        );
        assert_eq!(first.to[0].email.as_deref(), Some("to@domain.com"));
        assert_eq!(
            first.body_markdown,
            b"This message is created by Aspose.Email"
        );
        assert!(first.body_html.is_none());
        assert!(first.attachments.is_empty());
        assert!(first.raw_rfc822_generated);
        assert_eq!(first.raw_rfc822, second.raw_rfc822);
        assert_eq!(
            hex_lower(&Sha256::digest(&first.raw_rfc822)),
            ASCII_MSG_RFC822_SHA256
        );
    }

    #[test]
    fn real_msg_attachment_fixture_preserves_payload_order() {
        let source = decode_msg_fixture(ATTACHMENT_MSG_BASE64);
        let outlook = Outlook::from_slice(&source).expect("parse MSG fixture");
        assert!(!outlook.body.trim().is_empty());
        assert!(outlook.html.trim().is_empty());
        assert!(
            outlook
                .rtf_decompressed()
                .as_deref()
                .and_then(msg_rtf::extract_encapsulated_html)
                .is_some()
        );
        let parsed = parse_imported_email(
            ImportedEmailFormat::Msg,
            &source,
            &test_limits(source.len()),
        )
        .unwrap();

        assert_eq!(
            hex_lower(&Sha256::digest(&source)),
            "29e4b20323cdd95bcea6040aed7c69becee571cbc0d8855d8853b5c03646d375"
        );
        assert_eq!(
            parsed.subject.as_deref(),
            Some("Welcome to your new Outlook.com account")
        );
        assert_eq!(
            parsed
                .from
                .as_ref()
                .and_then(|value| value.email.as_deref()),
            Some("no-reply@microsoft.com")
        );
        assert_eq!(
            parsed.sent_at.as_deref(),
            Some("2021-03-14T03:05:03.125+00:00")
        );
        assert_eq!(
            parsed.received_at.as_deref(),
            Some("2021-03-14T03:05:03.125+00:00")
        );
        assert_eq!(
            hex_lower(&Sha256::digest(&parsed.body_markdown)),
            "00e963153e92e3d1f0b3e73c1482e69da054c2e800eb71f8b80a6d50b7418fbf"
        );
        let body_html = parsed.body_html.as_deref().expect("RTF-derived HTML");
        assert!(!body_html.windows(4).any(|window| window == b">**<"));
        assert_eq!(
            hex_lower(&Sha256::digest(body_html)),
            "c0659105d0cc5475982f180faa04a0b2a5a2c6a85e5b73a374498e36c72103e6"
        );
        assert_eq!(parsed.attachments.len(), 11);
        assert_eq!(
            parsed
                .attachments
                .iter()
                .filter(|attachment| attachment.is_inline)
                .count(),
            10
        );
        assert_eq!(parsed.attachments[0].filename, "colors.png");
        assert_eq!(parsed.attachments[0].bytes.len(), 5_306);
        assert_eq!(
            hex_lower(&Sha256::digest(&parsed.attachments[0].bytes)),
            "f7c84a9569ffec90c4f96ff2158d8f5061c2b827e0bbe4fd5ac5fff43357e528"
        );
        assert_eq!(parsed.attachments[10].filename, "microsoft-logo.png");
        assert_eq!(parsed.attachments[10].bytes.len(), 2_446);
        assert_eq!(
            hex_lower(&Sha256::digest(&parsed.attachments[10].bytes)),
            "a4e3abc6bf3ee4596151abb8d336f883860703e8ea297eb77181189a36813626"
        );
        assert_eq!(
            hex_lower(&Sha256::digest(&parsed.raw_rfc822)),
            "91a454b25396a0b8fbdb976515a42e1d1a48fd02c0e93709ab4d25c6a577a8db"
        );
    }

    #[test]
    fn import_limits_reject_without_truncation() {
        assert_eq!(
            rtf_declared_uncompressed_bytes("0000000000010000"),
            Some(256)
        );
        assert_eq!(rtf_declared_uncompressed_bytes("not-hex"), None);

        let mut source_limits = test_limits(REPRESENTATIVE_EML.len());
        source_limits.source_bytes = (REPRESENTATIVE_EML.len() - 1) as u64;
        assert!(matches!(
            parse_imported_email(ImportedEmailFormat::Eml, REPRESENTATIVE_EML, &source_limits),
            Err(ImportedEmailParseError::LimitExceeded(_))
        ));

        let mut header_limits = test_limits(REPRESENTATIVE_EML.len());
        header_limits.header_bytes = 32;
        assert!(matches!(
            parse_imported_email(ImportedEmailFormat::Eml, REPRESENTATIVE_EML, &header_limits),
            Err(ImportedEmailParseError::LimitExceeded(_))
        ));

        let mut attachment_limits = test_limits(REPRESENTATIVE_EML.len());
        attachment_limits.attachment_count = 1;
        assert!(matches!(
            parse_imported_email(
                ImportedEmailFormat::Eml,
                REPRESENTATIVE_EML,
                &attachment_limits,
            ),
            Err(ImportedEmailParseError::LimitExceeded(_))
        ));

        let msg = decode_msg_fixture(ASCII_MSG_BASE64);
        let mut generated_limits = test_limits(msg.len());
        generated_limits.generated_rfc822_bytes = 64;
        assert!(matches!(
            parse_imported_email(ImportedEmailFormat::Msg, &msg, &generated_limits),
            Err(ImportedEmailParseError::LimitExceeded(_))
        ));
    }

    #[test]
    fn invalid_eml_without_headers_is_rejected() {
        let source = b"not an email";
        let error =
            parse_imported_email(ImportedEmailFormat::Eml, source, &test_limits(source.len()))
                .unwrap_err();
        assert!(matches!(error, ImportedEmailParseError::InvalidFormat(_)));
    }

    #[test]
    fn truncated_real_msg_is_rejected() {
        let mut source = decode_msg_fixture(ASCII_MSG_BASE64);
        source.truncate(source.len() / 2);
        let error = parse_imported_email(
            ImportedEmailFormat::Msg,
            &source,
            &test_limits(source.len()),
        )
        .unwrap_err();
        assert!(matches!(error, ImportedEmailParseError::InvalidFormat(_)));
    }

    #[test]
    fn x500_is_not_exposed_as_smtp() {
        assert_eq!(
            normalize_smtp_address("/O=EXCHANGE/CN=Recipients/CN=A"),
            None
        );
        assert_eq!(
            format_address(&ImportedEmailAddress {
                name: Some("Exchange User".to_string()),
                email: None,
            }),
            None
        );
        assert_eq!(
            normalize_smtp_address("person@example.com").as_deref(),
            Some("person@example.com")
        );
    }

    #[test]
    fn generated_header_inputs_remove_or_reject_controls() {
        assert_eq!(
            clean_one_line(Some("Subject\r\nInjected\0Value\u{7f}")),
            Some("Subject Injected Value".to_string())
        );
        assert_eq!(
            normalize_smtp_address("person@example.com\r\nBcc: attacker@example.com"),
            None
        );
        assert_eq!(normalize_smtp_address("person\0@example.com"), None);
        assert_eq!(
            normalize_smtp_address("person@example.com>,<attacker@example.com"),
            None
        );
        assert_eq!(normalize_mime_type("application/pdf\r\nX-Evil: yes"), None);
        assert_eq!(normalize_mime_type("application/pdf\0"), None);
        assert_eq!(normalize_mime_type("application/pdf; name=evil"), None);
        assert_eq!(
            normalize_mime_type("application/vnd.ms-outlook").as_deref(),
            Some("application/vnd.ms-outlook")
        );
        assert_eq!(normalize_content_id("<image\r\nX-Evil: yes>"), None);
        assert_eq!(quote_header_param("report\u{7f}.pdf"), "report_.pdf");
        assert!(safe_header_value("safe value").is_ok());
        assert!(safe_header_value("unsafe\u{1f}value").is_err());
    }
}
