use std::io::Write;
use synck_vcs::{
    object::{ObjectError, VerifyingWriter, plain_id},
    types::ObjectId,
};

#[test]
fn streamed_objects_verify_at_every_write_width() {
    let bytes = b"\0\xff\xfe payload";
    let id = plain_id(bytes);
    for width in 1..=bytes.len() {
        let mut output = Vec::new();
        let mut writer = VerifyingWriter::new(id.clone(), &mut output);
        for part in bytes.chunks(width) {
            writer.write_all(part).unwrap();
        }
        assert_eq!(writer.finish().unwrap(), bytes.len() as u64);
        assert_eq!(output, bytes);
    }
}

#[test]
fn streamed_objects_reject_wrong_digest_and_truncation() {
    let mut writer = VerifyingWriter::new(ObjectId::from_digest([0; 32]), std::io::sink());
    writer.write_all(b"payload").unwrap();
    assert!(matches!(writer.finish(), Err(ObjectError::Digest)));
    let mut writer = VerifyingWriter::new(plain_id(b"payload"), std::io::sink());
    writer.write_all(b"pay").unwrap();
    assert!(matches!(writer.finish(), Err(ObjectError::Digest)));
}
