#!/usr/bin/env python3
"""Independent generator of the language-neutral vectors in docs/spec/vectors.

Usage: python3 -I scripts/generate_vectors.py [--check]

Uses only the Python standard library, `cryptography` for the ChaCha20 core, and
docs/spec/unicode-tables.json. It shares no code with the Rust engine. Hand-written
inputs stay in the vector files; every hash, digest and verdict is computed here
from format.md. With --check it writes nothing and fails if a vector would change.
"""

import copy
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


# --- Binary encoding (format.md, "Binary encoding") ------------------------
#
# Encoders take the readable JSON form of a value and return its bytes. A strict
# decoder reads the bytes back, so every valid vector is checked to round-trip and
# every rejected one to be refused.

MAX_SEQ = 2**63 - 1


class Malformed(Exception):
    pass


def u8(value):
    return value.to_bytes(1, "big")


def u16(value):
    return value.to_bytes(2, "big")


def u32(value):
    return value.to_bytes(4, "big")


def u64(value):
    return value.to_bytes(8, "big")


def i64(value):
    return value.to_bytes(8, "big", signed=True)


def uuid_bytes(value):
    return bytes.fromhex(value.replace("-", ""))


def uuid_text(raw):
    h = raw.hex()
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:]}"


def digest_bytes(value):
    assert re.fullmatch(r"sha256:[0-9a-f]{64}", value), value
    return bytes.fromhex(value[7:])


def enc_str(text):
    raw = text.encode("utf-8")
    return u16(len(raw)) + raw


def enc_bool(value):
    assert isinstance(value, bool)
    return u8(int(value))


def enc_time(text):
    seconds, nanos = parse_time(text)
    return i64(seconds) + u32(nanos)


def enc_target(value):
    return b"\0" if value is None else b"\1" + uuid_bytes(value)


def enc_source(value):
    assert set(value) == {"item_id", "item_seq"}, value
    return uuid_bytes(value["item_id"]) + u64(value["item_seq"])


def enc_content(roles):
    """Each role's entry is {"id": object ID, "size": stored byte length}."""
    ordered = sorted(roles, key=lambda role: role.encode("utf-8"))
    for role in ordered:
        assert set(roles[role]) == {"id", "size"}, roles[role]
        assert 0 <= roles[role]["size"] <= MAX_SEQ, roles[role]
    return u8(len(ordered)) + b"".join(
        enc_str(r) + digest_bytes(roles[r]["id"]) + u64(roles[r]["size"]) for r in ordered)


def blob(data):
    return u32(len(data)) + data


def days_from_civil(year, month, day):
    year -= month <= 2
    era = year // 400
    yoe = year - era * 400
    doy = (153 * (month + (-3 if month > 2 else 9)) + 2) // 5 + day - 1
    return era * 146097 + yoe * 365 + yoe // 4 - yoe // 100 + doy - 719468


def civil_from_days(days):
    days += 719468
    era = days // 146097
    doe = days - era * 146097
    yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
    doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
    mp = (5 * doy + 2) // 153
    month = mp + (3 if mp < 10 else -9)
    return yoe + era * 400 + (month <= 2), month, doy - (153 * mp + 2) // 5 + 1


TIME_MIN = days_from_civil(0, 1, 1) * 86400
TIME_MAX = days_from_civil(10000, 1, 1) * 86400 - 1


def parse_time(text):
    match = re.fullmatch(r"(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)(?:\.(\d{1,9}))?Z", text)
    year, month, day, hour, minute, second = map(int, match.groups()[:6])
    seconds = days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second
    return seconds, int((match.group(7) or "").ljust(9, "0"))


def format_time(seconds, nanos):
    days, rest = divmod(seconds, 86400)
    year, month, day = civil_from_days(days)
    return (f"{year:04}-{month:02}-{day:02}T{rest // 3600:02}:{rest // 60 % 60:02}:"
            f"{rest % 60:02}.{nanos:09}Z")


class Reader:
    def __init__(self, data):
        self.data, self.pos = data, 0

    def take(self, n):
        if self.pos + n > len(self.data):
            raise Malformed("truncated")
        self.pos += n
        return self.data[self.pos - n:self.pos]

    def end(self):
        if self.pos != len(self.data):
            raise Malformed("trailing bytes")

    def at_end(self):
        return self.pos == len(self.data)

    def uint(self, width):
        return int.from_bytes(self.take(width), "big")

    def u64(self):
        value = self.uint(8)
        if value > MAX_SEQ:
            raise Malformed("u64 above 2^63 - 1")
        return value

    def i64(self):
        return int.from_bytes(self.take(8), "big", signed=True)

    def uuid(self):
        raw = self.take(16)
        if raw[6] >> 4 != 7 or raw[8] >> 6 != 2:
            raise Malformed("not a UUIDv7")
        return uuid_text(raw)

    def digest(self):
        return "sha256:" + self.take(32).hex()

    def str(self):
        raw = self.take(self.uint(2))
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            raise Malformed("invalid UTF-8") from None
        if "\0" in text:
            raise Malformed("U+0000 in a string")
        return text

    def bool(self):
        value = self.uint(1)
        if value > 1:
            raise Malformed("bool above 1")
        return value == 1

    def seq(self):
        value = self.uint(8)
        if not 1 <= value <= MAX_SEQ:
            raise Malformed("seq out of range")
        return value

    def time(self):
        seconds, nanos = self.i64(), self.uint(4)
        if nanos >= 10**9 or not TIME_MIN <= seconds <= TIME_MAX:
            raise Malformed("time out of range")
        return format_time(seconds, nanos)

    def target(self):
        tag = self.uint(1)
        if tag > 1:
            raise Malformed("target tag above 1")
        return self.uuid() if tag else None

    def source(self):
        return {"item_id": self.uuid(), "item_seq": self.seq()}

    def content(self):
        count = self.uint(1)
        if count > 32:
            raise Malformed("more than 32 roles")
        roles, last = {}, None
        for _ in range(count):
            role = self.str()
            if last is not None and role.encode("utf-8") <= last:
                raise Malformed("roles not in strictly increasing order")
            last = role.encode("utf-8")
            roles[role] = {"id": self.digest(), "size": self.u64()}
        return roles

    def blob(self):
        return self.take(self.uint(4))


def enc_create(create):
    if "name" in create:
        assert set(create) <= {"item_type", "name", "parent_folder_id", "derived_from"}, create
        flags = 2 * ("parent_folder_id" in create) | 8 * ("derived_from" in create)
        body = enc_str(create["name"])
        if "parent_folder_id" in create:
            body += uuid_bytes(create["parent_folder_id"])
    else:
        assert set(create) <= {"item_type", "link", "key", "derived_from"}, create
        flags = 1 | 2 * ("link" in create) | 4 * ("key" in create) | 8 * ("derived_from" in create)
        body = uuid_bytes(create["link"]) if "link" in create else b""
        if "key" in create:
            body += enc_str(create["key"])
    if "derived_from" in create:
        body += enc_source(create["derived_from"])
    return u8(flags) + enc_str(create["item_type"]) + body


def dec_create(r):
    flags = r.uint(1)
    create = {"item_type": None}
    if flags & 0xF0 or (not flags & 1 and flags & 4):
        raise Malformed("unknown create_item flag bit")
    create["item_type"] = r.str()
    if flags & 1:
        if flags & 2:
            create["link"] = r.uuid()
        if flags & 4:
            create["key"] = r.str()
    else:
        create["name"] = r.str()
        if flags & 2:
            create["parent_folder_id"] = r.uuid()
    if flags & 8:
        create["derived_from"] = r.source()
    return create


def enc_tombstoned(value):
    assert value is True
    return b""


def dec_stream(r):
    stream = r.str()
    if not stream:
        raise Malformed("empty federation stream")
    return stream


# Item envelope fields in flag-bit order: name, encoder, decoder.
ITEM_FIELDS = [
    ("create_item", enc_create, dec_create),
    ("base_seq", u64, Reader.seq),
    ("metadata_id", digest_bytes, Reader.digest),
    ("set_content", enc_content, Reader.content),
    ("set_parent", enc_target, Reader.target),
    ("set_name", enc_str, Reader.str),
    ("set_link", enc_target, Reader.target),
    ("merged_from", enc_source, Reader.source),
    ("set_in_trash", enc_bool, Reader.bool),
    ("set_tombstoned", enc_tombstoned, lambda r: True),
    ("set_is_federated", enc_bool, Reader.bool),
    ("set_federation_stream_id", enc_str, dec_stream),
]
REF_FIELDS = [
    ("metadata_id", digest_bytes, Reader.digest),
    ("set_content", enc_content, Reader.content),
]


def envelope(event):
    """The exact bytes of an item or attached-ref envelope given as JSON."""
    if "owner_item_id" in event:
        kind, fields, width = 2, REF_FIELDS, 1
        head = uuid_bytes(event["owner_item_id"]) + u64(event["base_ref_seq"])
        known = {"owner_item_id", "event_type", "base_ref_seq"}
    else:
        kind, fields, width = 1, ITEM_FIELDS, 2
        head = uuid_bytes(event["item_id"])
        known = {"item_id", "event_type"}
    if set(event) - known - {name for name, _, _ in fields}:
        raise ValueError(f"not encodable: {sorted(set(event) - known)}")
    flags, body = 0, b""
    for bit, (name, encode, _) in enumerate(fields):
        if name in event:
            flags |= 1 << bit
            body += encode(event[name])
    return u8(kind) + enc_str(event["event_type"]) + head + flags.to_bytes(width, "big") + body


