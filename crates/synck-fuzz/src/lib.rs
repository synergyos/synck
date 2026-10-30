//! One check for each format Synck reads from untrusted sources.
//!
//! A check returns on anything a parser refuses and panics only when a property
//! breaks, so a crash means a bug. The mutated vectors test runs these on stable
//! with every `cargo test`, and `fuzz/` runs them under libFuzzer.

#![forbid(unsafe_code)]

use std::io::Write;

use synck_items::imported_email::{
    ImportedEmailFormat, ImportedEmailLimits, parse_imported_email_owned,
};
use synck_vcs::bundle::BundleReader;
use synck_vcs::metadata::Metadata;
use synck_vcs::naming::{compute_name_key, portable_name, validate_display_name};
use synck_vcs::object::{EncryptedFrame, Header, Object, verify_encrypted_frame};
use synck_vcs::state::{decode_node, encode_node};
use synck_vcs::types::{
    EventProposal, RefEventProposal, canonical_event_proposal_bytes, canonical_json_bytes,
    canonical_ref_event_proposal_bytes,
};

/// Canonical JSON is a fixed point.
pub fn json(data: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Ok(canonical) = canonical_json_bytes(&value) else {
        return;
    };
    let reparsed: serde_json::Value =
        serde_json::from_slice(&canonical).expect("canonical JSON parses");
    let again = canonical_json_bytes(&reparsed).expect("canonical JSON stays valid");
    assert_eq!(again, canonical);
}

/// A bundle is read one record at a time and stops at its first error.
pub fn bundle(data: &[u8]) {
    let mut input = data;
    let mut reader = BundleReader::new(&mut input);
    if reader.read_header().is_ok() {
        while let Ok(Some(_)) = reader.read_record() {}
    }
}

/// Frames parse without panicking, and a sealed object verifies under its own ID.
pub fn object(data: &[u8]) {
    let _ = Header::parse(data);
    let _ = EncryptedFrame::parse(data);
    let _ = verify_encrypted_frame(data);
    if let Ok(object) = Object::new(data) {
        let verified = Object::verify(&object.id(), object.payload().to_vec())
            .expect("an object verifies under its own ID");
        assert_eq!(verified.bytes(), data);
    }
}

/// Metadata verifies under its own ID and keeps its value.
pub fn metadata(data: &[u8]) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Ok(metadata) = Metadata::new(value) else {
        return;
    };
    let verified = Metadata::verify(&metadata.id(), metadata.payload().to_vec())
        .expect("metadata verifies under its own ID");
    assert_eq!(verified.value(), metadata.value());
}

/// Each state node has exactly one encoding.
pub fn state_node(data: &[u8]) {
    if let Ok(node) = decode_node(data) {
        assert_eq!(encode_node(&node).expect("a decoded node encodes"), data);
    }
}

/// Parsed proposals reach their canonical encoding without panicking.
pub fn event(data: &[u8]) {
    if let Ok(event) = serde_json::from_slice::<EventProposal>(data) {
        let _ = canonical_event_proposal_bytes(&event);
        let _ = event.source_revision();
    }
    if let Ok(event) = serde_json::from_slice::<RefEventProposal>(data) {
        let _ = canonical_ref_event_proposal_bytes(&event);
    }
}

/// Name keys are stable, and a portable name is accepted.
pub fn name(data: &[u8]) {
    let Ok(name) = std::str::from_utf8(data) else {
        return;
    };
    let _ = validate_display_name(name);
    let _ = compute_name_key(name);
    let portable = portable_name(name);
    assert_eq!(portable_name(&portable), portable);
}

pub fn eml(data: &[u8]) {
    email(ImportedEmailFormat::Eml, data);
}

pub fn msg(data: &[u8]) {
    email(ImportedEmailFormat::Msg, data);
}

fn email(format: ImportedEmailFormat, data: &[u8]) {
    let limits = ImportedEmailLimits::for_source_limit_and_parser_concurrency(1 << 22, 1);
    let _ = parse_imported_email_owned(format, data.to_vec(), &limits);
}

/// The first four bytes pick the claimed size, and bit 31 the codec.
pub fn chunk(data: &[u8]) {
    let Some((header, payload)) = data.split_first_chunk::<4>() else {
        return;
    };
    let header = u32::from_le_bytes(*header);
    let codec = if header >> 31 == 0 { "RAW" } else { "ZSTD" };
    synck_local::fuzz_entry::decode_chunk(codec, i64::from(header & 0x7fff_ffff), payload);
}

/// OOXML region analysis takes a file, as it does for working files.
pub fn ooxml(data: &[u8]) {
    let mut file = tempfile().expect("create a temporary file");
    file.write_all(data).expect("write a temporary file");
    synck_local::fuzz_entry::ooxml_regions(file, data.len() as u64);
}

fn tempfile() -> std::io::Result<std::fs::File> {
    let path = std::env::temp_dir().join(format!(
        "synck-fuzz-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let file = std::fs::File::options().read(true).write(true).create(true).truncate(true).open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}
