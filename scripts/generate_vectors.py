#!/usr/bin/env python3
"""Independent generator of the language-neutral vectors in docs/spec/vectors.

Usage: python3 -I scripts/generate_vectors.py [--check]

Uses only the Python standard library, `cryptography` for the ChaCha20 core, and
docs/spec/unicode-tables.json. It shares no code with the Rust engine. Hand-written
inputs stay in the vector files; every hash, digest and verdict is computed here
from format.md. With --check it writes nothing and fails if a vector would change.
"""

import copy
import datetime
import hashlib
import json
import re
import sqlite3
import sys
from pathlib import Path

from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305

ROOT = Path(__file__).resolve().parent.parent
SPEC = ROOT / "docs/spec"
VEC = SPEC / "vectors"
MAX_INT = 2**53 - 1
CHECK = sys.argv[1:] == ["--check"]
assert CHECK or not sys.argv[1:], __doc__
stale = []


def load(name):
    return json.loads((VEC / name).read_text(encoding="utf-8"))


def save(name, value):
    write(name, json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def write(name, text):
    path = VEC / name
    if not CHECK:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8", newline="\n")
    elif not path.exists() or path.read_text(encoding="utf-8") != text:
        stale.append(name)


def sha(data):
    return hashlib.sha256(data).digest()


def sha_id(data):
    return "sha256:" + sha(data).hex()


def jcs(value):
    """RFC 8785 for the integer-only subset."""
    if isinstance(value, dict):
        keys = sorted(value, key=lambda k: k.encode("utf-16-be"))
        return "{" + ",".join(jcs(k) + ":" + jcs(value[k]) for k in keys) + "}"
    if isinstance(value, list):
        return "[" + ",".join(jcs(v) for v in value) + "]"
    if isinstance(value, bool) or value is None or isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, int):
        return str(value)
    raise ValueError(f"not representable: {value!r}")


def jcs_bytes(value):
    return jcs(value).encode("utf-8")


SALT_PREFIX = b"SYNCK_OBJECT_V1\0"


def salted_utf8(salt, data):
    return SALT_PREFIX + b"\x01" + salt + data


def header(author, committer, occurred, message, count, digest):
    return {
        "object_type": "vcs_commit",
        "format_version": 6,
        "author_id": author,
        "committer_id": committer,
        "occurred_at": occurred,
        "message": message,
        "event_count": count,
        "events_digest": digest,
    }


def events_digest(items):
    """items: (family byte, raw 32-byte event digest)."""
    return sha_id(b"".join(bytes([family]) + digest for family, digest in items))


def checkpoint(previous, seq, commit_id, seconds, nanos):
    return sha(
        b"synck-checkpoint-v1"
        + previous
        + seq.to_bytes(8, "big")
        + bytes.fromhex(commit_id.removeprefix("sha256:"))
        + seconds.to_bytes(8, "big", signed=True)
        + nanos.to_bytes(4, "big")
    )


def genesis(repo_id, generation, encrypted=False):
    return sha(
        b"synck-genesis-v1"
        + uuid_bytes(repo_id)
        + uuid_bytes(generation)
        + (7).to_bytes(4, "big")
        + bytes([1 if encrypted else 0])
    )


# --- XChaCha20-Poly1305 ----------------------------------------------------


def _rotl(v, c):
    return ((v << c) & 0xFFFFFFFF) | (v >> (32 - c))