def decode_envelope(data):
    r = Reader(data)
    kind = r.uint(1)
    if kind == 1:
        event = {"event_type": r.str(), "item_id": r.uuid()}
        fields, flags = ITEM_FIELDS, r.uint(2)
    elif kind == 2:
        event = {"event_type": r.str(), "owner_item_id": r.uuid(), "base_ref_seq": r.u64()}
        fields, flags = REF_FIELDS, r.uint(1)
    else:
        raise Malformed("wrong kind byte")
    if flags >> len(fields):
        raise Malformed("unknown flag bit")
    for bit, (name, _, decode) in enumerate(fields):
        if flags >> bit & 1:
            event[name] = decode(r)
    r.end()
    return event


def commit_header(header):
    """The exact bytes of a commit header given as JSON."""
    assert set(header) == {
        "author_id", "committer_id", "occurred_at", "message", "event_count", "events_digest",
    }, header
    flags = (header["occurred_at"] is not None) | 2 * (header["message"] is not None)
    data = u8(3) + enc_str(header["author_id"]) + enc_str(header["committer_id"]) + u8(flags)
    if header["occurred_at"] is not None:
        data += enc_time(header["occurred_at"])
    if header["message"] is not None:
        data += enc_str(header["message"])
    return data + u32(header["event_count"]) + digest_bytes(header["events_digest"])


def decode_header(data):
    r = Reader(data)
    if r.uint(1) != 3:
        raise Malformed("wrong kind byte")
    header = {"author_id": r.str(), "committer_id": r.str()}
    flags = r.uint(1)
    if flags >> 2:
        raise Malformed("unknown flag bit")
    header["occurred_at"] = r.time() if flags & 1 else None
    header["message"] = r.str() if flags & 2 else None
    header["event_count"] = r.uint(4)
    header["events_digest"] = r.digest()
    r.end()
    return header


def events_digest(envelopes, repo_seq=None):
    """SHA-256 of the event digests in index order; of the position for no events."""
    if not envelopes:
        return sha_id(u64(repo_seq))
    return sha_id(b"".join(sha(data) for data in envelopes))


def header(author, committer, occurred, message, envelopes, repo_seq=None):
    return {
        "author_id": author, "committer_id": committer, "occurred_at": occurred,
        "message": message, "event_count": len(envelopes),
        "events_digest": events_digest(envelopes, repo_seq),
    }


def checked_envelope(event):
    data = envelope(event)
    assert decode_envelope(data) == event, event
    return data


def checked_header(value):
    data = commit_header(value)
    assert decode_header(data) == value, value
    return data


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
        + u32(8)
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


def encrypted_object(epoch, key, nonce, repo_id, item_id, role, plaintext):
    """Scheme 2, the only scheme."""
    aad = encrypted_aad(2, epoch, repo_id, item_id, role)
    head = ENCRYPTED_PREFIX + u16(2) + u64(epoch) + nonce
    return head + segmented_seal(key, nonce, plaintext, aad), aad


SEGMENT = 65536


def segmented_seal(key, prefix, plaintext, aad):
    """XChaCha20-Poly1305 over 64 KiB segments."""
    assert len(prefix) == 19
    parts = [plaintext[i:i + SEGMENT] for i in range(0, len(plaintext), SEGMENT)] or [b""]
    return b"".join(
        xchacha_seal(key, prefix + i.to_bytes(4, "big") + bytes([i == len(parts) - 1]), part, aad)
        for i, part in enumerate(parts)
    )


def segmented_frame_valid(n):
    r = n % (SEGMENT + 16)
    return n >= 16 and (r == 0 or r >= 17 or n == 16)


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


def ascii_upper(text):
    """Python's upper() maps U+0131 to I, and the format folds ASCII only."""
    return "".join(c.upper() if c.isascii() else c for c in text)


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
    return ascii_upper(name.split(".")[0].rstrip(" ")) not in DEVICES


# --- Vectors ---------------------------------------------------------------


def folder_event(ordinal):
    return {
        "item_id": f"019a2900-0000-7000-8000-{ordinal + 100:012x}",
        "event_type": "FOLDER::CREATE",
        "create_item": {"item_type": "FOLDER", "name": f"folder-{ordinal:05}"},
    }


