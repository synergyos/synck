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
| `file-version.json` | One FILE revision label | `metadata`, `event`, `canonical_event`, `event_digest` |
| `sync.json`, `sync-history.json`, `sync-large.json` | The sync protocol | Requests, records and digests |

`valid` means the input is accepted by the rule that file tests, which is not always
the whole format.

## records.json

Each case has a `name` and `commits`, applied in order. A commit has `events`, and may
have `ref_events` and a `message`. It also has its verdict: `valid`, and `state_digest`
when true or `reason` when false. A digest after every commit lets two implementations
compare state, not only verdicts. `reason` is for people: the verdict is the vector, not
the wording. These are replay rules, not family admission rules, so a case may hold
metadata no family would admit.

## file-folder/

These files are outside the format. They hold one writer's intent vocabulary, error
names and change groups, for implementations that want to share that writer's
behaviour. `events.json` there is an application view with expanded `set_metadata`, not
accepted envelopes.