def _quarter(s, a, b, c, d):
    s[a] = (s[a] + s[b]) & 0xFFFFFFFF
    s[d] = _rotl(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & 0xFFFFFFFF
    s[b] = _rotl(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b]) & 0xFFFFFFFF
    s[d] = _rotl(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & 0xFFFFFFFF
    s[b] = _rotl(s[b] ^ s[c], 7)


def hchacha20(key, nonce16):
    s = [0x61707865, 0x3320646E, 0x79622D32, 0x6B206574]
    s += [int.from_bytes(key[i : i + 4], "little") for i in range(0, 32, 4)]
    s += [int.from_bytes(nonce16[i : i + 4], "little") for i in range(0, 16, 4)]
    for _ in range(10):
        _quarter(s, 0, 4, 8, 12)
        _quarter(s, 1, 5, 9, 13)
        _quarter(s, 2, 6, 10, 14)
        _quarter(s, 3, 7, 11, 15)
        _quarter(s, 0, 5, 10, 15)
        _quarter(s, 1, 6, 11, 12)
        _quarter(s, 2, 7, 8, 13)
        _quarter(s, 3, 4, 9, 14)
    return b"".join(v.to_bytes(4, "little") for v in s[:4] + s[12:])


def xchacha_seal(key, nonce, plaintext, aad):
    subkey = hchacha20(key, nonce[:16])
    return ChaCha20Poly1305(subkey).encrypt(b"\0\0\0\0" + nonce[16:], plaintext, aad)


def xchacha_self_test():
    """draft-irtf-cfrg-xchacha-03, appendix A.3."""
    plaintext = (
        b"Ladies and Gentlemen of the class of '99: If I could offer you only "
        b"one tip for the future, sunscreen would be it."
    )
    aad = bytes.fromhex("50515253c0c1c2c3c4c5c6c7")
    key = bytes(range(0x80, 0xA0))
    nonce = bytes(range(0x40, 0x58))
    expected = bytes.fromhex(
        "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb"
        "731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452"
        "2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9"
        "21f9664c97637da9768812f615c68b13b52e"
        "c0875924c1c7987947deafd8780acf49"
    )
    assert xchacha_seal(key, nonce, plaintext, aad) == expected


ENCRYPTED_PREFIX = b"SYNCK_ENCRYPTED\0"
AAD_PREFIX = b"synck-encrypted-v1"


def uuid_bytes(value):
    return bytes.fromhex(value.replace("-", ""))


def encrypted_aad(scheme, epoch, repo_id, item_id, role):
    raw = role.encode("ascii")
    return (
        AAD_PREFIX
        + scheme.to_bytes(2, "big")
        + epoch.to_bytes(8, "big")
        + uuid_bytes(repo_id)
        + uuid_bytes(item_id)
        + bytes([len(raw)])
        + raw
    )


def encrypted_object(scheme, epoch, key, nonce, repo_id, item_id, role, plaintext):
    aad = encrypted_aad(scheme, epoch, repo_id, item_id, role)
    head = ENCRYPTED_PREFIX + scheme.to_bytes(2, "big") + epoch.to_bytes(8, "big") + nonce
    return head + xchacha_seal(key, nonce, plaintext, aad), aad


# --- Unicode 18.0.0 --------------------------------------------------------


TABLES = json.loads((SPEC / "unicode-tables.json").read_text(encoding="utf-8"))
assert TABLES["unicode_version"] == "18.0.0"
FOLD = {row[0]: "".join(map(chr, row[1:])) for row in TABLES["case_folding"]}
UNASSIGNED = {c for low, high in TABLES["unassigned"] for c in range(low, high + 1)}
DECOMPOSE = {row[0]: row[1:] for row in TABLES["decompositions"]}
COMBINING = {
    c: value for low, high, value in TABLES["combining_classes"]
    for c in range(low, high + 1)
}
EXCLUDED = {c for low, high in TABLES["composition_exclusions"] for c in range(low, high + 1)}
COMPOSE = {tuple(v): c for c, v in DECOMPOSE.items() if len(v) == 2 and c not in EXCLUDED}
HANGUL_S, HANGUL_L, HANGUL_V, HANGUL_T = 0xAC00, 0x1100, 0x1161, 0x11A7
FORBIDDEN = set('<>:"/\\|?*') | {chr(c) for c in range(0x20)} | {"\x7f"}
FORBIDDEN |= {chr(c) for c in range(0x80, 0xA0)} | {"\u2028", "\u2029"}
FORBIDDEN |= {chr(c) for c in range(0x202A, 0x202F)} | {chr(c) for c in range(0x2066, 0x206A)}
IGNORED = {chr(c) for c in range(0x200C, 0x2010)} | {chr(c) for c in range(0x206A, 0x2070)}
IGNORED |= {"\ufeff"}
DEVICES = {"CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"}
DEVICES |= {f"COM{i}" for i in "123456789¹²³"} | {f"LPT{i}" for i in "123456789¹²³"}


def decompose(code):
    if HANGUL_S <= code < HANGUL_S + 11172:
        index = code - HANGUL_S
        tail = index % 28
        syllable = [HANGUL_L + index // 588, HANGUL_V + index % 588 // 28]
        return syllable + ([HANGUL_T + tail] if tail else [])
    if code in DECOMPOSE:
        return [c for part in DECOMPOSE[code] for c in decompose(part)]
    return [code]


def compose_pair(first, second):
    if HANGUL_L <= first < HANGUL_L + 19 and HANGUL_V <= second < HANGUL_V + 21:
        return HANGUL_S + ((first - HANGUL_L) * 21 + second - HANGUL_V) * 28
    if (
        HANGUL_S <= first < HANGUL_S + 11172 and (first - HANGUL_S) % 28 == 0
        and HANGUL_T < second < HANGUL_T + 28
    ):
        return first + second - HANGUL_T
    return COMPOSE.get((first, second))


def nfc(text):
    """Unicode 18.0.0 NFC. The language's own tables are an older version."""
    codes = [c for ch in text for c in decompose(ord(ch))]
    start = 0
    for i in range(1, len(codes) + 1):
        if i == len(codes) or COMBINING.get(codes[i], 0) == 0:
            codes[start:i] = sorted(codes[start:i], key=lambda c: COMBINING.get(c, 0))
            start = i
    result = []
    starter = None
    last = -1
    for code in codes:
        combining = COMBINING.get(code, 0)
        pair = None if starter is None or last >= combining and last != -1 else (
            compose_pair(result[starter], code)
        )
        if pair is not None:
            result[starter] = pair
            continue
        result.append(code)
        if combining == 0:
            starter = len(result) - 1
            last = -1
        else:
            last = combining
    return "".join(map(chr, result))


def fold(name):
    return "".join(FOLD.get(ord(c), c) for c in name)


def reserved(name):
    return "".join(c for c in fold(name) if c not in IGNORED) in (".synck", "synck~1")


def name_valid(name):
    if not name or len(name.encode("utf-8")) > 255 or name in (".", ".."):
        return False
    if nfc(name) != name:
        return False
    if any(c in FORBIDDEN or ord(c) in UNASSIGNED for c in name):
        return False
    if name[-1] in " .":
        return False
    if reserved(name):
        return False
    return name.split(".")[0].rstrip(" ").upper() not in DEVICES


# --- Vectors ---------------------------------------------------------------


def folder_event(ordinal):
    return {
        "item_id": f"019a2900-0000-7000-8000-{ordinal + 100:012x}",
        "event_type": "FOLDER::CREATE",
        "create_item": {"item_type": "FOLDER", "name": f"folder-{ordinal:05}"},
    }


def accepted_time(text):
    value = datetime.datetime.strptime(text[:19], "%Y-%m-%dT%H:%M:%S")
    seconds = int(value.replace(tzinfo=datetime.timezone.utc).timestamp())
    return seconds, int(text[20:-1].ljust(9, "0"))


def history_case(case, history):
    seconds, nanos = accepted_time(history["accepted_at"])
    meta = history["metadata"]
    repository = history["repository"]
    previous = genesis(repository["repo_id"], repository["generation"])
    ordinal = 0
    commits = []
    for index, count in enumerate(case["event_counts"]):
        pairs = []
        for _ in range(count):
            pairs.append((0, sha(jcs_bytes(folder_event(ordinal)))))
            ordinal += 1
        digest = events_digest(pairs)
        commit = header(
            meta["author_id"], meta["committer_id"], meta["occurred_at"],
            f"commit-{index}", count, digest,
        )
        commit_id = sha_id(jcs_bytes(commit))
        previous = checkpoint(previous, index + 1, commit_id, seconds, nanos)
        commits.append({
            "commit_id": commit_id,
            "events_digest": digest,
            "checkpoint": "sha256:" + previous.hex(),
        })
    return commits


def new_names():
    additions = [
        ("assigned in Unicode 18.0", "\ua7dd", True, "\ua7dd"),
        ("private use is assigned", "", True, ""),
        ("unassigned", "a͸b", False, "a_b"),
        ("noncharacter", "a￾b", False, "a_b"),
        ("last code point", "a\U0010ffffb", False, "a_b"),
        ("control introducer", "a\x9bb", False, "a_b"),
        ("next line", "a\x85b", False, "a_b"),
        ("first C1", "a\x80b", False, "a_b"),
        ("last C1", "a\x9fb", False, "a_b"),
        ("no-break space follows C1", "a\xa0b", True, "a\xa0b"),
        ("line separator", "a\u2028b", False, "a_b"),
        ("paragraph separator", "a\u2029b", False, "a_b"),
        ("embedding", "a\u202ab", False, "a_b"),
        ("override", "\u202ecod.exe", False, "_cod.exe"),
        ("first isolate", "a\u2066b", False, "a_b"),
        ("last isolate", "a\u2069b", False, "a_b"),
        ("left-to-right mark", "a\u200eb", True, "a\u200eb"),
        ("right-to-left mark", "a\u200fb", True, "a\u200fb"),
        ("control directory", ".synck", False, "_.synck"),
        ("control directory, case", ".SyNcK", False, "_.SyNcK"),
        ("control directory, folded long s", ".\u017fynck", False, "_.\u017fynck"),
        ("control directory, non-joiner", ".sy\u200cnck", False, "_.sy\u200cnck"),
        ("control directory, joiner at the end", ".synck\u200d", False, "_.synck\u200d"),
        ("control directory, byte order mark", "\ufeff.synck", False, "_\ufeff.synck"),
        ("control directory, deprecated format", ".syn\u206eck", False, "_.syn\u206eck"),
        ("control directory, trailing dot", ".synck.", False, "_.synck"),
        ("short name", "SYNCK~1", False, "_SYNCK~1"),
        ("short name, lowercase", "synck~1", False, "_synck~1"),
        ("short name, ignored code point", "sy\u200dnck~1", False, "_sy\u200dnck~1"),
        ("ignore file", ".synckignore", True, ".synckignore"),
        ("not a short name", "synck~2", True, "synck~2"),
        ("no dot", "synck", True, "synck"),
        ("control directory with suffix", ".synck (2)", True, ".synck (2)"),
    ]
    generated = {name for _, name, _, _ in additions}
    cases = [c for c in load("names.json")["cases"] if c["name"] not in generated]
    for case in cases:
        assert name_valid(case["name"]) == case["valid"], case["name"]
        if case["valid"]:
            assert fold(case["name"]) == case["key"], case["name"]
    for label, name, valid, fixed in additions:
        assert name_valid(name) == valid, label
        assert name_valid(fixed), label
        cases.append({
            "name": name,
            "valid": valid,
            "key": fold(name) if valid else None,
            "fixed": fixed,
        })
    save("names.json", {"unicode_version": "18.0.0", "cases": cases})


def new_json():
    cases = [
        ("zero", "0", True),
        ("largest integer", str(MAX_INT), True),
        ("smallest integer", str(-MAX_INT), True),
        ("above the limit", str(MAX_INT + 1), False),
        ("below the limit", str(-MAX_INT - 1), False),
        ("exact power of two", "18014398509481984", False),
        ("fraction", "1.5", False),
        ("trailing zero fraction", "1.0", False),
        ("exponent", "1e3", False),
        ("negative zero", "-0", False),
        ("nul escape in a string", '{"a":"x\\u0000y"}', False),
        ("nul escape in a key", '{"\\u0000":1}', False),
        ("other control escape", '{"a":"\\u0001"}', True),
        ("short escapes", '{"a":"\\b\\t\\n\\f\\r"}', True),
        ("unsorted keys", '{"b":1,"a":2}', False),
        ("sorted by UTF-16 code units", '{"\U00010000":1,"":2}', True),
        ("duplicate keys", '{"a":1,"a":2}', False),
        ("whitespace", '{"a": 1}', False),
        ("lone surrogate", '{"a":"\\ud800"}', False),
        ("uppercase hex escape", '{"a":"\\u001F"}', False),
        ("64 levels of arrays", "[" * 64 + "]" * 64, True),
        ("65 levels of arrays", "[" * 65 + "]" * 65, False),
        ("64 levels of objects", '{"a":' * 63 + "{}" + "}" * 63, True),
        ("65 levels of objects", '{"a":' * 64 + "{}" + "}" * 64, False),
        ("64 levels, mixed", '[{"a":' * 32 + "0" + "}]" * 32, True),
        ("65 levels, mixed", '[{"a":' * 32 + "[]" + "}]" * 32, False),
    ]
    save("json.json", [
        {"name": name, "text": text, "valid": valid} for name, text, valid in cases
    ])


def new_objects():
    entries = []

    def plain(name, data):
        entries.append({
            "name": name, "form": "PLAIN", "valid": True, "bytes_hex": data.hex(),
            "payload_hex": data.hex(), "id": sha_id(data),
        })

    def salted(name, salt, data):
        payload = salted_utf8(salt, data)
        entries.append({
            "name": name, "form": "SALTED", "valid": True, "salt_hex": salt.hex(),
            "bytes_hex": data.hex(), "payload_hex": payload.hex(), "id": sha_id(payload),
        })

    def invalid(name, payload):
        entries.append({
            "name": name, "form": "SALTED", "valid": False,
            "payload_hex": payload.hex(), "id": sha_id(payload),
        })

    plain("empty plain", b"")
    plain("binary plain", bytes.fromhex("00ff0180"))
    plain("plain bytes that look like a salted frame", salted_utf8(bytes(32), b"x"))
    salted("exact utf8", bytes(range(31, -1, -1)), bytes.fromhex("2000c3a9f09fa6800d0a"))
    salted("empty utf8", bytes(32), b"")
    invalid("codec 0 is not defined", SALT_PREFIX + b"\x00" + bytes(32) + b"x")
    invalid("codec 2 is not defined", SALT_PREFIX + b"\x02" + bytes(32) + b"x")
    invalid("utf8 frame holds invalid UTF-8", SALT_PREFIX + b"\x01" + bytes(32) + b"\xff")
    invalid("frame shorter than its header", SALT_PREFIX + b"\x01" + bytes(31))
    save("objects.json", entries)


def new_encrypted():
    repo = "019a2900-0000-7000-8000-000000000001"
    item = "019a2900-0000-7000-8000-000000000003"
    key = bytes(range(0x80, 0xA0))
    nonce = bytes(range(0x40, 0x58))
    entries = []

    def valid(name, epoch, role, plaintext):
        payload, aad = encrypted_object(1, epoch, key, nonce, repo, item, role, plaintext)
        entries.append({
            "name": name, "frame_valid": True, "opens": True, "scheme": 1, "key_epoch": epoch,
            "key_hex": key.hex(), "nonce_hex": nonce.hex(), "repo_id": repo,
            "item_id": item, "role": role, "plaintext_hex": plaintext.hex(),
            "aad_hex": aad.hex(), "payload_hex": payload.hex(), "id": sha_id(payload),
        })

    valid("file bytes", 1, "bytes", bytes.fromhex("00ff0180"))
    valid("empty body", 3, "body", b"")
    valid("metadata", 2, "metadata", b'{"title":"Budget"}')
    valid("ingest key at epoch 0", 0, "source", b"From: a\r\n\r\nb")

    good = bytes.fromhex(entries[0]["payload_hex"])

    def invalid(name, change, frame_valid=True):
        vector = dict(entries[0], name=name, frame_valid=frame_valid, opens=False)
        vector.pop("aad_hex")
        vector.pop("plaintext_hex")
        vector.update(change)
        payload = bytes.fromhex(vector["payload_hex"])
        vector["id"] = sha_id(payload)
        entries.append(vector)

    invalid("wrong item", {"item_id": "019a2900-0000-7000-8000-000000000004"})
    invalid("wrong role", {"role": "body"})
    invalid("wrong epoch in the header", {"key_epoch": 2, "payload_hex": (
        good[:18] + (2).to_bytes(8, "big") + good[26:]).hex()})
    invalid("unknown scheme", {"scheme": 2, "payload_hex": (
        good[:16] + (2).to_bytes(2, "big") + good[18:]).hex()})
    invalid("scheme 0 is not defined", {"scheme": 0, "payload_hex": (
        good[:16] + (0).to_bytes(2, "big") + good[18:]).hex()}, False)
    invalid("truncated tag", {"payload_hex": good[:-1].hex()})
    invalid("no room for a tag", {"payload_hex": good[:66 - 1].hex()}, False)
    save("encrypted.json", entries)


def new_checkpoints():
    repo = "019a2900-0000-7000-8000-000000000001"
    generation = "019a2900-0000-7000-8000-000000000002"
    other = "019a2900-0000-7000-8000-000000000003"
    cases = [
        ("repository", repo, generation, False),
        ("encrypted repository", repo, generation, True),
        ("another repository", other, generation, False),
        ("another generation", repo, other, False),
    ]
    genesis_cases = [{
        "name": name, "repo_id": r, "generation": g, "format_version": 7,
        "encrypted": e, "value": genesis(r, g, e).hex(),
    } for name, r, g, e in cases]
    assert len({case["value"] for case in genesis_cases}) == len(cases)
    steps = load("checkpoints.json")["steps"]
    previous = genesis(repo, generation)
    for step in steps:
        step["previous"] = previous.hex()
        previous = checkpoint(
            previous, step["repo_seq"], step["commit_id"],
            step["accepted_seconds"], step["accepted_nanos"],
        )
        step["checkpoint"] = previous.hex()
    save("checkpoints.json", {"genesis": genesis_cases, "steps": steps})


def new_events():
    vectors = load("events.json")
    meta_salt = bytes([7]) * 32

    def finish(vector):
        event_bytes = jcs_bytes(vector["event"])
        digest = sha(event_bytes)
        vector["canonical_event"] = event_bytes.decode("utf-8")
        vector["event_digest"] = "sha256:" + digest.hex()
        vector["events_digest"] = events_digest([(0, digest)])
        old = vector["commit"]
        commit = header(
            old["author_id"], old["committer_id"], old["occurred_at"], old["message"],
            1, vector["events_digest"],
        )
        vector["commit"] = commit
        vector["canonical_commit"] = jcs(commit)
        vector["commit_id"] = sha_id(jcs_bytes(commit))
        return vector

    def set_metadata(vector, value, salt):
        payload = salted_utf8(salt, jcs_bytes(value))
        vector["metadata"] = {
            "value": value, "payload_hex": payload.hex(), "id": sha_id(payload),
        }
        vector["event"]["metadata_id"] = sha_id(payload)

    for vector in vectors:
        if "metadata" in vector:
            meta = vector["metadata"]
            payload = bytes.fromhex(meta["payload_hex"])
            assert payload[49:] == jcs_bytes(meta["value"]), vector["name"]
            assert sha_id(payload) == meta["id"]
        finish(vector)

    template = {"commit": {
        "author_id": "human", "committer_id": "human", "occurred_at": None, "message": None,
    }}
    folder = "01890c6c-3333-7777-8888-cccccccccccc"
    record = "01890c6c-4444-7777-8888-dddddddddddd"
    body = "sha256:" + "2a" * 32

    def add(name, event, metadata=None):
        vectors[:] = [v for v in vectors if v["name"] != name]
        vector = copy.deepcopy(template)
        vector["name"] = name
        vector["event"] = event
        if metadata is not None:
            set_metadata(vector, metadata, bytes([9]) * 32)
        vectors.append(finish(vector))

    add("canonical_vector_tree_item_in_a_folder", {
        "item_id": record, "event_type": "FILE::CREATE",
        "create_item": {"item_type": "FILE", "name": "a.txt", "parent_folder_id": folder},
        "set_content": {"bytes": body},
    }, {"content": {"size": 1}})
    add("canonical_vector_record", {
        "item_id": record, "event_type": "TASK::CREATE",
        "create_item": {"item_type": "TASK"},
    })
    add("canonical_vector_record_with_key", {
        "item_id": record, "event_type": "CHAT::CREATE",
        "create_item": {"item_type": "CHAT", "key": "direct:" + "ab" * 32},
    })
    add("canonical_vector_record_with_link", {
        "item_id": record, "event_type": "EMAIL::CREATE",
        "create_item": {"item_type": "EMAIL", "link": folder},
        "set_content": {"source": body},
    })
    add("canonical_vector_set_link", {
        "item_id": record, "base_seq": 3, "event_type": "EMAIL::SET_LINK", "set_link": folder,
    })
    add("canonical_vector_clear_link", {
        "item_id": record, "base_seq": 4, "event_type": "EMAIL::SET_LINK", "set_link": None,
    })
    add("canonical_vector_derived_from", {
        "item_id": record, "event_type": "TASK::CREATE",
        "create_item": {"item_type": "TASK",
                        "derived_from": {"item_id": folder, "item_seq": 7}},
    })
    add("canonical_vector_merged_from", {
        "item_id": record, "base_seq": 8, "event_type": "TASK::SET_CONTENT",
        "merged_from": {"item_id": folder, "item_seq": 9},
    })
    add("canonical_vector_set_title", {
        "item_id": record, "base_seq": 5, "event_type": "EMAIL::SET_TITLE",
    }, {"title": "2026-03-01 Budget"})
    save("events.json", vectors)


def new_commits():
    vectors = load("commits.json")
    for vector in vectors:
        pairs = []
        for event in vector["events"]:
            digest = sha(event["payload"].encode("utf-8"))
            event["digest"] = "sha256:" + digest.hex()
            pairs.append((event["family"], digest))
        old = vector["header"]
        vector["header"] = header(
            old["author_id"], old["committer_id"], old["occurred_at"], old["message"],
            old["event_count"], events_digest(pairs),
        )
        vector["canonical"] = jcs(vector["header"])
        vector["commit_id"] = sha_id(vector["canonical"].encode("utf-8"))
    save("commits.json", vectors)


def new_sync():
    history = load("sync-history.json")
    for name in ("many_small_commits", "starts_and_ends_mid_commit"):
        history[name]["commits"] = history_case(history[name], history)
    save("sync-history.json", history)
    for name in ("sync.json", "sync-large.json"):
        vector = load(name)
        if name == "sync.json":
            raw = jcs_bytes(vector["records"][0]["value"])
            vector["canonical_event_utf8"] = raw.decode("utf-8")
            vector["events_digest"] = events_digest([(0, sha(raw))])
            vector["request"]["events_digest"] = vector["events_digest"]
        vector["request_digest"] = sha_id(jcs_bytes(vector["request"]))
        meta = vector["request"]["metadata"]
        commit = header(
            meta["author_id"], meta["committer_id"], meta["occurred_at"], meta["message"],
            vector["request"]["event_count"], vector["events_digest"],
        )
        vector["commit_id"] = sha_id(jcs_bytes(commit))
        save(name, vector)


def new_file_version():
    vector = load("file-version.json")
    salt = bytes.fromhex(vector["metadata_payload_hex"])[17:49]
    payload = salted_utf8(salt, jcs_bytes(vector["metadata"]))
    vector["metadata_payload_hex"] = payload.hex()
    vector["event"]["metadata_id"] = sha_id(payload)
    event_bytes = jcs_bytes(vector["event"])
    vector["canonical_event"] = event_bytes.decode("utf-8")
    vector["event_digest"] = sha_id(event_bytes)
    vector["with_source_commit"] = file_version_with_source()
    save("file-version.json", vector)


def file_version_with_source():
    """A revision from an earlier commit is named by its commit and event index."""
    metadata = {
        "file_item_id": "01890f47-6a4c-7d8e-9f01-123456789abc",
        "version_id": "01890f47-6a4e-7f00-8123-3456789abcde",
        "source_item_seq": 2,
        "source_commit_id": "sha256:" + "3c" * 32,
        "source_commit_event_idx": 1,
        "kind": "MAJOR",
        "label_source": "EXTERNAL",
        "order_key": 2048,
        "operation_id": "named-label",
        "description": "Sent to the reviewer",
    }
    payload = salted_utf8(bytes(range(32, 64)), jcs_bytes(metadata))
    event = {
        "owner_item_id": metadata["file_item_id"],
        "event_type": "FILE_VERSION::CREATE",
        "base_ref_seq": 1,
        "metadata_id": sha_id(payload),
    }
    event_bytes = jcs_bytes(event)
    return {
        "metadata": metadata,
        "metadata_payload_hex": payload.hex(),
        "event": event,
        "canonical_event": event_bytes.decode("utf-8"),
        "event_digest": sha_id(event_bytes),
    }


def new_sync_wire():
    """Every JSON shape of protocol 1, and a proposal with head guards."""
    identity = {
        "repo_id": "019a2900-0000-7000-8000-000000000001",
        "generation": "019a2900-0000-7000-8000-000000000002",
    }
    first = "019a2900-0000-7000-8000-000000000100"
    second = "019a2900-0000-7000-8000-000000000101"
    accepted = "2026-10-28T00:00:00.123456789Z"
    genesis_value = genesis(identity["repo_id"], identity["generation"])
    start = {
        "repository": identity,
        "repo_seq": 0,
        "checkpoint": "sha256:" + genesis_value.hex(),
    }

    records = [
        {"kind": "item", "value": {
            "item_id": first, "base_seq": 3, "event_type": "FOLDER::SET_NAME",
            "set_name": "renamed",
        }},
        {"kind": "ref", "value": {
            "owner_item_id": second, "event_type": "FILE_VERSION::TOMBSTONE",
            "metadata_id": "sha256:" + "4d" * 32, "base_ref_seq": 2,
        }},
        {"kind": "guard", "value": {"item_id": first, "head_seq": 3}},
        {"kind": "guard", "value": {"item_id": second, "head_seq": 7}},
    ]
    item_payload = jcs_bytes(records[0]["value"])
    ref_payload = jcs_bytes(records[1]["value"])
    digest = events_digest([(0, sha(item_payload)), (1, sha(ref_payload))])
    guards = b"".join(sha(jcs_bytes(r["value"])) for r in records[2:])
    metadata = {
        "author_id": "owner", "committer_id": "device", "message": "Tidy",
        "occurred_at": None,
    }
    request = {
        "repository": identity, "operation_id": "op:guarded", "metadata": metadata,
        "event_count": 2, "events_digest": digest,
        "guard_count": 2, "guards_digest": sha_id(guards),
    }
    commit_id = sha_id(jcs_bytes(header("owner", "device", None, "Tidy", 2, digest)))
    request_digest = sha_id(jcs_bytes(request))

    seconds, nanos = accepted_time(accepted)
    tip = checkpoint(genesis_value, 1, commit_id, seconds, nanos)
    tip_id = "sha256:" + tip.hex()
    published = {"repository": identity, "repo_seq": 1, "checkpoint": tip_id}
    row = {
        "repo_id": identity["repo_id"], "commit_id": commit_id, "repo_seq": 1,
        "format_version": 6, "author_id": "owner", "committer_id": "device",
        "message": "Tidy", "occurred_at": None, "created_at": accepted,
        "event_count": 2, "events_digest": digest,
    }
    item_row = {
        "event_family": "item", "repo_id": identity["repo_id"], "item_id": first,
        "item_seq": 4, "event_type": "FOLDER::SET_NAME", "commit_id": commit_id,
        "commit_event_idx": 0, "repo_seq": 1, "payload": list(item_payload),
        "event_payload_digest": sha_id(item_payload), "created_at": accepted,
        "parent_folder_id": None, "parent_folder_id_before": None,
    }
    ref_row = {
        "event_family": "ref", "repo_id": identity["repo_id"], "owner_item_id": second,
        "ref_seq": 3, "event_type": "FILE_VERSION::TOMBSTONE", "commit_id": commit_id,
        "commit_event_idx": 1, "repo_seq": 1, "payload": list(ref_payload),
        "event_payload_digest": sha_id(ref_payload), "created_at": accepted,
    }
    fragment = {"header": {"commit": row, "checkpoint": tip_id}, "events": [item_row, ref_row]}
    done = {"completed": start, "pending": None}
    receipt = {
        "operation_id": "op:guarded", "request_digest": request_digest,
        "commit_id": commit_id, "accepted_at": accepted, "published": published,
    }
    capabilities = {
        "protocol_version": 1, "repository": identity, "encrypted": False, "latest": published,
        "policy": {"max_events_per_commit": 500, "max_object_bytes": None, "quota_bytes": None},
    }
    valid = [
        ("Capabilities", capabilities),
        ("ProposalPage", {
            "repository": identity, "operation_id": "op:guarded", "offset": 0,
            "records": records,
        }),
        ("PageAck", {"next_offset": 4, "expires_at": accepted}),
        ("SubmitRequest", request),
        ("SubmitResult", {"status": "accepted", "receipt": receipt}),
        ("SubmitResult", {"status": "conflict", "items": [
            {"item_id": first, "head_seq": 5}, {"item_id": second, "head_seq": 0},
        ]}),
        ("SubmitResult", {
            "status": "POLICY_LIMIT", "limit": "events_per_commit", "maximum": 500,
            "actual": 1200,
        }),
        ("Receipt", dict(receipt, published=None)),
        ("HistoryRequest", {"after": done, "limit": 512}),
        ("HistoryRequest", {"after": {
            "completed": start, "pending": {"commit_id": commit_id, "next_event_index": 1},
        }, "limit": 1}),
        ("HistoryPage", {"after": done, "fragments": [fragment], "has_more": False}),
    ]
    invalid = [
        ("Capabilities", dict(capabilities, protocol_version=2), "another protocol version"),
        ("Capabilities", dict(capabilities, extension=True), "unknown field"),
        ("ProposalPage", {
            "repository": identity, "operation_id": "op:guarded", "offset": 0,
            "records": [{"kind": "note", "value": {}}],
        }, "unknown record kind"),
        ("ProposalPage", {
            "repository": identity, "operation_id": "op:guarded", "offset": 0,
            "records": [{"kind": "guard", "value": dict(records[2]["value"], note="x")}],
        }, "unknown field in a guard"),
        ("PageAck", {"next_offset": 4, "expires_at": accepted, "extension": 1}, "unknown field"),
        ("SubmitRequest", dict(request, extension=True), "unknown field"),
        ("SubmitResult", {"status": "maybe"}, "unknown status"),
        ("SubmitResult", {
            "status": "POLICY_LIMIT", "limit": "events_per_commit", "maximum": 500,
            "actual": 1200, "extension": 1,
        }, "unknown field"),
        ("HistoryRequest", {"after": done, "limit": 0}, "limit below one"),
        ("HistoryRequest", {"after": done, "limit": 513}, "limit above a page"),
        ("HistoryPage", {
            "after": done, "has_more": False, "fragments": [
                dict(fragment, events=[dict(item_row, extension=1), ref_row]),
            ],
        }, "unknown field in an event row"),
        ("HistoryPage", {
            "after": done, "has_more": False, "fragments": [
                dict(fragment, header=dict(fragment["header"], commit=dict(row, extension=1))),
            ],
        }, "unknown field in a commit row"),
    ]
    save("sync-wire.json", {
        "description": "One JSON value for each protocol shape. A `valid` value parses and "
                       "serializes to the same JSON value; an `invalid` value is refused. "
                       "`guarded` is one proposal with two head guards.",
        "valid": [{"type": kind, "value": value} for kind, value in valid],
        "invalid": [
            {"type": kind, "value": value, "reason": reason} for kind, value, reason in invalid
        ],
        "guarded": {
            "request": request, "records": records, "canonical_events": [
                item_payload.decode("utf-8"), ref_payload.decode("utf-8"),
            ],
            "commit_id": commit_id, "request_digest": request_digest,
        },
    })


def new_repository():
    """A complete repository as a directory, with its rows as text."""
    repo = "019a2900-0000-7000-8000-000000000001"
    generation = "019a2900-0000-7000-8000-000000000002"
    docs = "019a2900-0000-7000-8000-000000000a00"
    notes = "019a2900-0000-7000-8000-000000000a01"
    scratch = "019a2900-0000-7000-8000-000000000a02"
    task = "019a2900-0000-7000-8000-000000000a03"
    blobs = {name: name.encode("utf-8") for name in ("one\n", "two\n", "scratch\n")}
    ids = {name: sha_id(data) for name, data in blobs.items()}
    stored = {}
    salt = iter(range(1, 256))

    def metadata(value):
        payload = salted_utf8(bytes([next(salt)]) * 32, jcs_bytes(value))
        stored[sha_id(payload)] = payload
        return sha_id(payload)

    def file_metadata(name):
        return metadata({"content": {"size": len(blobs[name])}})

    def label(version, item_seq, kind, order, operation, event_index, source=None):
        value = {
            "file_item_id": notes, "version_id": f"019a2900-0000-7000-8000-00000000b00{version}",
            "source_item_seq": item_seq, "source_commit_event_idx": event_index, "kind": kind,
            "label_source": "INTERNAL", "order_key": order, "operation_id": operation,
        }
        if source:
            value["source_commit_id"] = source
        return {
            "owner_item_id": notes, "event_type": "FILE_VERSION::CREATE",
            "base_ref_seq": version - 1, "metadata_id": metadata(value),
        }

    commits = [("Add the notes", [
        {"item_id": docs, "event_type": "FOLDER::CREATE",
         "create_item": {"item_type": "FOLDER", "name": "docs"}},
        {"item_id": notes, "event_type": "FILE::CREATE",
         "create_item": {"item_type": "FILE", "name": "notes.txt", "parent_folder_id": docs},
         "metadata_id": file_metadata("one\n"), "set_content": {"bytes": ids["one\n"]}},
    ], [label(1, 1, "MINOR", 1024, "first-label", 1)]), ("Change the notes", [
        {"item_id": notes, "base_seq": 1, "event_type": "FILE::SET_CONTENT",
         "metadata_id": file_metadata("two\n"), "set_content": {"bytes": ids["two\n"]}},
    ], [])]
    second_commit = None

    def identify(events, refs, message):
        pairs = [(0, sha(jcs_bytes(e))) for e in events] + [(1, sha(jcs_bytes(r))) for r in refs]
        digest = events_digest(pairs)
        return sha_id(jcs_bytes(header("owner", "owner", None, message, len(pairs), digest))), digest

    second_commit = identify(commits[1][1], commits[1][2], commits[1][0])[0]
    commits += [("Name the second revision", [], [
        label(2, 2, "MAJOR", 2048, "second-label", 0, second_commit),
    ]), ("Move the notes and file a task", [
        {"item_id": notes, "base_seq": 2, "event_type": "FILE::SET_PARENT", "set_parent": None},
        {"item_id": scratch, "event_type": "FILE::CREATE",
         "create_item": {"item_type": "FILE", "name": "scratch.txt"},
         "metadata_id": file_metadata("scratch\n"), "set_content": {"bytes": ids["scratch\n"]}},
        {"item_id": task, "event_type": "TASK::CREATE",
         "create_item": {"item_type": "TASK", "link": docs}},
    ], []), ("Trash the scratch file", [
        {"item_id": scratch, "base_seq": 1, "event_type": "FILE::TRASH", "set_in_trash": True},
    ], [])]

    def quote(text):
        return "NULL" if text is None else "'" + text.replace("'", "''") + "'"

    def blob(data):
        return "X'" + data.hex().upper() + "'"

    def stamp(index):
        return f"2026-10-28T00:00:{index:02d}.000000001Z"

    rows = {
        "commits": [], "events": [], "refs": [], "checkpoints": [],
    }
    items = {}
    created = {}
    previous = genesis(repo, generation)
    expected = []
    for index, (message, events, refs) in enumerate(commits, 1):
        at = stamp(index)
        commit_id, digest = identify(events, refs, message)
        pk = f"019a2900-0000-7000-8000-00000000c{index:03x}"
        replayed = replay_commit(items, {"events": events, "ref_events": refs, "message": message})
        rows["commits"].append(
            f"INSERT INTO vcs_commits VALUES({quote(repo)},{quote(pk)},"
            f"{blob(bytes.fromhex(commit_id[7:]))},{index},NULL,6,'owner','owner',{quote(message)},"
            f"NULL,{quote(at)},{len(events) + len(refs)},{blob(bytes.fromhex(digest[7:]))});"
        )
        for position, event in enumerate(events):
            payload = jcs_bytes(event)
            item_id = event["item_id"]
            created.setdefault(item_id, at)
            seq = items.get(item_id, {"seq": 0})["seq"] + 1
            before = items.get(item_id, {}).get("parent")
            after = replayed[item_id]["parent"]
            rows["events"].append(
                f"INSERT INTO vcs_events VALUES({quote(repo)},{quote(pk)},{position},"
                f"{quote(item_id)},{seq},{quote(event['event_type'])},"
                f"CAST({quote(payload.decode('utf-8'))} AS BLOB),{blob(sha(payload))},"
                f"{quote(at)},{quote(after)},"
                f"{quote(before if 'set_parent' in event else None)});"
            )
        for position, event in enumerate(refs, len(events)):
            payload = jcs_bytes(event)
            owner = event["owner_item_id"]
            rows["refs"].append(
                f"INSERT INTO vcs_ref_events VALUES({quote(repo)},{quote(pk)},{position},"
                f"{quote(owner)},{event['base_ref_seq'] + 1},{quote(event['event_type'])},"
                f"CAST({quote(payload.decode('utf-8'))} AS BLOB),{blob(sha(payload))},{quote(at)});"
            )
        items = replayed
        seconds, nanos = accepted_time(at)
        previous = checkpoint(previous, index, commit_id, seconds, nanos)
        rows["checkpoints"].append(
            f"INSERT INTO vcs_checkpoints VALUES({quote(repo)},{index},"
            f"{blob(bytes.fromhex(commit_id[7:]))},{blob(previous)});"
        )
        expected.append({
            "repo_seq": index, "commit_id": commit_id, "checkpoint": "sha256:" + previous.hex(),
            "state_digest": state_digest(items),
        })

    item_rows = [
        f"INSERT INTO vcs_items VALUES({quote(repo)},{quote(item_id)},{quote(item['type'])},"
        f"{quote(item['parent'])},{quote(item['name'])},"
        f"{quote(fold(item['name']) if item['tree'] else None)},{quote(item['key'])},"
        f"{quote(item['link'])},{item['seq']},{quote(item['life'])},{quote(created[item_id])},"
        f"'owner','LOCAL',NULL,{item['ref']});"
        for item_id, item in sorted(items.items())
    ]
    text = "\n".join([
        f"INSERT INTO vcs_history_generations VALUES({quote(repo)},{quote(generation)});",
        f"INSERT INTO vcs_repo_activity_heads VALUES({quote(repo)},{len(commits) + 1},"
        f"{quote(stamp(len(commits)))});",
        *(
            f"INSERT INTO vcs_metadata_objects VALUES({quote(repo)},"
            f"{blob(bytes.fromhex(object_id[7:]))},{blob(payload)});"
            for object_id, payload in sorted(stored.items())
        ),
        *rows["commits"], *rows["events"], *rows["refs"], *item_rows, *rows["checkpoints"],
    ]) + "\n"
    database = sqlite3.connect(":memory:")
    database.execute("PRAGMA foreign_keys = ON")
    database.executescript((SPEC / "history.sql").read_text(encoding="utf-8") + text)
    database.close()

    write("repository/rows.sql", text)
    write("repository/config.json", json.dumps({
        "format_version": 7, "repo_id": repo, "authority": "FOLLOWER", "storage_mode": "WHOLE_FILE",
    }, indent=2) + "\n")
    for name, data in blobs.items():
        write("repository/objects/" + ids[name][7:], data.decode("utf-8"))
    save("repository/expected.json", {
        "generation": generation,
        "commits": expected,
        "tree": {"notes.txt": ids["two\n"]},
        "folders": ["docs"],
    })


# --- Structural rules: a small reference replay ----------------------------

TREE_TYPES = {"FILE", "FOLDER", "FEDERATED_MOUNT"}
RECORD_TYPES = {
    "EMAIL", "TASK_LIST", "TASK", "CHAT", "LOCAL_COMPANION", "CALENDAR", "CALENDAR_EVENT",
    "CALL", "POST", "DATABASE", "TABLE", "TIME_SETTINGS", "TIME_BOOK", "TIME_ENTRY",
    "PROFILE", "CONTACT", "USER_STATE", "FEDERATION_SCOPE",
}


CORE_EVENTS = {
    "CREATE", "SET_NAME", "SET_PARENT", "SET_LINK", "SET_METADATA", "SET_CONTENT", "TRASH",
    "RESTORE", "TOMBSTONE", "FEDERATE",
}


class Invalid(Exception):
    pass


def check_item_type(kind):
    if len(kind) > 64 or kind in ("ITEM", "FILE_VERSION") or not re.fullmatch(r"[A-Z][A-Z0-9_]*", kind):
        raise Invalid("invalid item type")


def check_event_type(event_type, kind):
    word = r"[A-Z][A-Z0-9_.]*"
    if len(event_type) > 128 or not re.fullmatch(f"{word}(::{word})+", event_type):
        raise Invalid("invalid event type")
    family, rest = event_type.split("::", 1)
    if family == "ITEM" and rest in CORE_EVENTS:
        raise Invalid("second spelling of a core event")
    if family not in ("ITEM", kind):
        raise Invalid("event family does not match the item")


def state_digest(items):
    data = b"synck-state-v1\n"
    for item_id in sorted(items, key=uuid_bytes):
        item = items[item_id]
        data += jcs_bytes({
            "item_id": item_id,
            "item_type": item["type"],
            "parent_folder_id": item["parent"],
            "name": item["name"],
            "name_key": fold(item["name"]) if item["tree"] else None,
            "record_key": item["key"],
            "link_folder_id": item["link"],
            "content": item["content"],
            "head_seq": item["seq"],
            "ref_head_seq": item["ref"],
            "lifecycle_state": item["life"],
            "sequencing_mode": item["mode"],
            "federation_stream_id": item["stream"],
        }) + b"\n"
    return sha_id(data)


def replay_commit(items, commit):
    if commit.get("message") == "":
        raise Invalid("empty message")
    items = copy.deepcopy(items)
    heads = {i: v["seq"] for i, v in items.items()}
    changed_links = set()
    for event in commit["events"]:
        item_id = event["item_id"]
        create = event.get("create_item")
        source = event.get("merged_from") or (create or {}).get("derived_from")
        if create is not None and "merged_from" in event:
            raise Invalid("merged_from on a created item")
        if source is not None:
            if set(source) != {"item_id", "item_seq"}:
                raise Invalid("unknown source field")
            if source["item_id"] == item_id:
                raise Invalid("an item is not its own source")
            if not 1 <= source["item_seq"] <= heads.get(source["item_id"], 0):
                raise Invalid("source revision does not exist before this commit")
            mode = items.get(item_id, {}).get("mode")
            if mode == "FEDERATED" or event.get("set_is_federated") is True:
                raise Invalid("a federated event has no source revision")
        if create is not None:
            if item_id in items:
                raise Invalid("created twice")
            kind = create["item_type"]
            check_item_type(kind)
            if set(create) - {"item_type", "name", "parent_folder_id", "key", "link",
                              "derived_from"}:
                raise Invalid("unknown create field")
            tree = "name" in create
            if tree and ("key" in create or "link" in create):
                raise Invalid("tree item with key or link")
            if not tree and "parent_folder_id" in create:
                raise Invalid("record with parent")
            if tree and kind in RECORD_TYPES:
                raise Invalid("record type in tree form")
            if not tree and kind in TREE_TYPES:
                raise Invalid("tree type in record form")
            if kind == "FILE" and "bytes" not in event.get("set_content", {}):
                raise Invalid("FILE created without bytes")
            if "key" in create and not create["key"]:
                raise Invalid("empty key")
            if tree and not name_valid(create["name"]):
                raise Invalid("invalid name")
            items[item_id] = {
                "type": kind, "tree": tree, "name": create.get("name"),
                "parent": create.get("parent_folder_id"), "key": create.get("key"),
                "link": create.get("link"), "life": "LIVE", "mode": "LOCAL", "seq": 1,
                "ref": 0, "content": {}, "stream": None,
            }
            if "link" in create:
                changed_links.add(item_id)
        else:
            if item_id not in items:
                raise Invalid("unknown item")
            item = items[item_id]
            if item["life"] == "TOMBSTONED":
                raise Invalid("event after tombstone")
            if event.get("base_seq") != heads.get(item_id):
                raise Invalid("wrong base")
            item["seq"] += 1
        item = items[item_id]
        check_event_type(event["event_type"], item["type"])
        if create is not None and event["event_type"].startswith("ITEM::"):
            raise Invalid("an ITEM event does not create")
        if item["tree"] and "set_link" in event:
            raise Invalid("set_link on a tree item")
        if not item["tree"] and ("set_name" in event or "set_parent" in event):
            raise Invalid("set_name or set_parent on a record")
        if "set_name" in event:
            if not name_valid(event["set_name"]):
                raise Invalid("invalid name")
            item["name"] = event["set_name"]
        if "set_parent" in event:
            item["parent"] = event["set_parent"]
        if "set_link" in event:
            item["link"] = event["set_link"]
            changed_links.add(item_id)
        if "set_tombstoned" in event:
            if not event["set_tombstoned"]:
                raise Invalid("a tombstone is permanent")
            item["life"] = "TOMBSTONED"
        if "set_in_trash" in event:
            if event["set_in_trash"] and item["life"] == "LIVE":
                item["life"] = "TRASH"
            if not event["set_in_trash"] and item["life"] == "TRASH":
                item["life"] = "LIVE"
        if "set_content" in event and event["event_type"].count("::") == 1:
            if item["type"] == "FILE" and set(event["set_content"]) != {"bytes"}:
                raise Invalid("a FILE's content is exactly its bytes")
            if item["type"] == "FOLDER":
                raise Invalid("a FOLDER has no content")
            item["content"] = event["set_content"]
        if event.get("set_is_federated") is True:
            item["mode"] = "FEDERATED"
            item["stream"] = event["set_federation_stream_id"]
        if event.get("set_is_federated") is False:
            item["mode"] = "LOCAL"
            item["stream"] = None

    ref_heads = {i: v["ref"] for i, v in items.items()}
    for event in commit.get("ref_events", []):
        owner = event["owner_item_id"]
        if owner not in items:
            raise Invalid("ref without an owner")
        if items[owner]["life"] == "TOMBSTONED":
            raise Invalid("ref on a tombstoned owner")
        if event["base_ref_seq"] != ref_heads[owner]:
            raise Invalid("wrong ref base")
        items[owner]["ref"] += 1

    names, keys, streams = set(), set(), set()
    for item_id, item in items.items():
        if item["stream"] is not None:
            if item["stream"] in streams:
                raise Invalid("federation stream conflict")
            streams.add(item["stream"])
        if item["parent"] is not None:
            parent = items.get(item["parent"])
            if parent is None or parent["type"] != "FOLDER":
                raise Invalid("parent is not a folder")
        if item["life"] == "LIVE" and item["tree"]:
            name_key = (item["parent"], fold(item["name"]))
            if name_key in names:
                raise Invalid("sibling name conflict")
            names.add(name_key)
        if item["life"] == "LIVE" and item["key"] is not None:
            record_key = (item["type"], item["key"])
            if record_key in keys:
                raise Invalid("record key conflict")
            keys.add(record_key)
        if item["tree"] or item["life"] == "TOMBSTONED" or item["link"] is None:
            continue
        target = items.get(item["link"])
        if target is None or target["type"] != "FOLDER":
            raise Invalid("link target is not a folder")
        if target["life"] == "TOMBSTONED":
            raise Invalid("link to a tombstoned folder")
        if item_id in changed_links and target["life"] != "LIVE":
            raise Invalid("link set to a folder that is not live")
        if item["mode"] == "FEDERATED":
            raise Invalid("federated record with a link")
    return items


def new_records():
    ids = {name: f"019a2900-0000-7000-8000-{n:012x}" for n, name in enumerate(
        ["f1", "f2", "r1", "r2", "r3", "c1"], 0x200)}
    content = "sha256:" + "ab" * 32

    def folder(name, item="f1", parent=None):
        create = {"item_type": "FOLDER", "name": name}
        if parent:
            create["parent_folder_id"] = ids[parent]
        return {"item_id": ids[item], "event_type": "FOLDER::CREATE", "create_item": create}

    def record(item, kind="EMAIL", **fields):
        return {
            "item_id": ids[item], "event_type": f"{kind}::CREATE",
            "create_item": dict({"item_type": kind}, **fields),
        }

    def update(item, base, kind, **fields):
        return dict({"item_id": ids[item], "base_seq": base, "event_type": kind}, **fields)

    def family(item):
        return "FOLDER" if item.startswith("f") else "EMAIL"

    def trash(item, base, kind=None):
        return update(item, base, f"{kind or family(item)}::TRASH", set_in_trash=True)

    def restore(item, base, kind=None):
        return update(item, base, f"{kind or family(item)}::RESTORE", set_in_trash=False)

    def tombstone(item, base, kind=None):
        return update(item, base, f"{kind or family(item)}::TOMBSTONE", set_tombstoned=True)

    def tree(item, kind, name):
        return {
            "item_id": ids[item], "event_type": f"{kind}::CREATE",
            "create_item": {"item_type": kind, "name": name},
        }

    def typed(item, event_type):
        return {
            "item_id": ids[item], "event_type": event_type,
            "create_item": {"item_type": "TASK"},
        }

    def tree_in(item, kind, name, parent):
        return {
            "item_id": ids[item], "event_type": f"{kind}::CREATE",
            "create_item": {"item_type": kind, "name": name, "parent_folder_id": ids[parent]},
        }

    f1, f2 = ids["f1"], ids["f2"]
    other = "sha256:" + "cd" * 32
    file_item = {**tree("r1", "FILE", "a"), "set_content": {"bytes": content}}
    cases = [
        ("record without key or link", [([record("r1", "TASK")], True)]),
        ("record with name", [([{
            "item_id": ids["r1"], "event_type": "TASK::CREATE",
            "create_item": {"item_type": "TASK", "name": "a"},
        }], False)]),
        ("tree item without name", [([record("r1", "FOLDER")], False)]),
        ("record with parent", [([record("r1", "TASK", parent_folder_id=f1)], False)]),
        ("tree item with key", [([{
            "item_id": ids["r1"], "event_type": "FOLDER::CREATE",
            "create_item": {"item_type": "FOLDER", "name": "a", "key": "k"},
        }], False)]),
        ("tree item with link", [([folder("F"), {
            "item_id": ids["r1"], "event_type": "FOLDER::CREATE",
            "create_item": {"item_type": "FOLDER", "name": "a", "link": f1},
        }], False)]),
        ("empty key", [([record("r1", "CHAT", key="")], False)]),
        ("file created with bytes", [([{
            "item_id": ids["r1"], "event_type": "FILE::CREATE",
            "create_item": {"item_type": "FILE", "name": "a"},
            "set_content": {"bytes": content},
        }], True)]),
        ("file created without bytes", [([{
            "item_id": ids["r1"], "event_type": "FILE::CREATE",
            "create_item": {"item_type": "FILE", "name": "a"},
        }], False)]),
        ("unknown type, either form", [
            ([record("r1", "FUTURE")], True),
            ([{"item_id": ids["r2"], "event_type": "FUTURE::CREATE",
               "create_item": {"item_type": "FUTURE", "name": "a"}}], True),
        ]),
        ("records do not use the name namespace", [
            ([folder("Same"), record("r1", "TASK"), record("r2", "TASK")], True),
        ]),
        ("empty commit message", [([record("r1", "TASK")], False, "")]),
        ("rename a record", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "TASK::SET_NAME", set_name="a")], False),
        ]),
        ("move a record", [
            ([folder("F"), record("r1", "TASK")], True),
            ([update("r1", 1, "TASK::SET_PARENT", set_parent=f1)], False),
        ]),
        ("link a tree item", [
            ([folder("F"), folder("G", "f2")], True),
            ([update("f2", 1, "FOLDER::SET_LINK", set_link=f1)], False),
        ]),
        ("link at creation to a folder of the same commit", [
            ([folder("F"), record("r1", link=f1)], True),
        ]),
        ("link to a missing item", [([record("r1", link=f2)], False)]),
        ("link to a record", [
            ([record("r1", "TASK_LIST")], True),
            ([record("r2", link=ids["r1"])], False),
        ]),
        ("move and clear a link", [
            ([folder("F"), folder("G", "f2"), record("r1", link=f1)], True),
            ([update("r1", 1, "EMAIL::SET_LINK", set_link=f2)], True),
            ([update("r1", 2, "EMAIL::SET_LINK", set_link=None)], True),
        ]),
        ("set a link to a folder in the trash", [
            ([folder("F"), record("r1")], True),
            ([trash("f1", 1)], True),
            ([update("r1", 1, "EMAIL::SET_LINK", set_link=f1)], False),
        ]),
        ("trash keeps a link", [
            ([folder("F"), record("r1", link=f1)], True),
            ([trash("f1", 1)], True),
            ([restore("f1", 2)], True),
        ]),
        ("trash a folder with its records in one commit", [
            ([folder("F"), record("r1", link=f1)], True),
            ([trash("f1", 1), trash("r1", 1)], True),
        ]),
        ("tombstone a folder that still has a link", [
            ([folder("F"), record("r1", link=f1)], True),
            ([tombstone("f1", 1)], False),
        ]),
        ("tombstone a folder and clear its link", [
            ([folder("F"), record("r1", link=f1)], True),
            ([tombstone("f1", 1), update("r1", 1, "EMAIL::SET_LINK", set_link=None)], True),
        ]),
        ("tombstone a folder and its record", [
            ([folder("F"), record("r1", link=f1)], True),
            ([tombstone("f1", 1), tombstone("r1", 1)], True),
        ]),
        ("tombstone a folder linked by a record in the trash", [
            ([folder("F"), record("r1", link=f1)], True),
            ([trash("r1", 1)], True),
            ([tombstone("f1", 1)], False),
        ]),
        ("two live records with one key", [
            ([record("r1", "CHAT", key="k"), record("r2", "CHAT", key="k")], False),
        ]),
        ("one key under two types", [
            ([record("r1", "CHAT", key="k"), record("r2", "POST", key="k")], True),
        ]),
        ("keys are exact bytes", [
            ([record("r1", "CHAT", key="k"), record("r2", "CHAT", key="K")], True),
        ]),
        ("reuse a key while the old record is in the trash", [
            ([record("r1", "CHAT", key="k")], True),
            ([trash("r1", 1, "CHAT")], True),
            ([record("r2", "CHAT", key="k")], True),
        ]),
        ("restore while another live record has the key", [
            ([record("r1", "CHAT", key="k")], True),
            ([trash("r1", 1, "CHAT")], True),
            ([record("r2", "CHAT", key="k")], True),
            ([restore("r1", 2, "CHAT")], False),
        ]),
        ("reuse a key after a tombstone", [
            ([record("r1", "CHAT", key="k")], True),
            ([tombstone("r1", 1, "CHAT")], True),
            ([record("r2", "CHAT", key="k")], True),
        ]),
        ("swap a key inside one commit", [
            ([record("r1", "CHAT", key="k")], True),
            ([trash("r1", 1, "CHAT"), record("r2", "CHAT", key="k")], True),
        ]),
        ("federate a linked record", [
            ([folder("F"), record("r1", "CHAT", link=f1)], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], False),
        ]),
        ("federate and move the link to a companion", [
            ([folder("F"), record("r1", "CHAT", link=f1)], True),
            ([update("r1", 1, "CHAT::SET_LINK", set_link=None),
              update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s"),
              record("c1", "LOCAL_COMPANION", key=ids["r1"], link=f1)], True),
        ]),
        ("link a federated record", [
            ([folder("F"), record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], True),
            ([update("r1", 2, "CHAT::SET_LINK", set_link=f1)], False),
        ]),
        ("subitem and dotted event types", [
            ([record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::MESSAGE::CREATE"),
              update("r1", 1, "CHAT::MESSAGE.ATTACHMENT::SET_METADATA")], True),
        ]),
        ("a family event on any item", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "ITEM::DATA_LINK::PUT")], True),
        ]),
        ("core event spelled with ITEM", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "ITEM::TRASH", set_in_trash=True)], False),
        ]),
        ("event of another family", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "POST::TRASH", set_in_trash=True)], False),
        ]),
        ("event type with one segment", [([typed("r1", "CREATE")], False)]),
        ("event type in lowercase", [([typed("r1", "TASK::create")], False)]),
        ("event type with an empty segment", [([typed("r1", "TASK::::CREATE")], False)]),
        ("event type at its limit", [
            ([typed("r1", "TASK::" + "A" * 122)], True),
        ]),
        ("event type over its limit", [
            ([typed("r1", "TASK::" + "A" * 123)], False),
        ]),
        ("item type at its limit", [
            ([record("r1", "T" * 64)], True),
        ]),
        ("item type over its limit", [([record("r1", "T" * 65)], False)]),
        ("item type in lowercase", [([record("r1", "Task")], False)]),
        ("ITEM is not an item type", [([record("r1", "ITEM")], False)]),
        ("reserved tree name", [([tree("r1", "FOLDER", ".synck")], False)]),
        ("reserved tree name in a subfolder", [
            ([folder("F"), {
                "item_id": ids["r1"], "event_type": "FOLDER::CREATE",
                "create_item": {"item_type": "FOLDER", "name": ".SYNCK", "parent_folder_id": f1},
            }], False),
        ]),
        ("rename to a short name", [
            ([folder("F")], True),
            ([update("f1", 1, "FOLDER::SET_NAME", set_name="synck~1")], False),
        ]),
        ("control character in a tree name", [
            ([tree("r1", "FOLDER", "a\x9bb")], False),
        ]),
        ("record keys are not tree names", [
            ([record("r1", "CHAT", key=".synck")], True),
        ]),
        ("ref ledger", [
            ([{**tree("r1", "FILE", "a"), "set_content": {"bytes": content}}], True),
            ({"events": [], "ref_events": [
                {"owner_item_id": ids["r1"], "base_ref_seq": 0, "event_type": "FILE_VERSION::CREATE"},
                {"owner_item_id": ids["r1"], "base_ref_seq": 0, "event_type": "FILE_VERSION::SET_KIND"},
            ]}, True),
        ]),
        ("a FILE always has bytes", [
            ([file_item], True),
            ([update("r1", 1, "FILE::SET_CONTENT", set_content={})], False),
        ]),
        ("a FILE has no other role", [
            ([file_item], True),
            ([update("r1", 1, "FILE::SET_CONTENT", set_content={"body": other})], False),
        ]),
        ("a FOLDER has no content", [
            ([folder("F")], True),
            ([update("f1", 1, "ITEM::NOTE", set_content={"body": other})], False),
        ]),
        ("a subitem does not replace the content", [
            ([{**record("r1"), "set_content": {"source": content}}], True),
            ([update("r1", 1, "EMAIL::ATTACHMENT::CREATE", set_content={"source": other})], True),
        ]),
        ("a two-segment event replaces the whole content", [
            ([{**record("r1", "TASK"), "set_content": {"body": content}}], True),
            ([update("r1", 1, "TASK::SET_CONTENT", set_content={"notes": other})], True),
        ]),
        ("a tombstone keeps name, parent and content", [
            ([folder("F"), {**tree("r1", "FILE", "a"), "set_content": {"bytes": content}}], True),
            ([update("r1", 1, "FILE::SET_PARENT", set_parent=f1)], True),
            ([update("r1", 2, "FILE::TOMBSTONE", set_tombstoned=True)], True),
        ]),
        ("a tombstone is permanent", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "ITEM::NOTE", set_tombstoned=False)], False),
        ]),
        ("no event follows a tombstone in one commit", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "TASK::TOMBSTONE", set_tombstoned=True),
              update("r1", 1, "TASK::SET_METADATA")], False),
        ]),
        ("no ref event on a tombstoned owner", [
            ([file_item], True),
            ([update("r1", 1, "FILE::TOMBSTONE", set_tombstoned=True)], True),
            ({"events": [], "ref_events": [
                {"owner_item_id": ids["r1"], "base_ref_seq": 0, "event_type": "FILE_VERSION::CREATE"},
            ]}, False),
        ]),
        ("trash and restore change nothing when already there", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "TASK::RESTORE", set_in_trash=False)], True),
            ([update("r1", 2, "TASK::TRASH", set_in_trash=True)], True),
            ([update("r1", 3, "TASK::TRASH", set_in_trash=True)], True),
        ]),
        ("a folder in the trash may hold a live child", [
            ([folder("F"), tree_in("r1", "FOLDER", "a", "f1")], True),
            ([trash("f1", 1)], True),
        ]),
        ("a child may precede its parent in one commit", [
            ([tree_in("r1", "FOLDER", "a", "f1"), folder("F")], True),
        ]),
        ("parent is a record", [
            ([record("r1", "TASK"), tree_in("r2", "FOLDER", "a", "r1")], False),
        ]),
        ("parent is missing", [
            ([tree_in("r1", "FOLDER", "a", "f1")], False),
        ]),
        ("ledger names are not item types", [
            ([record("r1", "FILE_VERSION")], False),
        ]),
        ("create with an event of its family", [
            ([{"item_id": ids["r1"], "event_type": "TASK::IMPORT",
               "create_item": {"item_type": "TASK"}}], True),
        ]),
        ("ITEM events do not create", [
            ([{"item_id": ids["r1"], "event_type": "ITEM::NOTE",
               "create_item": {"item_type": "TASK"}}], False),
        ]),
        ("derived from an earlier revision", [
            ([record("r1", "TASK")], True),
            ([record("r2", "TASK", derived_from={"item_id": ids["r1"], "item_seq": 1})], True),
        ]),
        ("derived across item types", [
            ([record("r1", "EMAIL")], True),
            ([record("r2", "TASK", derived_from={"item_id": ids["r1"], "item_seq": 1})], True),
        ]),
        ("merged from an earlier revision, keeping trash and repeats", [
            ([record("r1", "TASK"), record("r2", "TASK")], True),
            ([update("r1", 1, "TASK::TRASH", set_in_trash=True),
              update("r2", 1, "TASK::NOTE",
                     merged_from={"item_id": ids["r1"], "item_seq": 1})], True),
            ([update("r2", 2, "TASK::NOTE",
                     merged_from={"item_id": ids["r1"], "item_seq": 2})], True),
        ]),
        ("source revision from the future", [
            ([record("r1", "TASK")], True),
            ([record("r2", "TASK", derived_from={"item_id": ids["r1"], "item_seq": 2})], False),
        ]),
        ("source revision zero", [
            ([record("r1", "TASK")], True),
            ([record("r2", "TASK", derived_from={"item_id": ids["r1"], "item_seq": 0})], False),
        ]),
        ("source from the same commit", [
            ([record("r1", "TASK"),
              record("r2", "TASK", derived_from={"item_id": ids["r1"], "item_seq": 1})], False),
        ]),
        ("source revision made by the same commit", [
            ([record("r1", "TASK"), record("r2", "TASK")], True),
            ([update("r1", 1, "TASK::NOTE"),
              update("r2", 1, "TASK::NOTE",
                     merged_from={"item_id": ids["r1"], "item_seq": 2})], False),
        ]),
        ("source from another repository", [
            ([record("r2", "TASK", derived_from={"item_id": ids["r1"], "item_seq": 1})], False),
        ]),
        ("an item is not its own source", [
            ([record("r1", "TASK")], True),
            ([update("r1", 1, "TASK::NOTE",
                     merged_from={"item_id": ids["r1"], "item_seq": 1})], False),
        ]),
        ("merged_from on a created item", [
            ([record("r1", "TASK")], True),
            ([dict(record("r2", "TASK"),
                   merged_from={"item_id": ids["r1"], "item_seq": 1})], False),
        ]),
        ("a federated event has no source", [
            ([record("r1", "TASK")], True),
            ([dict(record("r2", "TASK", derived_from={"item_id": ids["r1"], "item_seq": 1}),
                   set_is_federated=True, set_federation_stream_id="stream-1")], False),
        ]),
        ("federated bootstrap", [
            ([{"item_id": ids["r1"], "event_type": "TASK::CREATE",
               "create_item": {"item_type": "TASK"}, "set_is_federated": True,
               "set_federation_stream_id": "stream-1"},
              {"item_id": ids["r1"], "event_type": "TASK::FEDERATE", "set_is_federated": True,
               "set_federation_stream_id": "stream-1"}], True),
        ]),
        ("two items on one stream", [
            ([{"item_id": ids["r1"], "event_type": "TASK::CREATE",
               "create_item": {"item_type": "TASK"}, "set_is_federated": True,
               "set_federation_stream_id": "stream-1"},
              {"item_id": ids["r2"], "event_type": "TASK::CREATE",
               "create_item": {"item_type": "TASK"}, "set_is_federated": True,
               "set_federation_stream_id": "stream-1"}], False),
        ]),
    ]
    output = []
    for name, steps in cases:
        items = {}
        commits = []
        for step in steps:
            body, valid = step[0], step[1]
            commit = dict(body) if isinstance(body, dict) else {"events": body}
            commit["valid"] = valid
            if len(step) > 2:
                commit["message"] = step[2]
            try:
                items = replay_commit(items, commit)
                assert valid, f"{name}: accepted"
                commit["state_digest"] = state_digest(items)
            except Invalid as error:
                assert not valid, f"{name}: {error}"
                commit["reason"] = str(error)
            commits.append(commit)
        output.append({"name": name, "commits": commits})
    save("records.json", output)


