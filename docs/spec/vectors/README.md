# Synck format vectors

Language-neutral test data for [the format](../format.md). An implementation that
reads these files needs no Rust source. Every ID is canonical; digests may be
synthetic, and an example ID never means the object exists.

| File | Fixes | Shape |
| --- | --- | --- |
| `names.json` | Portable names, keys and proposed spellings | `cases`: `name`, `valid`, `key`, `fixed` |
| `name-tags.json` | Tagged names in encrypted repositories | `key_hex`; `tags`: `name`, `tag` (null when the name is invalid); `histories`: replay cases as in `records.json`, each with `encrypted` |
| `json.json` | Hashed JSON | List: `text`, `valid` |
| `objects.json` | PLAIN and SALTED objects | List: `form`, `bytes_hex`, `payload_hex`, `id`, `valid` |
| `encrypted.json` | The encrypted frame (scheme 2) and associated data, and rejected frames such as scheme 1 | List: `frame_valid` needs no key; `opens` needs `key_hex` |
| `encrypted-segments.json` | Scheme 2 objects too large for hex, and its length rule | `objects`: `plaintext_length`, `stored_length`, `id` of the plaintext described by `plaintext`; `lengths`: `ciphertext_length`, `frame_valid`, `plaintext_length` |
| `metadata.json` | One metadata payload | `value`, `canonical_json`, `payload_hex`, `id` |
| `events.json` | Envelope bytes, event digests and commit IDs, and rejected encodings | `valid`: `event`, `envelope_hex`, `event_digest`, `commit`, `header_hex`, `commit_id`; `invalid`: `name`, `envelope_hex` |
| `commits.json` | Commit identity | List: `header`, `events` (`event`, `envelope_hex`, `digest`), `header_hex`, `commit_id` |
| `checkpoints.json` | The genesis value and checkpoint chain | `genesis` and `steps`, including an invalid sequence gap |
| `records.json` | Replay | List of commit sequences; see below |
| `file-version.json` | FILE revision labels | `metadata`, `metadata_payload_hex`, `event`, `envelope_hex`, `event_digest`; `with_description` repeats them for a later revision with a description |
| `sync.json`, `sync-history.json`, `sync-large.json` | The sync protocol | Requests, records, their bytes and digests |
| `sync-wire.json` | The protocol's binary shapes and guards | `valid`: `type`, `value`, `bytes_hex`; `invalid`: `type`, `bytes_hex`, `reason`; and `guarded` |
| `repository/` | A whole repository | See below |

`valid` means the input is accepted by the rule that file tests, which is not always
the whole format.

## records.json

Each case has a `name` and `commits`, applied in order. A commit has `events`, and may
have `ref_events` and a `message`. It also has its verdict: `valid`, and `state_digest`
when true or `reason` when false. A digest after every commit lets two implementations
compare state, not only verdicts. `reason` is for people: the verdict is the vector, not
the wording. These are replay rules, not family admission rules, so a case may hold
metadata no family would admit.

## repository/

A complete repository, as text. To build it:

1. Create an empty directory with `.synck/`, `.synck/objects/` and an empty
   `.synck/lock`.
2. Copy `config.json` and `objects/` into `.synck/`.
3. Create `.synck/repository.sqlite3` with the pragmas from [the format](../format.md),
   then run [history.sql](../history.sql) and `rows.sql`.

`expected.json` gives each commit's ID, checkpoint and state digest, the live files
and their object IDs, and the folders. The history covers a tree item in a folder, a
new revision, a label in the same commit as its revision, a label of a revision made
in an earlier commit, a move, a linked record and a file in the trash.

## Regenerating

`scripts/generate_vectors.py` computes every hash, digest and verdict here from the
format, sharing no code with the engine. Inputs written by hand stay in the files, so
regenerating changes nothing unless the format did:

```sh
python3 -I scripts/generate_vectors.py --check
```

It needs the Python `cryptography` package. It also carries a small replay of the tree,
record, key, link and event type rules, which must agree with every case in
`records.json`.

## file-folder/

These files are outside the format. They hold one writer's intent vocabulary, error
names and change groups, for implementations that want to share that writer's
behaviour. `events.json` there is an application view with expanded `set_metadata`, not
accepted envelopes.
