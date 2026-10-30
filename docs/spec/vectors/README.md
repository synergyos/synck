# Synck format vectors

Language-neutral test data for [the format](../format.md). An implementation that
reads these files needs no Rust source. Every ID is canonical; digests may be
synthetic, and an example ID never means the object exists.

| File | Fixes | Shape |
| --- | --- | --- |
| `names.json` | Portable names, keys and proposed spellings | `cases`: `name`, `valid`, `key`, `fixed` |
| `json.json` | Hashed JSON | List: `text`, `valid` |
| `objects.json` | PLAIN and SALTED objects | List: `form`, `bytes_hex`, `payload_hex`, `id`, `valid` |
| `encrypted.json` | The encrypted frame and associated data | List: `frame_valid` needs no key; `opens` needs `key_hex` |
| `metadata.json` | One metadata payload | `value`, `canonical_json`, `payload_hex`, `id` |
| `events.json` | Canonical envelopes, event digests and commit hashes | List: `event`, `canonical_event`, `event_digest`, `commit`, `commit_id` |
| `commits.json` | Commit identity | List: `header`, `events`, `canonical`, `commit_id` |
| `checkpoints.json` | The genesis value and checkpoint chain | `genesis` and `steps`, including an invalid sequence gap |
| `records.json` | Replay | List of commit sequences; see below |
| `bundle.json` | The end record and acceptance times | `genesis`, and `cases` with `bundle` and `valid` |
| `file-version.json` | FILE revision labels | `metadata`, `event`, `canonical_event`, `event_digest`; `with_source_commit` repeats them for a revision in an earlier commit |
| `sync.json`, `sync-history.json`, `sync-large.json` | The sync protocol | Requests, records and digests |
| `sync-wire.json` | The protocol's JSON shapes and head guards | `valid` and `invalid` lists of `type` and `value`, and `guarded` |
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
new revision, a label in the same commit as its revision, a label naming an earlier
commit, a move, a linked record and a file in the trash.

## Regenerating

`scripts/generate_vectors.py` computes every hash, digest and verdict here from the
format, sharing no code with the engine. Inputs written by hand stay in the files, so
regenerating changes nothing unless the format did:

```sh
python3 -I scripts/generate_vectors.py --check
```

It needs the Python `cryptography` package. It also carries a small replay of the tree,
record, key, link and event type rules, and a check of the bundle end record, which
must agree with every case in `records.json` and `bundle.json`.

## file-folder/

These files are outside the format. They hold one writer's intent vocabulary, error
names and change groups, for implementations that want to share that writer's
behaviour. `events.json` there is an application view with expanded `set_metadata`, not
accepted envelopes.