def bundle_line(record):
    return json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n"


def bundle_end(lines, tip):
    body = "".join(lines)
    return bundle_line({
        "type": "synck_vcs_bundle_end",
        "record_count": sum(1 for line in lines if line.strip()),
        "checkpoint": tip,
        "digest": sha_id(body.encode("utf-8")),
    })


def bundle_valid(text, genesis_value):
    """The end-record rules alone: a reader checks these before accepting anything."""
    if not text.endswith("\n"):
        return False
    lines = text.split("\n")[:-1]
    try:
        end = json.loads(lines[-1])
    except ValueError:
        return False
    if end.get("type") != "synck_vcs_bundle_end":
        return False
    body = text[: len(text) - len(lines[-1]) - 1]
    records = [json.loads(line) for line in lines[:-1] if line.strip()]
    if any(record["type"] == "synck_vcs_bundle_end" for record in records):
        return False
    commits = [record for record in records if record["type"] == "commit"]
    tip = commits[-1]["checkpoint"] if commits else genesis_value
    times = {record["commit_id"]: accepted_time(record["created_at"]) for record in commits}
    if any(
        record["type"] == "event" and accepted_time(record["created_at"]) != times[record["commit_id"]]
        for record in records
    ):
        return False
    return (
        end["record_count"] == len(records)
        and end["checkpoint"] == tip
        and end["digest"] == sha_id(body.encode("utf-8"))
    )


