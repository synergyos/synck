use serde_json::Value;
use synck_vcs::types::{
    CreateItem, EventContent, EventProposal, ItemId, Sha256Digest, canonical_commit_bytes_for_hash,
    canonical_event_proposal_bytes, compute_sha256_digest_bytes, sha256_digest_to_digest_bytes,
    sha256_prefixed_hex_from_digest_bytes,
};
use uuid::Uuid;

#[derive(Clone)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        let seed = if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        };
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    fn next_bool(&mut self) -> bool {
        (self.next_u64() & 1) == 1
    }

    fn gen_usize(&mut self, max_exclusive: usize) -> usize {
        if max_exclusive == 0 {
            return 0;
        }
        (self.next_u64() as usize) % max_exclusive
    }
}

fn fuzz_iters() -> usize {
    std::env::var("SYNCK_VCS_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(500)
}

fn gen_uuid_v7(rng: &mut XorShift64) -> Uuid {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&rng.next_u64().to_be_bytes());
    bytes[8..].copy_from_slice(&rng.next_u64().to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn gen_item_id(rng: &mut XorShift64) -> ItemId {
    ItemId::parse(gen_uuid_v7(rng).to_string()).expect("generated ItemId must be UUIDv7")
}

fn gen_sha256_id(rng: &mut XorShift64) -> String {
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(8) {
        chunk.copy_from_slice(&rng.next_u64().to_be_bytes());
    }
    format!("sha256:{}", hex::encode(bytes))
}

fn gen_string(rng: &mut XorShift64, max_len: usize) -> String {
    const UNICODE: &[char] = &['é', 'ß', '中', 'Ω', 'λ', 'Ж', 'א', 'م'];
    let len = rng.gen_usize(max_len + 1);
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        let bucket = rng.gen_usize(6);
        let ch = match bucket {
            0 => (b'a' + (rng.gen_usize(26) as u8)) as char,
            1 => (b'A' + (rng.gen_usize(26) as u8)) as char,
            2 => (b'0' + (rng.gen_usize(10) as u8)) as char,
            3 => match rng.gen_usize(10) {
                0 => ' ',
                1 => '\n',
                2 => '\t',
                3 => '\r',
                4 => '"',
                5 => '\\',
                6 => '/',
                7 => '\u{0000}',
                8 => '\u{001f}',
                _ => '\u{007f}',
            },
            4 => UNICODE[rng.gen_usize(UNICODE.len())],
            _ => '-',
        };
        s.push(ch);
    }
    s
}

fn gen_json_number(rng: &mut XorShift64) -> serde_json::Number {
    const MAX_SAFE_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

    match rng.gen_usize(3) {
        0 => serde_json::Number::from(rng.next_u32() as i64 - (i32::MAX as i64)),
        1 => serde_json::Number::from(rng.next_u64() % (MAX_SAFE_JSON_INTEGER + 1)),
        _ => {
            let sign = if rng.next_bool() { 1.0 } else { -1.0 };
            let num = (rng.next_u32() % 1_000_000) as f64;
            let den = ((rng.next_u32() % 10_000) + 1) as f64;
            let exp = (rng.next_u32() % 81) as i32 - 40;
            let v = sign * (num / den) * 10f64.powi(exp);
            serde_json::Number::from_f64(v).unwrap_or_else(|| serde_json::Number::from(0))
        }
    }
}

fn gen_json_value(rng: &mut XorShift64, depth: u8) -> Value {
    if depth == 0 {
        return match rng.gen_usize(5) {
            0 => Value::Null,
            1 => Value::Bool(rng.next_bool()),
            2 => Value::Number(gen_json_number(rng)),
            3 => Value::String(gen_string(rng, 32)),
            _ => Value::Array(Vec::new()),
        };
    }

    match rng.gen_usize(7) {
        0 => Value::Null,
        1 => Value::Bool(rng.next_bool()),
        2 => Value::Number(gen_json_number(rng)),
        3 => Value::String(gen_string(rng, 64)),
        4 => {
            let n = rng.gen_usize(6);
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(gen_json_value(rng, depth - 1));
            }
            Value::Array(out)
        }
        _ => {
            let n = rng.gen_usize(6);
            let mut obj = serde_json::Map::new();
            for _ in 0..n {
                obj.insert(gen_string(rng, 16), gen_json_value(rng, depth - 1));
            }
            Value::Object(obj)
        }
    }
}

fn gen_event_content(rng: &mut XorShift64) -> EventContent {
    EventContent::bytes(synck_vcs::types::ObjectId::parse(gen_sha256_id(rng)).unwrap())
}

