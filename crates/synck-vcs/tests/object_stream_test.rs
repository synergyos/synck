use sha2::{Digest, Sha256};
use std::io::Write;
use synck_vcs::{
    object::{Codec, Decoder, Header, Object, ObjectError},
    types::ObjectId,
};

#[test]
fn streamed_objects_validate_every_boundary_and_preserve_raw_bytes() {
    for (codec, bytes) in [
        (Codec::Raw, b"\0\xff\xfe".as_slice()),
        (Codec::Utf8, "a🦀é\0\r\n".as_bytes()),
    ] {
        let object = Object::new(codec, bytes).unwrap();
        for width in 1..=object.payload().len() {
            let mut output = Vec::new();
            let mut decoder = Decoder::new(object.id(), &mut output);
            for part in object.payload().chunks(width) {
                decoder.write_all(part).unwrap();
            }
            assert_eq!(decoder.finish().unwrap(), (bytes.len() as u64, codec));
            assert_eq!(output, bytes);
        }
    }
}

#[test]
fn streamed_objects_reject_truncation_bad_utf8_and_wrong_digest() {
    for bytes in [b"\xff".as_slice(), b"\xf0\x9f", b"\xe2\x28\xa1"] {
        let mut payload = Header::new(Codec::Utf8).unwrap().as_bytes().to_vec();
        payload.extend_from_slice(bytes);
        let id = ObjectId::from_digest(Sha256::digest(&payload).into());
        let mut decoder = Decoder::new(id, std::io::sink());
        let result = payload
            .chunks(1)
            .try_for_each(|chunk| decoder.write_all(chunk));
        assert!(result.is_err() || decoder.finish().is_err());
    }
    let object = Object::new(Codec::Raw, b"payload").unwrap();
    let mut decoder = Decoder::new(ObjectId::from_digest([0; 32]), std::io::sink());
    decoder.write_all(object.payload()).unwrap();
    assert!(matches!(decoder.finish(), Err(ObjectError::Digest)));
    let mut decoder = Decoder::new(object.id(), std::io::sink());
    decoder.write_all(&object.payload()[..12]).unwrap();
    assert!(matches!(decoder.finish(), Err(ObjectError::Framing)));
}