def history_case(case, history):
    seconds, nanos = parse_time(history["accepted_at"])
    meta = history["metadata"]
    repository = history["repository"]
    previous = genesis(repository["repo_id"], repository["generation"])
    ordinal = 0
    commits = []
    for index, count in enumerate(case["event_counts"]):
        envelopes = []
        for _ in range(count):
            envelopes.append(checked_envelope(folder_event(ordinal)))
            ordinal += 1
        commit = header(
            meta["author_id"], meta["committer_id"], meta["occurred_at"],
            f"commit-{index}", envelopes, index + 1,
        )
        data = checked_header(commit)
        commit_id = sha_id(data)
        previous = checkpoint(previous, index + 1, commit_id, seconds, nanos)
        commits.append({
            "commit_id": commit_id,
            "events_digest": commit["events_digest"],
            "header_hex": data.hex(),
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
        ("truncation exposes a device name", "CON" + " " * 300 + "x", False, "_CON"),
        ("truncation exposes the control directory", ".synck" + " " * 300 + "x", False, "_.synck"),
        ("truncation leaves nothing", " " * 300 + "x", False, "_"),
        ("device name prefix overflows", "CON." + "a" * 251, False, "_CON." + "a" * 250),
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
    nonce = bytes(range(0x40, 0x53))
    entries = []

    def valid(name, epoch, role, plaintext):
        payload, aad = encrypted_object(epoch, key, nonce, repo, item, role, plaintext)
        entries.append({
            "name": name, "frame_valid": True, "opens": True, "scheme": 2, "key_epoch": epoch,
            "key_hex": key.hex(), "nonce_hex": nonce.hex(), "repo_id": repo,
            "item_id": item, "role": role, "plaintext_hex": plaintext.hex(),
            "aad_hex": aad.hex(), "payload_hex": payload.hex(), "id": sha_id(payload),
        })

    valid("file bytes", 1, "bytes", bytes.fromhex("00ff0180"))
    valid("empty file", 1, "bytes", b"")
    valid("empty body", 3, "body", b"")
    valid("metadata", 2, "metadata", b'{"title":"Budget"}')
    valid("ingest key at epoch 0", 0, "source", b"From: a\r\n\r\nb")
    good = bytes.fromhex(entries[0]["payload_hex"])

    def invalid(name, payload, frame_valid=True, **change):
        vector = dict(entries[0], name=name, frame_valid=frame_valid, opens=False, **change)
        vector.pop("aad_hex")
        vector.pop("plaintext_hex")
        vector["payload_hex"] = payload.hex()
        vector["id"] = sha_id(payload)
        entries.append(vector)

    invalid("wrong item", good, item_id="019a2900-0000-7000-8000-000000000004")
    invalid("wrong role", good, role="body")
    invalid("wrong epoch in the header", good[:18] + u64(2) + good[26:], key_epoch=2)
    invalid("unknown scheme", good[:16] + u16(3) + good[18:], False, scheme=3)
    invalid("scheme 0 is not defined", good[:16] + u16(0) + good[18:], False, scheme=0)
    # Format 7's scheme 1: one XChaCha20-Poly1305 seal under a 24-byte nonce.
    nonce24 = bytes(range(0x40, 0x58))
    aad1 = encrypted_aad(1, 1, repo, item, "bytes")
    invalid("scheme 1 is not a scheme", ENCRYPTED_PREFIX + u16(1) + u64(1) + nonce24 + xchacha_seal(
        key, nonce24, bytes.fromhex("00ff0180"), aad1), False, scheme=1, nonce_hex=nonce24.hex())
    invalid("truncated tag", good[:-1])
    invalid("no room for a tag", good[:45 + 15], False)
    # A one-segment object whose segment claims not to be the last, or another index.
    aad = encrypted_aad(2, 1, repo, item, "bytes")
    invalid("segment not marked last", good[:45] + xchacha_seal(
        key, nonce + u32(0) + b"\0", bytes.fromhex("00ff0180"), aad))
    invalid("segment with the wrong index", good[:45] + xchacha_seal(
        key, nonce + u32(1) + b"\1", bytes.fromhex("00ff0180"), aad))
    for vector in entries:
        assert segmented_frame_valid(len(bytes.fromhex(vector["payload_hex"])) - 45) or (
            not vector["frame_valid"]), vector["name"]
    save("encrypted.json", entries)


def new_encrypted_segments():
    """Objects too large to list in hex, and the key-free length rule."""
    repo = "019a2900-0000-7000-8000-000000000001"
    item = "019a2900-0000-7000-8000-000000000003"
    key = bytes(range(0x80, 0xA0))
    nonce = bytes(range(0x40, 0x53))
    objects = []
    for name, length in [
        ("one full segment", SEGMENT),
        ("one byte into a second segment", SEGMENT + 1),
        ("two full segments and part of a third", 2 * SEGMENT + 300),
    ]:
        plaintext = bytes(i % 251 for i in range(length))
        payload, _ = encrypted_object(1, key, nonce, repo, item, "bytes", plaintext)
        objects.append({
            "name": name, "plaintext_length": length, "segments": -(-length // SEGMENT),
            "stored_length": len(payload), "id": sha_id(payload),
        })
    lengths = []
    for n in [0, 15, 16, 17, 32, SEGMENT + 16, SEGMENT + 17, SEGMENT + 31, SEGMENT + 32,
              SEGMENT + 33, 2 * (SEGMENT + 16), 2 * (SEGMENT + 16) + 1, 2 * (SEGMENT + 16) + 17]:
        valid = segmented_frame_valid(n)
        lengths.append({
            "ciphertext_length": n, "frame_valid": valid,
            "plaintext_length": n - 16 * -(-n // (SEGMENT + 16)) if valid else None,
        })
    save("encrypted-segments.json", {
        "key_hex": key.hex(), "nonce_hex": nonce.hex(), "repo_id": repo, "item_id": item,
        "role": "bytes", "key_epoch": 1,
        "plaintext": "byte i is i mod 251",
        "objects": objects, "lengths": lengths,
    })


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
        "name": name, "repo_id": r, "generation": g, "format_version": 8,
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


def refused(decode, data):
    try:
        decode(data)
    except Malformed:
        return data.hex()
    raise AssertionError(f"accepted: {data.hex()}")


def new_events():
    vectors = load("events.json")["valid"]

    def finish(vector):
        data = checked_envelope(vector["event"])
        vector["envelope_hex"] = data.hex()
        vector["event_digest"] = sha_id(data)
        old = vector["commit"]
        vector["commit"] = header(
            old["author_id"], old["committer_id"], old["occurred_at"], old["message"], [data],
        )
        vector["events_digest"] = vector["commit"]["events_digest"]
        head = checked_header(vector["commit"])
        vector["header_hex"] = head.hex()
        vector["commit_id"] = sha_id(head)
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
            assert sha_id(payload) == meta["id"] == vector["event"]["metadata_id"]
        for stored in vector.get("objects", []):
            payload = bytes.fromhex(stored["payload_hex"])
            assert sha_id(payload) == stored["id"], vector["name"]
            assert vector["event"]["set_content"][stored["role"]] == {
                "id": stored["id"], "size": len(payload)}, vector["name"]
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
        "set_content": {"bytes": {"id": body, "size": 1}},
    })
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
        "set_content": {"source": {"id": body, "size": 2048}},
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
    add("canonical_vector_trash", {
        "item_id": record, "base_seq": 6, "event_type": "EMAIL::TRASH", "set_in_trash": True,
    })
    add("canonical_vector_tombstone", {
        "item_id": record, "base_seq": 7, "event_type": "EMAIL::TOMBSTONE", "set_tombstoned": True,
    })
    add("canonical_vector_ref_with_content", {
        "owner_item_id": record, "event_type": "NOTE::PUT", "base_ref_seq": 5,
        "set_content": {"body": {"id": body, "size": 61},
                        "attachment": {"id": "sha256:" + "3b" * 32, "size": 2**63 - 1}},
    }, {"title": "Note"})
    assert all(decode_envelope(bytes.fromhex(v["envelope_hex"])) == v["event"] for v in vectors)

    # Rejected encodings, each one change away from an accepted envelope.
    item = uuid_bytes(record)
    digest = digest_bytes(body)

    def raw(event_type, flags, fields, item_id=item, kind=1):
        return u8(kind) + enc_str(event_type) + item_id + u16(flags) + fields

    def ref(flags, fields, base=u64(0)):
        return u8(2) + enc_str("FILE_VERSION::CREATE") + item + base + u8(flags) + fields

    def roles(*names, size=u64(1)):
        return u8(len(names)) + b"".join(enc_str(name) + digest + size for name in names)

    trash = raw("EMAIL::TRASH", 0x102, u64(6) + u8(1))
    assert decode_envelope(trash) == vectors[-3]["event"]
    federate = 1 << 1 | 1 << 10 | 1 << 11
    rejected = [
        ("trailing byte", trash + b"\0"),
        ("truncated", trash[:-1]),
        ("kind byte 0", u8(0) + trash[1:]),
        ("kind byte 3", u8(3) + trash[1:]),
        ("item flag bit 12", raw("EMAIL::TRASH", 0x1102, u64(6) + u8(1))),
        ("item flag bit 15", raw("EMAIL::TRASH", 0x8102, u64(6) + u8(1))),
        ("ref flag bit 2", ref(4, b"")),
        ("create_item flag bit 4", raw("TASK::CREATE", 1, u8(0x11) + enc_str("TASK"))),
        ("tree create_item with bit 2", raw(
            "FOLDER::CREATE", 1, u8(0x04) + enc_str("FOLDER") + enc_str("a") + enc_str("k"))),
        ("item ID is a UUIDv4", raw(
            "EMAIL::TRASH", 0x102, u64(6) + u8(1),
            uuid_bytes("01890c6c-4444-4777-8888-dddddddddddd"))),
        ("item ID has another variant", raw(
            "EMAIL::TRASH", 0x102, u64(6) + u8(1),
            uuid_bytes("01890c6c-4444-7777-c888-dddddddddddd"))),
        ("bool value 2", raw("EMAIL::TRASH", 0x102, u64(6) + u8(2))),
        ("base_seq 0", raw("EMAIL::TRASH", 0x102, u64(0) + u8(1))),
        ("base_seq 2^63", raw("EMAIL::TRASH", 0x102, u64(2**63) + u8(1))),
        ("base_ref_seq 2^63", ref(0, b"", u64(2**63))),
        ("source item_seq 0", raw("TASK::NOTE", 0x82, u64(1) + uuid_bytes(folder) + u64(0))),
        ("content roles out of order", raw(
            "EMAIL::SET_CONTENT", 0x0A, u64(1) + roles("source", "provider"))),
        ("content role repeated", raw("TASK::SET_CONTENT", 0x0A, u64(1) + roles("body", "body"))),
        ("33 content roles", raw(
            "TASK::SET_CONTENT", 0x0A, u64(1) + roles(*(f"r{i:02}" for i in range(33))))),
        ("content size 2^63", raw(
            "TASK::SET_CONTENT", 0x0A, u64(1) + roles("body", size=u64(2**63)))),
        ("content entry without its size", raw(
            "TASK::SET_CONTENT", 0x0A, u64(1) + roles("body", size=b""))),
        ("target tag 2", raw("FILE::SET_PARENT", 0x12, u64(1) + u8(2) + uuid_bytes(folder))),
        ("string holds U+0000", raw("FILE::SET_NAME", 0x22, u64(1) + u16(3) + b"a\0b")),
        ("string holds invalid UTF-8", raw("FILE::SET_NAME", 0x22, u64(1) + u16(2) + b"\xc3\x28")),
        ("string runs past the end", raw("FILE::SET_NAME", 0x22, u64(1) + u16(10) + b"abc")),
        ("empty federation stream", raw("TASK::FEDERATE", federate, u64(1) + u8(1) + u16(0))),
    ]
    save("events.json", {
        "valid": vectors,
        "invalid": [
            {"name": name, "envelope_hex": refused(decode_envelope, data)}
            for name, data in rejected
        ],
    })


def new_commits():
    vectors = load("commits.json")
    for vector in vectors:
        envelopes = []
        for event in vector["events"]:
            data = checked_envelope(event["event"])
            event["envelope_hex"] = data.hex()
            event["digest"] = sha_id(data)
            envelopes.append(data)
        old = vector["header"]
        vector["header"] = header(
            old["author_id"], old["committer_id"], old["occurred_at"], old["message"], envelopes,
        )
        data = checked_header(vector["header"])
        vector["header_hex"] = data.hex()
        vector["commit_id"] = sha_id(data)
    save("commits.json", vectors)


def new_sync():
    history = load("sync-history.json")
    for name in ("many_small_commits", "starts_and_ends_mid_commit"):
        history[name]["commits"] = history_case(history[name], history)
    save("sync-history.json", history)
    for name in ("sync.json", "sync-large.json"):
        vector = load(name)
        request = vector["request"]
        if name == "sync.json":
            for record in vector["records"]:
                record["envelope_hex"] = checked_envelope(record["value"]).hex()
            envelopes = [bytes.fromhex(record["envelope_hex"]) for record in vector["records"]]
        else:
            count = vector["event_generation"]["count"]
            envelopes = [checked_envelope(folder_event(i)) for i in range(count)]
            vector["first_envelope_hex"] = envelopes[0].hex()
            vector["last_envelope_hex"] = envelopes[-1].hex()
        old = request["header"]
        request["header"] = header(
            old["author_id"], old["committer_id"], old["occurred_at"], old["message"], envelopes,
        )
        request["guard_count"], request["guards_digest"] = 0, sha_id(b"")
        vector["events_digest"] = request["header"]["events_digest"]
        head = checked_header(request["header"])
        vector["header_hex"] = head.hex()
        vector["commit_id"] = sha_id(head)
        data = wire("SubmitRequest", request)
        vector["request_hex"] = data.hex()
        vector["request_digest"] = sha_id(data)
        if name == "sync.json":
            vector["page_hex"] = wire("ProposalPage", {
                "repository": request["repository"], "operation_id": request["operation_id"],
                "offset": 0, "records": [
                    {"kind": r["kind"], "value": r["value"]} for r in vector["records"]
                ],
            }).hex()
        save(name, vector)


def new_file_version():
    owner = "01890f47-6a4c-7d8e-9f01-123456789abc"
    first = label_vector(owner, 0, bytes(range(32)), {
        "version_id": "01890f47-6a4d-7e9f-8012-23456789abcd",
        "source_item_seq": 1,
        "kind": "MINOR",
        "label_source": "INTERNAL",
        "order_key": 1024,
    })
    first["with_description"] = label_vector(owner, 1, bytes(range(32, 64)), {
        "version_id": "01890f47-6a4e-7f00-8123-3456789abcde",
        "source_item_seq": 2,
        "kind": "MAJOR",
        "label_source": "EXTERNAL",
        "order_key": 2048,
        "description": "Sent to the reviewer",
    })
    save("file-version.json", first)


def label_vector(owner, base_ref_seq, salt, metadata):
    """A FILE_VERSION::CREATE label: slim metadata, the owner only in the envelope."""
    assert set(metadata) - {"description"} == {
        "version_id", "source_item_seq", "kind", "label_source", "order_key",
    }
    payload = salted_utf8(salt, jcs_bytes(metadata))
    event = {
        "owner_item_id": owner,
        "event_type": "FILE_VERSION::CREATE",
        "base_ref_seq": base_ref_seq,
        "metadata_id": sha_id(payload),
    }
    data = checked_envelope(event)
    return {
        "metadata": metadata,
        "metadata_payload_hex": payload.hex(),
        "event": event,
        "envelope_hex": data.hex(),
        "event_digest": sha_id(data),
    }


# --- Protocol 2 messages (protocol.md, "Encoding") --------------------------
#
# A shape is a primitive name, a SHAPES name, or ("opt", T), ("list", T[, min, max])
# or ("fields", ((name, T), ...)). JSON values use the field names of the table.

LIFECYCLE = ["LIVE", "TRASH", "TOMBSTONED"]
MODES = ["LOCAL", "FEDERATED"]
LIMITS = ["events_per_commit", "object_bytes", "quota_bytes"]
REFUSALS = [
    "INVALID", "UNAUTHENTICATED", "FORBIDDEN", "NOT_FOUND", "OBJECT_MISSING", "CHECKPOINT",
    "LOCKED", "LOCK_LOST", "GENERATION", "OPERATION_REUSED", "OBJECTS_MISSING",
    "UPLOAD_EXPIRED", "SERVER_OVERLOADED",
]


def fields(*pairs):
    return ("fields", pairs)


GUARDED_ITEM = fields(("item_id", "uuid"), ("head_seq", "seq"))
CONFLICT = fields(("item_id", "uuid"), ("head_seq", "u64"))

SHAPES = {
    "RepositoryIdentity": fields(("repo_id", "uuid"), ("generation", "uuid")),
    "Cursor": fields(
        ("repository", "RepositoryIdentity"), ("repo_seq", "u64"), ("checkpoint", "digest")),
    "Capabilities": fields(
        ("protocol_version", "version"), ("encrypted", "bool"), ("latest", "Cursor"),
        ("policy", "ServerPolicy")),
    "ServerPolicy": fields(
        ("max_events_per_commit", ("opt", "u64")), ("max_object_bytes", ("opt", "u64")),
        ("quota_bytes", ("opt", "u64"))),
    "ProposalPage": fields(
        ("repository", "RepositoryIdentity"), ("operation_id", "str"), ("offset", "u64"),
        ("records", ("list", "Record"))),
    "Record": "record",
    "PageAck": fields(("next_offset", "u64"), ("expires_at", "time")),
    "SubmitRequest": fields(
        ("repository", "RepositoryIdentity"), ("operation_id", "str"),
        ("repository_head", ("opt", "u64")), ("header", "header"), ("guard_count", "u64"),
        ("guards_digest", "digest")),
    "SubmitResult": "submit_result",
    "Receipt": fields(
        ("operation_id", "str"), ("request_digest", "digest"), ("commit_id", "digest"),
        ("accepted_at", "time"), ("published", ("opt", "Cursor"))),
    "HistoryRequest": fields(("after", "HistoryPosition"), ("limit", "limit")),
    "HistoryPosition": fields(
        ("completed", "Cursor"),
        ("pending", ("opt", fields(("commit_id", "digest"), ("next_event_index", "index"))))),
    "HistoryPage": fields(
        ("after", "HistoryPosition"), ("fragments", ("list", "Fragment")), ("has_more", "bool")),
    "Fragment": fields(("header", "PublishedHeader"), ("events", ("list", "Event"))),
    "PublishedHeader": "published_header",
    "Event": "event",
    "StateRequest": fields(("after", "StatePosition"), ("limit", "limit")),
    "StatePosition": fields(
        ("repository", "RepositoryIdentity"), ("changed_seq", "u64"), ("item_id", ("opt", "uuid"))),
    "StatePage": fields(
        ("after", "StatePosition"), ("entries", ("list", "StateEntry")),
        ("departed", ("list", fields(("item_id", "uuid"), ("changed_seq", "u64")))),
        ("through", "Cursor"), ("has_more", "bool")),
    "StateEntry": fields(
        ("item_id", "uuid"), ("item_type", "str"), ("changed_seq", "u64"), ("head_seq", "seq"),
        ("ref_head_seq", "u64"), ("parent_folder_id", "target"), ("name", ("opt", "str")),
        ("record_key", ("opt", "str")), ("link_folder_id", "target"),
        ("lifecycle_state", "lifecycle"), ("sequencing_mode", "mode"),
        ("federation_stream_id", ("opt", "str")), ("content", "content"),
        ("metadata_id", ("opt", "digest")), ("name_metadata_id", ("opt", "digest")),
        ("created_at", "time"), ("created_by", "str"), ("changed_at", "time"),
        ("changed_by", "str")),
    "ItemHistoriesRequest": fields(
        ("repository", "RepositoryIdentity"),
        ("items", ("list", fields(
            ("item_id", "uuid"), ("after_seq", "u64"), ("after_ref_seq", "u64")), 1, 512)),
        ("limit", "limit")),
    "ItemHistoriesPage": fields(
        ("repository", "RepositoryIdentity"), ("pages", ("list", "ItemHistoryPage"))),
    "ItemHistoryPage": fields(
        ("item_id", "uuid"), ("events", ("list", "ItemEvent")), ("refs", ("list", "ItemEvent")),
        ("commits", ("list", "ItemCommit")), ("has_more", "bool")),
    "ItemEvent": fields(("repo_seq", "u64"), ("commit_event_idx", "u32"), ("event", "Event")),
    "ItemCommit": fields(
        ("repo_seq", "u64"), ("commit_id", "digest"), ("author_id", "str"),
        ("occurred_at", "time"), ("created_at", "time")),
    "CommitLogRequest": fields(
        ("repository", "RepositoryIdentity"), ("before_seq", ("opt", "u64")), ("limit", "limit")),
    "CommitLogPage": fields(("commits", ("list", "CommitSummary")), ("has_more", "bool")),
    "CommitSummary": fields(
        ("repo_seq", "u64"), ("commit_id", "digest"), ("author_id", "str"),
        ("occurred_at", "time"), ("event_count", "u32"),
        ("events", ("list", fields(
            ("item_id", "uuid"), ("item_seq", "seq"), ("commit_event_idx", "u32"),
            ("event_type", "str"))))),
    "ObjectRequest": ("list", "digest", 1, 512),
    "LeaseRequest": fields(("paths", ("list", "str")), ("owner", "str"), ("ttl", "u32")),
    "Lease": fields(("token", "str"), ("paths", ("list", "LeasedPath"))),
    "LeasedPath": fields(("path", "str"), ("owner", "str"), ("expires_at", "i64")),
    "Leases": ("list", "LeasedPath"),
    "LeaseRenewal": fields(("token", "str"), ("ttl", "u32")),
    "LeaseToken": fields(("token", "str")),
    "LeasePaths": ("list", "str"),
    "Released": fields(("released", "u64")),
    # Not in the shape table: a refused request's body and an object pack.
    "Refusal": "refusal",
    "ObjectPack": "object_pack",
}


def dec_bounded(r, width, low, high, what):
    value = r.uint(width)
    if not low <= value <= high:
        raise Malformed(f"{what} out of range")
    return value


def enc_record(value):
    kind, body = value["kind"], value["value"]
    assert set(value) == {"kind", "value"}, value
    if kind in ("item", "ref"):
        data = envelope(body)
        assert data[0] == (1 if kind == "item" else 2), value
        return u8(data[0]) + blob(data)
    if kind == "guard":
        return u8(3) + wire_encode(GUARDED_ITEM, body)
    assert kind == "subtree_guard", value
    assert set(body) == {"root_item_id", "require_empty_live_descendants", "descendants"}
    return (u8(4) + uuid_bytes(body["root_item_id"])
            + enc_bool(body["require_empty_live_descendants"])
            + wire_encode(("list", GUARDED_ITEM), body["descendants"]))


def dec_record(r):
    tag = r.uint(1)
    if tag in (1, 2):
        data = r.blob()
        body = decode_envelope(data)
        if data[0] != tag:
            raise Malformed("record tag does not match the envelope kind")
        return {"kind": "item" if tag == 1 else "ref", "value": body}
    if tag == 3:
        return {"kind": "guard", "value": wire_decode(GUARDED_ITEM, r)}
    if tag == 4:
        return {"kind": "subtree_guard", "value": {
            "root_item_id": r.uuid(), "require_empty_live_descendants": r.bool(),
            "descendants": wire_decode(("list", GUARDED_ITEM), r),
        }}
    raise Malformed("unknown record tag")


def enc_submit_result(value):
    status = value["status"]
    if status == "accepted":
        assert set(value) == {"status", "receipt"}
        return u8(0) + wire_encode("Receipt", value["receipt"])
    if status == "conflict":
        assert set(value) == {"status", "items"}
        return u8(1) + wire_encode(("list", CONFLICT), value["items"])
    assert status == "POLICY_LIMIT" and set(value) == {"status", "limit", "maximum", "actual"}
    return u8(2) + u8(LIMITS.index(value["limit"]) + 1) + u64(value["maximum"]) + u64(value["actual"])


def dec_submit_result(r):
    status = r.uint(1)
    if status == 0:
        return {"status": "accepted", "receipt": wire_decode("Receipt", r)}
    if status == 1:
        return {"status": "conflict", "items": wire_decode(("list", CONFLICT), r)}
    if status == 2:
        limit = LIMITS[dec_bounded(r, 1, 1, len(LIMITS), "policy limit") - 1]
        return {"status": "POLICY_LIMIT", "limit": limit, "maximum": r.u64(), "actual": r.u64()}
    raise Malformed("unknown status")


PUBLISHED_HEADER = (
    "commit_id", "repo_seq", "created_at", "checkpoint", "author_id", "committer_id",
    "occurred_at", "message", "event_count",
)


def enc_published_header(value):
    assert set(value) == set(PUBLISHED_HEADER), value
    occurred, message = value["occurred_at"], value["message"]
    data = (digest_bytes(value["commit_id"]) + u64(value["repo_seq"])
            + enc_time(value["created_at"]) + digest_bytes(value["checkpoint"])
            + enc_str(value["author_id"]) + enc_str(value["committer_id"])
            + u8((occurred is not None) | 2 * (message is not None)))
    if occurred is not None:
        data += enc_time(occurred)
    if message is not None:
        data += enc_str(message)
    return data + u32(value["event_count"])


def dec_published_header(r):
    value = {
        "commit_id": r.digest(), "repo_seq": r.u64(), "created_at": r.time(),
        "checkpoint": r.digest(), "author_id": r.str(), "committer_id": r.str(),
    }
    flags = r.uint(1)
    if flags >> 2:
        raise Malformed("unknown flag bit")
    value["occurred_at"] = r.time() if flags & 1 else None
    value["message"] = r.str() if flags & 2 else None
    value["event_count"] = r.uint(4)
    return value


def enc_event(value):
    data = envelope(value["envelope"])
    if data[0] == 1:
        assert set(value) == {"envelope", "item_seq", "parent_folder_id",
                              "parent_folder_id_before"}, value
        return (blob(data) + u64(value["item_seq"]) + enc_target(value["parent_folder_id"])
                + enc_target(value["parent_folder_id_before"]))
    assert set(value) == {"envelope", "ref_seq"}, value
    return blob(data) + u64(value["ref_seq"])


def dec_event(r):
    data = r.blob()
    body = decode_envelope(data)
    if data[0] == 2:
        return {"envelope": body, "ref_seq": r.seq()}
    return {
        "envelope": body, "item_seq": r.seq(), "parent_folder_id": r.target(),
        "parent_folder_id_before": r.target(),
    }


def enc_refusal(value):
    code = value["code"]
    data = u8(REFUSALS.index(code) + 1)
    if code == "LOCKED":
        assert set(value) == {"code", "path", "owner"}
        return data + enc_str(value["path"]) + enc_str(value["owner"])
    if code == "OBJECTS_MISSING":
        assert set(value) == {"code", "objects"}
        return data + wire_encode(("list", "digest"), value["objects"])
    assert set(value) == {"code"}
    return data


def dec_refusal(r):
    code = REFUSALS[dec_bounded(r, 1, 1, len(REFUSALS), "refusal code") - 1]
    if code == "LOCKED":
        return {"code": code, "path": r.str(), "owner": r.str()}
    if code == "OBJECTS_MISSING":
        return {"code": code, "objects": wire_decode(("list", "digest"), r)}
    return {"code": code}


def enc_pack(entries):
    data = b""
    for entry in entries:
        assert set(entry) == {"object_id", "bytes_hex"}
        stored = bytes.fromhex(entry["bytes_hex"])
        data += digest_bytes(entry["object_id"]) + u64(len(stored)) + stored
    return data


def dec_pack(r):
    entries = []
    while not r.at_end():
        object_id, size = r.digest(), r.u64()
        stored = r.take(size)
        if sha_id(stored) != object_id:
            raise Malformed("object does not match its ID")
        entries.append({"object_id": object_id, "bytes_hex": stored.hex()})
    if not 1 <= len(entries) <= 512:
        raise Malformed("a pack holds 1 to 512 objects")
    return entries


CODECS = {
    "u32": (u32, lambda r: r.uint(4)),
    "u64": (u64, Reader.u64),
    "i64": (i64, Reader.i64),
    "uuid": (uuid_bytes, Reader.uuid),
    "digest": (digest_bytes, Reader.digest),
    "str": (enc_str, Reader.str),
    "bool": (enc_bool, Reader.bool),
    "seq": (u64, Reader.seq),
    "time": (enc_time, Reader.time),
    "target": (enc_target, Reader.target),
    "content": (enc_content, Reader.content),
    "limit": (u16, lambda r: dec_bounded(r, 2, 1, 512, "limit")),
    "index": (u32, lambda r: dec_bounded(r, 4, 1, 2**32 - 1, "pending event index")),
    "version": (u16, lambda r: dec_bounded(r, 2, 2, 2, "protocol version")),
    "lifecycle": (lambda v: u8(LIFECYCLE.index(v) + 1),
                  lambda r: LIFECYCLE[dec_bounded(r, 1, 1, 3, "lifecycle state") - 1]),
    "mode": (lambda v: u8(MODES.index(v) + 1),
             lambda r: MODES[dec_bounded(r, 1, 1, 2, "sequencing mode") - 1]),
    "envelope": (lambda v: blob(envelope(v)), lambda r: decode_envelope(r.blob())),
    "header": (lambda v: blob(commit_header(v)), lambda r: decode_header(r.blob())),
    "record": (enc_record, dec_record),
    "submit_result": (enc_submit_result, dec_submit_result),
    "published_header": (enc_published_header, dec_published_header),
    "event": (enc_event, dec_event),
    "refusal": (enc_refusal, dec_refusal),
    "object_pack": (enc_pack, dec_pack),
}


def wire_encode(shape, value):
    shape = SHAPES.get(shape, shape) if isinstance(shape, str) else shape
    if isinstance(shape, str):
        return CODECS[shape][0](value)
    if shape[0] == "opt":
        return b"\0" if value is None else b"\1" + wire_encode(shape[1], value)
    if shape[0] == "list":
        return u32(len(value)) + b"".join(wire_encode(shape[1], v) for v in value)
    assert set(value) == {name for name, _ in shape[1]}, (value, shape)
    return b"".join(wire_encode(kind, value[name]) for name, kind in shape[1])


def wire_decode(shape, r):
    shape = SHAPES.get(shape, shape) if isinstance(shape, str) else shape
    if isinstance(shape, str):
        return CODECS[shape][1](r)
    if shape[0] == "opt":
        tag = r.uint(1)
        if tag > 1:
            raise Malformed("opt tag above 1")
        return wire_decode(shape[1], r) if tag else None
    if shape[0] == "list":
        count = r.uint(4)
        low, high = shape[2:] or (0, 2**32 - 1)
        if not low <= count <= high:
            raise Malformed("list length out of range")
        return [wire_decode(shape[1], r) for _ in range(count)]
    return {name: wire_decode(kind, r) for name, kind in shape[1]}


def wire_message(shape):
    def decode(data):
        r = Reader(data)
        value = wire_decode(shape, r)
        r.end()
        return value
    return decode


def wire(shape, value):
    """The bytes of one protocol value, checked to decode back to it."""
    data = wire_encode(shape, value)
    assert wire_message(shape)(data) == value, (shape, value)
    return data


def new_sync_wire():
    """One value of every protocol 2 shape, bytes a receiver refuses, and a guarded proposal."""
    identity = {
        "repo_id": "019a2900-0000-7000-8000-000000000001",
        "generation": "019a2900-0000-7000-8000-000000000002",
    }
    first = "019a2900-0000-7000-8000-000000000100"
    second = "019a2900-0000-7000-8000-000000000101"
    parent = "019a2900-0000-7000-8000-000000000102"
    chat = "019a2900-0000-7000-8000-000000000103"
    gone = "019a2900-0000-7000-8000-000000000104"
    accepted = "2026-10-28T00:00:00.123456789Z"
    occurred = "2026-10-28T09:00:00.000000000Z"
    genesis_value = genesis(identity["repo_id"], identity["generation"])
    start = {
        "repository": identity,
        "repo_seq": 0,
        "checkpoint": "sha256:" + genesis_value.hex(),
    }

    item_event = {
        "item_id": first, "base_seq": 3, "event_type": "FOLDER::SET_NAME", "set_name": "renamed",
    }
    ref_event = {
        "owner_item_id": second, "event_type": "FILE_VERSION::TOMBSTONE",
        "metadata_id": "sha256:" + "4d" * 32, "base_ref_seq": 2,
    }
    records = [
        {"kind": "item", "value": item_event},
        {"kind": "ref", "value": ref_event},
        {"kind": "guard", "value": {"item_id": first, "head_seq": 3}},
        {"kind": "guard", "value": {"item_id": second, "head_seq": 7}},
        {"kind": "subtree_guard", "value": {
            "root_item_id": first, "require_empty_live_descendants": False,
            "descendants": [{"item_id": second, "head_seq": 7}],
        }},
    ]
    record_bytes = [wire("Record", record) for record in records]
    envelopes = [checked_envelope(item_event), checked_envelope(ref_event)]
    guards_digest = sha_id(b"".join(sha(data) for data in record_bytes[2:]))
    commit = header("owner", "device", occurred, "Tidy", envelopes)
    commit_id = sha_id(checked_header(commit))
    request = {
        "repository": identity, "operation_id": "op:guarded", "repository_head": 0,
        "header": commit, "guard_count": 3, "guards_digest": guards_digest,
    }
    request_bytes = wire("SubmitRequest", request)
    request_digest = sha_id(request_bytes)

    seconds, nanos = parse_time(accepted)
    tip_id = "sha256:" + checkpoint(genesis_value, 1, commit_id, seconds, nanos).hex()
    published = {"repository": identity, "repo_seq": 1, "checkpoint": tip_id}
    published_header = {
        "commit_id": commit_id, "repo_seq": 1, "created_at": accepted, "checkpoint": tip_id,
        "author_id": "owner", "committer_id": "device", "occurred_at": occurred,
        "message": "Tidy", "event_count": 2,
    }
    item_row = {
        "envelope": item_event, "item_seq": 4, "parent_folder_id": parent,
        "parent_folder_id_before": None,
    }
    ref_row = {"envelope": ref_event, "ref_seq": 3}
    fragment = {"header": published_header, "events": [item_row, ref_row]}
    done = {"completed": start, "pending": None}
    pending = {"completed": start, "pending": {"commit_id": commit_id, "next_event_index": 1}}
    receipt = {
        "operation_id": "op:guarded", "request_digest": request_digest,
        "commit_id": commit_id, "accepted_at": accepted, "published": published,
    }
    policy = {"max_events_per_commit": 500, "max_object_bytes": None, "quota_bytes": None}
    capabilities = {
        "protocol_version": 2, "encrypted": False, "latest": published, "policy": policy,
    }
    file_entry = {
        "item_id": second, "item_type": "FILE", "changed_seq": 1, "head_seq": 7,
        "ref_head_seq": 3, "parent_folder_id": first, "name": "a.txt", "record_key": None,
        "link_folder_id": None, "lifecycle_state": "LIVE", "sequencing_mode": "LOCAL",
        "federation_stream_id": None,
        "content": {"bytes": {"id": "sha256:" + "ab" * 32, "size": 7}},
        "metadata_id": "sha256:" + "5e" * 32, "name_metadata_id": None,
        "created_at": accepted, "created_by": "owner", "changed_at": occurred,
        "changed_by": "owner",
    }
    chat_entry = dict(
        file_entry, item_id=chat, item_type="CHAT", head_seq=2, ref_head_seq=0,
        parent_folder_id=None, name=None, record_key="direct:" + "ab" * 32,
        lifecycle_state="TRASH", sequencing_mode="FEDERATED", federation_stream_id="stream-1",
        content={"body": {"id": "sha256:" + "6f" * 32, "size": 52},
                 "attachment": {"id": "sha256:" + "70" * 32, "size": 4096}},
        name_metadata_id="sha256:" + "71" * 32,
    )
    state_start = {"repository": identity, "changed_seq": 0, "item_id": None}
    item_commit = {
        "repo_seq": 1, "commit_id": commit_id, "author_id": "owner", "occurred_at": occurred,
        "created_at": accepted,
    }
    first_history = {
        "item_id": first, "events": [{"repo_seq": 1, "commit_event_idx": 0, "event": item_row}],
        "refs": [], "commits": [item_commit], "has_more": False,
    }
    second_history = {
        "item_id": second, "events": [],
        "refs": [{"repo_seq": 1, "commit_event_idx": 1, "event": ref_row}],
        "commits": [item_commit], "has_more": False,
    }
    summary = {
        "repo_seq": 1, "commit_id": commit_id, "author_id": "owner", "occurred_at": occurred,
        "event_count": 2, "events": [{
            "item_id": first, "item_seq": 4, "commit_event_idx": 0,
            "event_type": "FOLDER::SET_NAME",
        }],
    }
    leased = {"path": "docs/notes.txt", "owner": "agent-1", "expires_at": 1793145600}
    one, empty = b"one\n", b""
    pack = [
        {"object_id": sha_id(one), "bytes_hex": one.hex()},
        {"object_id": sha_id(empty), "bytes_hex": ""},
    ]
    valid = [
        ("RepositoryIdentity", identity),
        ("Cursor", published),
        ("Capabilities", capabilities),
        ("ServerPolicy", policy),
        ("ProposalPage", {
            "repository": identity, "operation_id": "op:guarded", "offset": 0,
            "records": records,
        }),
        *(("Record", record) for record in records),
        ("PageAck", {"next_offset": 5, "expires_at": accepted}),
        ("SubmitRequest", request),
        ("SubmitResult", {"status": "accepted", "receipt": receipt}),
        ("SubmitResult", {"status": "conflict", "items": [
            {"item_id": first, "head_seq": 5}, {"item_id": second, "head_seq": 0},
        ]}),
        ("SubmitResult", {
            "status": "POLICY_LIMIT", "limit": "events_per_commit", "maximum": 500,
            "actual": 1200,
        }),
        ("Receipt", receipt),
        ("Receipt", dict(receipt, published=None)),
        ("HistoryRequest", {"after": done, "limit": 512}),
        ("HistoryRequest", {"after": pending, "limit": 1}),
        ("HistoryPosition", pending),
        ("HistoryPage", {"after": done, "fragments": [fragment], "has_more": False}),
        ("Fragment", fragment),
        ("PublishedHeader", published_header),
        ("PublishedHeader", dict(published_header, occurred_at=None, message=None)),
        ("Event", item_row),
        ("Event", ref_row),
        ("StateRequest", {"after": state_start, "limit": 512}),
        ("StatePosition", {"repository": identity, "changed_seq": 1, "item_id": second}),
        ("StatePage", {
            "after": state_start, "entries": [file_entry],
            "departed": [{"item_id": gone, "changed_seq": 1}], "through": published,
            "has_more": False,
        }),
        ("StateEntry", file_entry),
        ("StateEntry", chat_entry),
        ("ItemHistoriesRequest", {"repository": identity, "items": [
            {"item_id": first, "after_seq": 3, "after_ref_seq": 0},
            {"item_id": second, "after_seq": 7, "after_ref_seq": 2},
        ], "limit": 512}),
        ("ItemHistoriesPage", {"repository": identity, "pages": [first_history, second_history]}),
        ("ItemHistoryPage", first_history),
        ("ItemEvent", first_history["events"][0]),
        ("ItemCommit", item_commit),
        ("CommitLogRequest", {"repository": identity, "before_seq": None, "limit": 50}),
        ("CommitLogRequest", {"repository": identity, "before_seq": 2, "limit": 50}),
        ("CommitLogPage", {"commits": [summary], "has_more": False}),
        ("CommitSummary", summary),
        ("ObjectRequest", [entry["object_id"] for entry in pack]),
        ("LeaseRequest", {"paths": ["docs/notes.txt"], "owner": "agent-1", "ttl": 600}),
        ("Lease", {"token": "lease-token-1", "paths": [leased]}),
        ("LeasedPath", leased),
        ("Leases", [leased]),
        ("LeaseRenewal", {"token": "lease-token-1", "ttl": 600}),
        ("LeaseToken", {"token": "lease-token-1"}),
        ("LeasePaths", ["docs"]),
        ("Released", {"released": 1}),
        ("Refusal", {"code": "INVALID"}),
        ("Refusal", {"code": "LOCKED", "path": "docs/notes.txt", "owner": "agent-1"}),
        ("Refusal", {"code": "OBJECTS_MISSING", "objects": [pack[0]["object_id"]]}),
        ("ObjectPack", pack),
    ]
    assert {kind for kind, _ in valid} == set(SHAPES)

    def enc(shape, value):
        return wire_encode(shape, value)

    caps_tail = enc("Cursor", published) + enc("ServerPolicy", policy)
    item_bytes = envelope(item_event)
    guard = enc("Record", records[2])
    assert item_bytes[35:37] == u16(0x22)
    entry_bytes = enc("StateEntry", file_entry)
    # item_id, item_type, changed_seq, head_seq, ref_head_seq, parent, name, no key, no link.
    lifecycle_at = 16 + 6 + 8 + 8 + 8 + 17 + len(enc(("opt", "str"), "a.txt")) + 1 + 1
    assert entry_bytes[lifecycle_at:lifecycle_at + 2] == u8(1) + u8(1)
    good_header = commit_header(commit)
    assert good_header[16] == 3
    bad_header = good_header[:16] + u8(7) + good_header[17:]
    ack = enc("PageAck", {"next_offset": 5, "expires_at": accepted})
    invalid = [
        ("Capabilities", enc("Capabilities", capabilities) + b"\0", "trailing byte"),
        ("Capabilities", u16(1) + enc_bool(False) + caps_tail, "protocol version 1"),
        ("Capabilities", u16(2) + u8(2) + caps_tail, "bool value 2"),
        ("ServerPolicy", u8(2) + u64(500) + u8(0) + u8(0), "opt tag 2"),
        ("RepositoryIdentity", uuid_bytes("019a2900-0000-4000-8000-000000000001")
         + uuid_bytes(identity["generation"]), "repository ID is a UUIDv4"),
        ("Cursor", enc("Cursor", published)[:-1], "truncated"),
        ("Record", u8(0) + blob(item_bytes), "record tag 0"),
        ("Record", u8(5) + blob(item_bytes), "record tag 5"),
        ("Record", u8(1) + blob(envelope(ref_event)), "tag 1 holds an attached-ref envelope"),
        ("Record", u8(2) + blob(item_bytes), "tag 2 holds an item envelope"),
        ("Record", u8(1) + blob(item_bytes + b"\0"), "envelope with a trailing byte"),
        ("Record", u8(1) + blob(u8(3) + item_bytes[1:]), "envelope with kind byte 3"),
        ("Record", u8(1) + blob(item_bytes[:35] + u16(0x1022) + item_bytes[37:]),
         "envelope with flag bit 12"),
        ("Record", u8(1) + u32(len(item_bytes) + 1) + item_bytes, "blob runs past the end"),
        ("Record", guard[:-8] + u64(0), "head guard with seq 0"),
        ("Record", u8(4) + uuid_bytes(first) + u8(2) + u32(0), "subtree guard with bool 2"),
        ("PageAck", u64(2**63) + ack[8:], "next_offset 2^63"),
        ("PageAck", ack[:16] + u32(10**9), "nanoseconds 10^9"),
        ("SubmitRequest", enc("SubmitRequest", request).replace(
            enc_str("op:guarded"), enc_str("op:\0guard")), "operation ID holds U+0000"),
        ("SubmitRequest", enc("SubmitRequest", request).replace(
            blob(good_header), blob(bad_header)), "header flag bit 2"),
        ("SubmitResult", u8(3), "status 3"),
        ("SubmitResult", u8(2) + u8(4) + u64(500) + u64(1200), "policy limit code 4"),
        ("SubmitResult", u8(2) + u8(0) + u64(500) + u64(1200), "policy limit code 0"),
        ("HistoryRequest", enc("HistoryPosition", done) + u16(0), "limit 0"),
        ("HistoryRequest", enc("HistoryPosition", done) + u16(513), "limit 513"),
        ("HistoryPosition", enc("Cursor", start) + u8(1) + digest_bytes(commit_id) + u32(0),
         "pending event index 0"),
        ("PublishedHeader", enc("PublishedHeader", published_header).replace(
            enc_str("device") + u8(3), enc_str("device") + u8(7)), "flag bit 2"),
        ("Event", blob(item_bytes) + u64(4) + u8(2) + uuid_bytes(parent) + u8(0),
         "target tag 2"),
        ("Event", blob(item_bytes) + u64(4) + enc_target(parent), "item event without its before"),
        ("StateEntry", entry_bytes.replace(
            u8(1) + enc_str("bytes"), u8(2) + enc_str("bytes") + digest_bytes(
                "sha256:" + "ab" * 32) + u64(7) + enc_str("attachment")),
         "content roles out of order"),
        ("StateEntry", entry_bytes[:lifecycle_at] + u8(4) + entry_bytes[lifecycle_at + 1:],
         "lifecycle state 4"),
        ("StateEntry", entry_bytes[:lifecycle_at] + u8(0) + entry_bytes[lifecycle_at + 1:],
         "lifecycle state 0"),
        ("StateEntry", entry_bytes[:lifecycle_at + 1] + u8(3) + entry_bytes[lifecycle_at + 2:],
         "sequencing mode 3"),
        ("ObjectRequest", u32(0), "no object IDs"),
        ("ObjectRequest", u32(513) + digest_bytes(pack[0]["object_id"]) * 513,
         "513 object IDs"),
        ("ItemHistoriesRequest", enc("RepositoryIdentity", identity) + u32(0) + u16(512),
         "no items"),
        ("Leases", u32(2) + enc("LeasedPath", leased), "list count past the end"),
        ("Refusal", u8(0), "refusal code 0"),
        ("Refusal", u8(14), "refusal code 14"),
        ("Refusal", u8(1) + b"\0", "trailing byte"),
        ("ObjectPack", b"", "no objects"),
        ("ObjectPack", digest_bytes(sha_id(one)) + u64(5) + one, "size runs past the end"),
        ("ObjectPack", digest_bytes(sha_id(one)) + u64(4) + b"two\n", "bytes do not match the ID"),
    ]
    save("sync-wire.json", {
        "description": "One value of each protocol 2 shape: `valid` gives its readable JSON "
                       "`value` and its exact `bytes_hex`, which decode to that value; "
                       "`invalid` gives bytes a receiver refuses. `guarded` is one proposal "
                       "with two head guards and a subtree guard.",
        "valid": [
            {"type": kind, "value": value, "bytes_hex": wire(kind, value).hex()}
            for kind, value in valid
        ],
        "invalid": [
            {"type": kind, "bytes_hex": refused(wire_message(kind), data), "reason": reason}
            for kind, data, reason in invalid
        ],
        "guarded": {
            "request": request, "records": records,
            "envelopes_hex": [data.hex() for data in envelopes],
            "records_hex": [data.hex() for data in record_bytes],
            "guards_digest": guards_digest,
            "commit_id": commit_id, "request_hex": request_bytes.hex(),
            "request_digest": request_digest,
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

    # The salts are those of the format 7 vector's labels, which conversion keeps.
    def metadata(value, salt):
        payload = salted_utf8(bytes([salt]) * 32, jcs_bytes(value))
        stored[sha_id(payload)] = payload
        return sha_id(payload)

    def contents(name):
        return {"bytes": {"id": ids[name], "size": len(blobs[name])}}

    def label(version, item_seq, kind, order, salt):
        value = {
            "version_id": f"019a2900-0000-7000-8000-00000000b00{version}",
            "source_item_seq": item_seq, "kind": kind, "label_source": "INTERNAL",
            "order_key": order,
        }
        return {
            "owner_item_id": notes, "event_type": "FILE_VERSION::CREATE",
            "base_ref_seq": version - 1, "metadata_id": metadata(value, salt),
        }

    commits = [("Add the notes", [
        {"item_id": docs, "event_type": "FOLDER::CREATE",
         "create_item": {"item_type": "FOLDER", "name": "docs"}},
        {"item_id": notes, "event_type": "FILE::CREATE",
         "create_item": {"item_type": "FILE", "name": "notes.txt", "parent_folder_id": docs},
         "set_content": contents("one\n")},
    ], [label(1, 1, "MINOR", 1024, 2)]), ("Change the notes", [
        {"item_id": notes, "base_seq": 1, "event_type": "FILE::SET_CONTENT",
         "set_content": contents("two\n")},
    ], []), ("Name the second revision", [], [
        label(2, 2, "MAJOR", 2048, 4),
    ]), ("Nothing remains of this commit", [], []), ("Move the notes and file a task", [
        {"item_id": notes, "base_seq": 2, "event_type": "FILE::SET_PARENT", "set_parent": None},
        {"item_id": scratch, "event_type": "FILE::CREATE",
         "create_item": {"item_type": "FILE", "name": "scratch.txt"},
         "set_content": contents("scratch\n")},
        {"item_id": task, "event_type": "TASK::CREATE",
         "create_item": {"item_type": "TASK", "link": docs}},
    ], []), ("Trash the scratch file", [
        {"item_id": scratch, "base_seq": 1, "event_type": "FILE::TRASH", "set_in_trash": True},
    ], []), ("Nothing remains of this commit", [], [])]

    def quote(text):
        return "NULL" if text is None else "'" + text.replace("'", "''") + "'"

    def hexblob(data):
        return "X'" + data.hex().upper() + "'"

    def stamp(index):
        return f"2026-10-28T00:00:{index:02d}.000000001Z"

    rows = {"commits": [], "events": [], "refs": []}
    items = {}
    created = {}
    previous = genesis(repo, generation)
    expected = []
    for index, (message, events, refs) in enumerate(commits, 1):
        at = stamp(index)
        envelopes = [checked_envelope(e) for e in events + refs]
        commit_id = sha_id(checked_header(header("owner", "owner", None, message, envelopes, index)))
        seconds, nanos = parse_time(at)
        previous = checkpoint(previous, index, commit_id, seconds, nanos)
        pk = f"019a2900-0000-7000-8000-00000000c{index:03x}"
        replayed = replay_commit(items, {"events": events, "ref_events": refs, "message": message})
        rows["commits"].append(
            f"INSERT INTO vcs_commits VALUES({quote(repo)},{quote(pk)},"
            f"{hexblob(digest_bytes(commit_id))},{index},NULL,'owner','owner',{quote(message)},"
            f"NULL,{quote(at)},{len(envelopes)},{hexblob(previous)});"
        )
        for position, event in enumerate(events):
            item_id = event["item_id"]
            created.setdefault(item_id, at)
            seq = items.get(item_id, {"seq": 0})["seq"] + 1
            before = items.get(item_id, {}).get("parent")
            after = replayed[item_id]["parent"]
            rows["events"].append(
                f"INSERT INTO vcs_events VALUES({quote(repo)},{quote(pk)},{position},"
                f"{quote(item_id)},{seq},{quote(event['event_type'])},"
                f"{hexblob(envelopes[position])},{quote(at)},{quote(after)},"
                f"{quote(before if 'set_parent' in event else None)});"
            )
        for position, event in enumerate(refs, len(events)):
            owner = event["owner_item_id"]
            rows["refs"].append(
                f"INSERT INTO vcs_ref_events VALUES({quote(repo)},{quote(pk)},{position},"
                f"{quote(owner)},{event['base_ref_seq'] + 1},{quote(event['event_type'])},"
                f"{hexblob(envelopes[position])},{quote(at)});"
            )
        items = replayed
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
        *(
            f"INSERT INTO vcs_metadata_objects VALUES({quote(repo)},"
            f"{hexblob(digest_bytes(object_id))},{hexblob(payload)});"
            for object_id, payload in sorted(stored.items())
        ),
        *rows["commits"], *rows["events"], *rows["refs"], *item_rows,
    ]) + "\n"
    database = sqlite3.connect(":memory:")
    database.execute("PRAGMA foreign_keys = ON")
    database.executescript((SPEC / "history.sql").read_text(encoding="utf-8") + text)
    database.close()

    write("repository/rows.sql", text)
    write("repository/config.json", json.dumps({
        "format_version": 8, "repo_id": repo, "authority": "FOLLOWER", "storage_mode": "WHOLE_FILE",
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

# The core's tree types; every other type takes its class from its create form.
TREE_TYPES = {"FILE", "FOLDER", "FEDERATED_MOUNT"}


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
    created, rebound, mode_changed = set(), set(), set()
    for event in commit["events"] + commit.get("ref_events", []):
        envelope(event)  # every case must have bytes
    for event in commit["events"]:
        item_id = event["item_id"]
        create = event.get("create_item")
        source = event.get("merged_from") or (create or {}).get("derived_from")
        if create is not None and "merged_from" in event:
            raise Invalid("merged_from on a created item")
        modifies = any(field in event for field in (
            "set_parent", "set_name", "set_link", "set_tombstoned", "set_in_trash"))
        if create is not None and modifies:
            raise Invalid("a creation carries its fields in create_item")
        if modifies and ("set_is_federated" in event or "set_federation_stream_id" in event):
            raise Invalid("a federation transition changes nothing else")
        if source is not None:
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
            tree = "name" in create
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
            created.add(item_id)
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
            if item["type"] in ("FOLDER", "LOCAL_COMPANION"):
                raise Invalid("this item type is never federated")
            stream = event.get("set_federation_stream_id")
            if not stream:
                raise Invalid("federation needs a stream")
            if create is not None:
                item["mode"], item["stream"] = "FEDERATED", stream
            elif (item_id not in rebound
                    and item["mode"] == "FEDERATED" and item["stream"] == stream):
                rebound.add(item_id)
            elif item["mode"] == "LOCAL" and item_id not in mode_changed:
                mode_changed.add(item_id)
                item["mode"], item["stream"] = "FEDERATED", stream
            else:
                raise Invalid("illegal federation transition")
        if "set_federation_stream_id" in event and event.get("set_is_federated") is not True:
            raise Invalid("a stream comes with a federation")
        if event.get("set_is_federated") is False:
            if item["mode"] != "FEDERATED" or item_id in mode_changed:
                raise Invalid("only a federated item returns to local, once")
            mode_changed.add(item_id)
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
    content = {"id": "sha256:" + "ab" * 32, "size": 5}

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
    other = {"id": "sha256:" + "cd" * 32, "size": 9}
    file_item = {**tree("r1", "FILE", "a"), "set_content": {"bytes": content}}
    cases = [
        ("record without key or link", [([record("r1", "TASK")], True)]),
        ("another type takes the tree form from its creation", [([{
            "item_id": ids["r1"], "event_type": "TASK::CREATE",
            "create_item": {"item_type": "TASK", "name": "a"},
        }], True)]),
        ("tree item without name", [([record("r1", "FOLDER")], False)]),
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
        ("a companion never federates", [
            ([record("c1", "LOCAL_COMPANION", key=ids["r1"])], True),
            ([update("c1", 1, "LOCAL_COMPANION::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], False),
        ]),
        ("a creation carries its link in create_item", [
            ([folder("F"), dict(record("r1", "CHAT"), set_link=f1)], False),
        ]),
        ("a folder never federates", [
            ([folder("F")], True),
            ([update("f1", 1, "FOLDER::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], False),
        ]),
        ("a folder never federates, even within one commit", [
            ([dict(folder("F"), set_is_federated=True, set_federation_stream_id="s"),
              {"item_id": f1, "event_type": "FOLDER::FEDERATE", "set_is_federated": False}], False),
        ]),
        ("a record may leave federation in the commit that created it", [
            ([dict(record("r1", "CHAT"), set_is_federated=True, set_federation_stream_id="s"),
              {"item_id": ids["r1"], "event_type": "CHAT::FEDERATE", "set_is_federated": False}], True),
        ]),
        ("a companion never federates, even within one commit", [
            ([dict(record("c1", "LOCAL_COMPANION", key=ids["r1"]), set_is_federated=True,
                   set_federation_stream_id="s"),
              {"item_id": ids["c1"], "event_type": "LOCAL_COMPANION::FEDERATE",
               "set_is_federated": False}], False),
        ]),
        ("a federated item does not federate again", [
            ([record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], True),
            ([update("r1", 2, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s2")], False),
        ]),
        ("a record federated after its creation may repeat its stream once", [
            ([record("r1", "CHAT"),
              {"item_id": ids["r1"], "event_type": "CHAT::FEDERATE", "set_is_federated": True,
               "set_federation_stream_id": "s"},
              {"item_id": ids["r1"], "event_type": "CHAT::FEDERATE", "set_is_federated": True,
               "set_federation_stream_id": "s"}], True),
        ]),
        ("a bootstrap repeats its stream only once", [
            ([dict(record("r1", "CHAT"), set_is_federated=True, set_federation_stream_id="s"),
              {"item_id": ids["r1"], "event_type": "CHAT::FEDERATE", "set_is_federated": True,
               "set_federation_stream_id": "s"},
              {"item_id": ids["r1"], "event_type": "CHAT::FEDERATE", "set_is_federated": True,
               "set_federation_stream_id": "s"}], False),
        ]),
        ("a record federates and returns across commits", [
            ([record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], True),
            ([update("r1", 2, "CHAT::FEDERATE", set_is_federated=False)], True),
        ]),
        ("a federated record repeats its stream in a later commit", [
            ([dict(record("r1", "CHAT"), set_is_federated=True, set_federation_stream_id="s")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], True),
        ]),
        ("a later commit repeats the stream only once", [
            ([dict(record("r1", "CHAT"), set_is_federated=True, set_federation_stream_id="s")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s"),
              update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s")], False),
        ]),
        ("a bootstrap repeats its own stream", [
            ([dict(record("r1", "CHAT"), set_is_federated=True, set_federation_stream_id="s"),
              {"item_id": ids["r1"], "event_type": "CHAT::FEDERATE", "set_is_federated": True,
               "set_federation_stream_id": "other"}], False),
        ]),
        ("one commit changes an item's mode once", [
            ([record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s"),
              update("r1", 1, "CHAT::FEDERATE", set_is_federated=False)], False),
        ]),
        ("federation needs a stream", [
            ([record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True)], False),
        ]),
        ("federation needs a nonempty stream", [
            ([record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="")], False),
        ]),
        ("a new link never targets a tombstoned folder", [
            ([folder("F")], True),
            ([record("r1", "CHAT")], True),
            ([tombstone("f1", 1, "FOLDER")], True),
            ([update("r1", 1, "CHAT::SET_LINK", set_link=f1)], False),
        ]),
        ("a commit with no events", [
            ([record("r1", "CHAT")], True),
            ([], True),
            ([update("r1", 1, "CHAT::SET_METADATA")], True),
        ]),
        ("a creation changes nothing else", [
            ([dict(record("r1", "CHAT"), set_in_trash=True)], False),
        ]),
        ("a federation transition changes nothing else", [
            ([record("r1", "CHAT")], True),
            ([update("r1", 1, "CHAT::FEDERATE", set_is_federated=True,
                     set_federation_stream_id="s", set_in_trash=True)], False),
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


MAX_BUNDLE_RECORD = 8 * 1024 * 1024


def bundle_end(lines, tip):
    body = "".join(lines)
    return bundle_line({
        "type": "synck_vcs_bundle_end",
        "record_count": sum(1 for line in lines if line != "\n"),
        "checkpoint": tip,
        "digest": sha_id(body.encode("utf-8")),
    })


def bundle_valid(text, genesis_value):
    """The end-record rules, event times and commit IDs."""
    if not text.endswith("\n"):
        return False
    lines = text.split("\n")[:-1]
    if any(len(line.encode("utf-8")) >= MAX_BUNDLE_RECORD for line in lines):
        return False
    try:
        end = json.loads(lines[-1])
    except ValueError:
        return False
    if end.get("type") != "synck_vcs_bundle_end":
        return False
    body = text[: len(text) - len(lines[-1]) - 1]
    records = [json.loads(line) for line in lines[:-1] if line]
    if any(record["type"] == "synck_vcs_bundle_end" for record in records):
        return False
    commits = [record for record in records if record["type"] == "commit"]
    tip = commits[-1]["checkpoint"] if commits else genesis_value
    times = {record["commit_id"]: parse_time(record["created_at"]) for record in commits}
    events = [record for record in records if record["type"] in ("event", "ref_event")]
    if any(parse_time(record["created_at"]) != times[record["commit_id"]] for record in events):
        return False
    for commit in commits:
        envelopes = [
            bytes.fromhex(record["payload_hex"]) for record in events
            if record["commit_id"] == commit["commit_id"]
        ]
        value = header(
            commit["author_id"], commit["committer_id"], commit["occurred_at"],
            commit["message"], envelopes, commit["repo_seq"],
        )
        if commit["event_count"] != len(envelopes) or sha_id(commit_header(value)) != commit["commit_id"]:
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
    content = {"id": "sha256:" + "ab" * 32, "size": 1}
    meta = salted_utf8(bytes([9]) * 32, jcs_bytes({"color": "red"}))
    head = bundle_line({
        "type": "synck_vcs_bundle_header", "bundle_format_version": 6, "repo_id": repo,
        "history_generation": generation, "encrypted": False, "exported_at": at,
        "schema": {"subsystem": "synck_vcs_sqlite", "schema_id": "core-v8"},
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
    envelopes = [checked_envelope(first), checked_envelope(second)]
    commit_id = sha_id(checked_header(header("owner", "device", None, "add a file", envelopes)))
    seconds, nanos = parse_time(at)
    base = genesis(repo, generation)
    tip = "sha256:" + checkpoint(base, 1, commit_id, seconds, nanos).hex()

    def event(index, value, parent):
        return bundle_line({
            "type": "event", "commit_id": commit_id, "repo_seq": 1, "commit_event_idx": index,
            "item_id": value["item_id"], "item_seq": 1, "event_type": value["event_type"],
            "payload_hex": envelopes[index].hex(), "created_at": at, "parent_folder_id": parent, "parent_folder_id_before": None,
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
            "operation_id": "op:1", "author_id": "owner",
            "committer_id": "device", "message": "add a file", "occurred_at": None,
            "created_at": at, "event_count": 2,
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

    def with_empty_commit(position):
        commit_id = sha_id(commit_header(header(
            "owner", "device", None, "nothing left", [], position)))
        end_tip = "sha256:" + checkpoint(
            bytes.fromhex(tip[7:]), 2, commit_id, *parse_time(at)).hex()
        body = lines[:5] + [bundle_line({
            "type": "commit", "commit_id": commit_id, "checkpoint": end_tip, "repo_seq": 2,
            "operation_id": None, "author_id": "owner",
            "committer_id": "device", "message": "nothing left", "occurred_at": None,
            "created_at": at, "event_count": 0,
        })] + lines[5:]
        return "".join(body) + bundle_end(body, end_tip)

    cases = [
        ("complete", whole, True),
        ("empty history", head + bundle_end([head], genesis_value), True),
        ("a commit with no events", with_empty_commit(2), True),
        ("an empty commit hashes its position", with_empty_commit(1), False),
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
    new_encrypted_segments()
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