fn gen_event_proposal(rng: &mut XorShift64) -> EventProposal {
    let item_id = gen_item_id(rng);
    let create_item = if rng.next_bool() {
        Some(CreateItem {
            item_type: if rng.next_bool() {
                "FILE".to_string()
            } else {
                "FOLDER".to_string()
            },
            parent_folder_id: if rng.next_bool() {
                Some(gen_item_id(rng))
            } else {
                None
            },
            name: gen_string(rng, 64),
        })
    } else {
        None
    };

    EventProposal {
        item_id: Some(item_id),
        event_type: gen_string(rng, 32),
        set_metadata: if rng.next_bool() {
            Some(synck_vcs::metadata::MetadataRef::new(serde_json::json!({"value":gen_json_value(rng, 3)})).unwrap())
        } else {
            None
        },
        set_content: if rng.next_bool() {
            Some(gen_event_content(rng))
        } else {
            None
        },
        create_item,
        set_parent: match rng.gen_usize(3) {
            0 => None,
            1 => Some(None),
            _ => Some(Some(gen_item_id(rng))),
        },
        set_name: if rng.next_bool() {
            Some(gen_string(rng, 64))
        } else {
            None
        },
        set_tombstoned: if rng.next_bool() {
            Some(rng.next_bool())
        } else {
            None
        },
        set_in_trash: if rng.next_bool() {
            Some(rng.next_bool())
        } else {
            None
        },
        set_is_federated: None,
        set_federation_stream_id: None,
        base_seq: if rng.next_bool() {
            Some((rng.next_u32() % 10_000) as i64)
        } else {
            None
        },
    }
}

#[test]
fn canonical_event_bytes_roundtrip_is_idempotent() {
    let mut rng = XorShift64::new(0x7265_706c_6179_2d31);
    for _ in 0..fuzz_iters() {
        let ev = gen_event_proposal(&mut rng);
        let b1 = canonical_event_proposal_bytes(&ev).expect("canonicalize");
        let parsed: EventProposal = serde_json::from_slice(&b1).expect("parse canonical bytes");
        let b2 = canonical_event_proposal_bytes(&parsed).expect("re-canonicalize");
        assert_eq!(b1, b2);
    }
}

#[test]
fn canonical_json_value_roundtrip_is_idempotent() {
    let mut rng = XorShift64::new(0x7266_6338_3738_352d);
    for _ in 0..fuzz_iters().min(300) {
        let v = gen_json_value(&mut rng, 5);
        let b1 = serde_json_canonicalizer::to_vec(&v).expect("canonicalize");
        let v2: Value = serde_json::from_slice(&b1).expect("parse canonical json");
        let b2 = serde_json_canonicalizer::to_vec(&v2).expect("re-canonicalize");
        assert_eq!(b1, b2);
    }
}

#[test]
fn canonical_commit_bytes_are_canonical_json() {
    let mut rng = XorShift64::new(0x636f_6d6d_6974_2d31);
    for _ in 0..fuzz_iters().min(300) {
        let author_id = gen_string(&mut rng, 32);
        let message = if rng.next_bool() {
            Some(gen_string(&mut rng, 128))
        } else {
            None
        };

        let events_digest =
            Sha256Digest::parse(gen_sha256_id(&mut rng)).expect("events_digest must parse");
        let event_count = (rng.next_u32() % 100) as usize + 1;

        let b1 = canonical_commit_bytes_for_hash(
            &author_id,
            &author_id,
            None,
            message.as_deref(),
            None,
            event_count,
            &events_digest,
        )
        .expect("canonical commit bytes");

        let v: Value = serde_json::from_slice(&b1).expect("parse canonical commit json");
        let b2 = serde_json_canonicalizer::to_vec(&v).expect("canonicalize parsed json");
        assert_eq!(b1, b2);
    }
}

#[test]
fn json_number_canonicalization_is_idempotent_for_tricky_inputs() {
    let cases = [
        "0",
        "-0",
        "1",
        "-1",
        "1.0",
        "1.00",
        "1e0",
        "1E0",
        "1e+0",
        "1e-0",
        "0.1",
        "1e-20",
        "1e20",
        "9007199254740991",
        "18446744073709551615",
        "-9223372036854775808",
    ];

    for s in cases {
        let v: Value = serde_json::from_str(s).expect("parse json number");
        let b1 = serde_json_canonicalizer::to_vec(&v).expect("canonicalize");
        let v2: Value = serde_json::from_slice(&b1).expect("parse canonical");
        let b2 = serde_json_canonicalizer::to_vec(&v2).expect("re-canonicalize");
        assert_eq!(b1, b2);
    }
}

#[test]
fn canonical_event_digest_matches_payload_bytes() {
    let mut rng = XorShift64::new(0x6469_6765_7374_2d31);
    for _ in 0..fuzz_iters().min(300) {
        let ev = gen_event_proposal(&mut rng);
        let bytes = canonical_event_proposal_bytes(&ev).expect("canonical event bytes");
        let digest = compute_sha256_digest_bytes(&bytes);
        let id = Sha256Digest::parse(sha256_prefixed_hex_from_digest_bytes(digest))
            .expect("sha256 digest parse");
        assert_eq!(sha256_digest_to_digest_bytes(&id), digest);
    }
}
