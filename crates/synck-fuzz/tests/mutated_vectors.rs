//! Mutate the published vectors with a fixed seed, so every run is the same
//! short fuzz pass. `fuzz/` searches much further from the same starting points.

use std::path::Path;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;

const ROUNDS: usize = 400;

/// `SYNCK_MUTATION_ROUNDS` runs a longer pass.
fn rounds() -> usize {
    std::env::var("SYNCK_MUTATION_ROUNDS").map_or(ROUNDS, |rounds| rounds.parse().unwrap())
}

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

fn mutate(random: &mut XorShift64, seed: &[u8], splice: &[u8]) -> Vec<u8> {
    const INTERESTING: &[u8] = b"\x00\x01\x7f\x80\xff\"\\{}[],:0-\n";
    let mut data = seed.to_vec();
    for _ in 0..=random.below(4) {
        let at = random.below(data.len());
        match random.below(6) {
            0 if !data.is_empty() => data[at] ^= 1 << random.below(8),
            1 if !data.is_empty() => data[at] = INTERESTING[random.below(INTERESTING.len())],
            2 => data.truncate(at),
            3 if !data.is_empty() => {
                let end = (at + 1 + random.below(16)).min(data.len());
                data.drain(at..end);
            }
            4 if !data.is_empty() => {
                let end = (at + 1 + random.below(64)).min(data.len());
                let copy = data[at..end].to_vec();
                data.splice(at..at, copy);
            }
            _ => {
                let start = random.below(splice.len());
                let end = (start + random.below(64)).min(splice.len());
                data.splice(at..at, splice[start..end].iter().copied());
            }
        }
    }
    data
}

fn exercise(name: &str, check: fn(&[u8]), seeds: &[Vec<u8>]) {
    assert!(!seeds.is_empty(), "{name} has no seeds");
    let mut random = XorShift64(0x9e37_79b9_7f4a_7c15);
    let rounds = rounds();
    for seed in seeds {
        check(seed);
        for _ in 0..rounds {
            let splice = &seeds[random.below(seeds.len())];
            check(&mutate(&mut random, seed, splice));
        }
    }
}

fn vectors(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spec/vectors").join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn field_bytes(value: &Value, field: &str) -> Vec<Vec<u8>> {
    value
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|case| case[field].as_str())
        .map(|text| text.as_bytes().to_vec())
        .collect()
}

fn hex_bytes(value: &Value, field: &str) -> Vec<Vec<u8>> {
    field_bytes(value, field)
        .into_iter()
        .map(|text| {
            text.chunks(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect()
        })
        .collect()
}

#[test]
fn json_survives_mutation() {
    exercise("json", synck_fuzz::json, &field_bytes(&vectors("json.json"), "text"));
}

#[test]
fn bundles_survive_mutation() {
    exercise("bundle", synck_fuzz::bundle, &field_bytes(&vectors("bundle.json")["cases"], "bundle"));
}

#[test]
fn objects_survive_mutation() {
    let objects = hex_bytes(&vectors("objects.json"), "bytes_hex");
    exercise("object", synck_fuzz::object, &objects);
}

#[test]
fn metadata_survives_mutation() {
    let vector = vectors("metadata.json");
    exercise("metadata", synck_fuzz::metadata, &[vector["canonical_json"].as_str().unwrap().as_bytes().to_vec()]);
}

#[test]
fn state_nodes_survive_mutation() {
    let vector = vectors("objects.json");
    let mut seeds = hex_bytes(&vector, "payload_hex");
    seeds.push(vec![0; 64]);
    exercise("state node", synck_fuzz::state_node, &seeds);
}

#[test]
fn events_survive_mutation() {
    let vector = vectors("events.json");
    let seeds: Vec<Vec<u8>> = vector
        .as_array()
        .unwrap()
        .iter()
        .map(|case| serde_json::to_vec(&case["event"]).unwrap())
        .collect();
    exercise("event", synck_fuzz::event, &seeds);
}

#[test]
fn names_survive_mutation() {
    let vector = vectors("names.json");
    let mut seeds: Vec<Vec<u8>> = Vec::new();
    fn collect(value: &Value, seeds: &mut Vec<Vec<u8>>) {
        match value {
            Value::String(text) => seeds.push(text.as_bytes().to_vec()),
            Value::Array(values) => values.iter().for_each(|value| collect(value, seeds)),
            Value::Object(values) => values.values().for_each(|value| collect(value, seeds)),
            _ => {}
        }
    }
    collect(&vector, &mut seeds);
    exercise("name", synck_fuzz::name, &seeds);
}

fn fixture(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../synck-items/tests/fixtures/imported_email")
        .join(name);
    std::fs::read(path).unwrap()
}

#[test]
fn emails_survive_mutation() {
    exercise("eml", synck_fuzz::eml, &[fixture("representative.eml")]);
}

#[test]
fn outlook_messages_survive_mutation() {
    let seed = fixture("ascii.msg.b64");
    let seed: Vec<u8> = seed.into_iter().filter(|byte| !byte.is_ascii_whitespace()).collect();
    exercise("msg", synck_fuzz::msg, &[STANDARD.decode(seed).unwrap()]);
}

#[test]
fn chunks_survive_mutation() {
    let raw = [&5_u32.to_le_bytes()[..], b"hello"].concat();
    let zstd = [&(0x8000_0040_u32).to_le_bytes()[..], b"\x28\xb5\x2f\xfd\x20\x40\x45\x00\x00\x10"].concat();
    exercise("chunk", synck_fuzz::chunk, &[raw, zstd]);
}

#[test]
fn ooxml_survives_mutation() {
    // An empty archive, then the same one after a stored entry.
    let mut empty = b"PK\x05\x06".to_vec();
    empty.extend([0; 18]);
    let mut stored = b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x05\0\0\0".to_vec();
    stored.extend(b"a.xml");
    stored.extend(&empty);
    exercise("ooxml", synck_fuzz::ooxml, &[empty, stored]);
}