def new_bundle():
    repo = "019a2900-0000-7000-8000-000000000001"
    generation = "019a2900-0000-7000-8000-000000000002"
    folder_id = "019a2900-0000-7000-8000-000000000010"
    file_id = "019a2900-0000-7000-8000-000000000011"
    domain = "019a2900-0000-7000-8000-000000000020"
    at = "2026-10-30T12:00:00.000000000Z"
    content = "sha256:" + "ab" * 32
    meta = salted_utf8(bytes([9]) * 32, jcs_bytes({"content": {"size": 1}}))
    head = bundle_line({
        "type": "synck_vcs_bundle_header", "bundle_format_version": 5, "repo_id": repo,
        "history_generation": generation, "encrypted": False, "exported_at": at,
        "schema": {"subsystem": "synck_vcs_sqlite", "schema_id": "core-v7"},
        "commit_format_version": 6,
    })
    first = {
        "item_id": folder_id, "event_type": "FOLDER::CREATE",
        "create_item": {"item_type": "FOLDER", "name": "Docs"},
    }
    second = {
        "item_id": file_id, "event_type": "FILE::CREATE",
        "create_item": {"item_type": "FILE", "name": "a.txt", "parent_folder_id": folder_id},
        "metadata_id": sha_id(meta), "set_content": {"bytes": content},
    }
    pairs = [(0, sha(jcs_bytes(first))), (0, sha(jcs_bytes(second)))]
    digest = events_digest(pairs)
    commit = header("owner", "device", None, "add a file", 2, digest)
    commit_id = sha_id(jcs_bytes(commit))
    seconds, nanos = accepted_time(at)
    base = genesis(repo, generation)
    tip = "sha256:" + checkpoint(base, 1, commit_id, seconds, nanos).hex()

    def event(index, value, parent):
        return bundle_line({
            "type": "event", "commit_id": commit_id, "repo_seq": 1, "commit_event_idx": index,
            "item_id": value["item_id"], "item_seq": 1, "event_type": value["event_type"],
            "payload_json": jcs(value), "event_payload_digest": "sha256:" + pairs[index][1].hex(),
            "created_at": at, "parent_folder_id": parent, "parent_folder_id_before": None,
        })

    def item(item_id, kind, parent, name):
        return bundle_line({
            "type": "item", "item_id": item_id, "item_type": kind, "parent_folder_id": parent,
            "name": name, "name_key": fold(name), "record_key": None, "link_folder_id": None,
            "head_seq": 1, "ref_head_seq": 0, "lifecycle_state": "LIVE", "created_at": at,
            "created_by": "owner", "sequencing_mode": "LOCAL", "federation_stream_id": None,
        })

    lines = [
        head,
        bundle_line({"type": "metadata", "object_id": sha_id(meta), "payload_hex": meta.hex()}),
        bundle_line({
            "type": "commit", "commit_id": commit_id, "checkpoint": tip, "repo_seq": 1,
            "operation_id": "op:1", "format_version": 6, "author_id": "owner",
            "committer_id": "device", "message": "add a file", "occurred_at": None,
            "created_at": at, "event_count": 2, "events_digest": digest,
        }),
        event(0, first, None),
        event(1, second, folder_id),
        item(folder_id, "FOLDER", None, "Docs"),
        item(file_id, "FILE", folder_id, "a.txt"),
        bundle_line({
            "type": "federation_domain", "domain_id": domain, "last_domain_seq": 7,
            "created_at": at, "updated_at": at,
        }),
    ]
    whole = "".join(lines) + bundle_end(lines, tip)
    end = bundle_end(lines, tip)
    genesis_value = "sha256:" + base.hex()

    def changed(field, value):
        record = json.loads(end)
        record[field] = value
        return "".join(lines) + bundle_line(record)

    edited = lines[-1].replace('"last_domain_seq":7', '"last_domain_seq":6')

    def event_time(value):
        retimed = lines[:4] + [lines[4].replace(at, value)] + lines[5:]
        return "".join(retimed) + bundle_end(retimed, tip)

    blank = lines[:3] + ["\n"] + lines[3:]
    cases = [
        ("complete", whole, True),
        ("empty history", head + bundle_end([head], genesis_value), True),
        ("blank body line", "".join(blank) + bundle_end(blank, tip), True),
        ("event spells the same instant another way", event_time("2026-10-30T12:00:00Z"), True),
        ("event time differs from its commit", event_time("2026-10-30T12:00:00.000000001Z"), False),
        ("no end record", "".join(lines), False),
        ("cut after the commit", "".join(lines[:3]), False),
        ("cut after the item section", "".join(lines[:7]), False),
        ("cut inside the end record", whole[:-20], False),
        ("no final line feed", whole[:-1], False),
        ("federation domain lost, end record kept", "".join(lines[:7]) + end, False),
        ("federation domain edited, end record kept", "".join(lines[:7]) + edited + end, False),
        ("wrong record count", changed("record_count", 7), False),
        ("wrong checkpoint", changed("checkpoint", genesis_value), False),
        ("wrong digest", changed("digest", "sha256:" + "00" * 32), False),
        ("record after the end", whole + lines[-1], False),
        ("blank line after the end", whole + "\n", False),
        ("two end records", whole + end, False),
    ]
    output = []
    for name, text, valid in cases:
        assert bundle_valid(text, genesis_value) == valid, name
        output.append({"name": name, "valid": valid, "bundle": text})
    save("bundle.json", {"genesis": genesis_value, "cases": output})


def main():
    xchacha_self_test()
    new_names()
    new_json()
    new_objects()
    new_encrypted()
    new_events()
    new_commits()
    new_sync()
    new_sync_wire()
    new_file_version()
    new_records()
    new_repository()
    new_checkpoints()
    new_bundle()
    if stale:
        sys.exit("stale vectors: " + ", ".join(stale))
    print("vectors match" if CHECK else "vectors written")


main()
