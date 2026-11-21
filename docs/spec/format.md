# Synck repository format 8

This is the authoritative format-8 specification. A reader accepts only this
repository format and history bundle format 6. Schema versions are independent
of release numbers. There are no older-format readers or migrations. The
normative SQL and JSON vectors linked here are part of this specification; no
Rust source is needed to interpret a repository.

Only bytes are specified: what a repository, an event, an object and a bundle
contain, and what makes them valid. Commands, caches, checkouts and recovery belong
to the [guides](../README.md). Every rule here binds a writer, a reader and a
verifier unless it names one of them; examples and rationale do not add rules. A
present-tense rule is as binding as one spelled MUST.

## Core and item families

The core is everything a reader needs without knowing any item type: this
container, the binary encoding, event envelopes, tree and record rules, objects,
commits and checkpoints. After 1.0 it changes only with a new repository format.

Item families define event types, metadata keys and content roles. Each family
is versioned separately, in this document; no family version is stored in history.
Adding a family or an event type never changes the core. All families below are at
version 1.

Migration-only event types are forbidden. A conversion between formats happens
offline and writes the resulting history; it leaves no conversion, repair or
backfill events behind.

## Fixed validity limits

These limits belong to format 8, not to a writer configuration or a hosted
service. Every writer, reader, replay/import path and `fsck` enforces them.
Lengths are UTF-8 bytes (not characters); event size is its exact encoded
envelope.

| Field | Maximum |
| --- | ---: |
| Commit message | 16,384 bytes (16 KiB) |
| Author ID, committer ID (each) | 256 bytes |
| Operation ID | 256 bytes |
| One item or attached-ref event payload | 1,048,576 bytes (1 MiB) |
| One name | 255 bytes |
| One record key | 256 bytes |
| One item type | 64 bytes |
| One event type | 128 bytes |
| Metadata JSON nesting | 64 levels |

Author and committer IDs are nonempty. A stored operation ID is null or
nonblank UTF-8, and nonblank means at least one code point that is not Unicode
White_Space. The schema's `trim()` check is a weaker backstop, not the rule. A
commit submitted through the [protocol](protocol.md) requires one; a local commit
may store null. Preserve its exact bytes, including whitespace; do not trim or
derive a different retry identity. An operation ID is retry identity, not
history: no hash covers it. A store keeps it at least 90 days after acceptance
and may then clear it to null. A message is null or nonempty; an empty message
is rejected. Name normalization and forbidden-character rules below still apply.

There is **no per-commit event-count or aggregate byte admission limit**. Counts
and positions must still fit their specified integer representation. Transfer
page bounds and optional server admission policy never change repository
validity or split/rewrite accepted commits. See [the sync protocol](protocol.md).
Configuration cannot override format validity.

## Container and identifiers

A repository is a directory containing `.synck/` with:

- `config.json`: UTF-8 JSON configuration.
- `repository.sqlite3`: canonical history, schema [history.sql](history.sql).
- `objects/`: required directory; in WHOLE_FILE mode it holds objects.
- `object-store.sqlite3`: required only in CHUNKED mode, schema
  [object-store.sql](object-store.sql).
- `checkout.json`: optional mutable checkout state.
- `lock`: required empty file for local coordination. A reader must obtain a consistent snapshot, including SQLite WAL contents.
- `tmp/`: staging that a writer clears under the lock; optional without a checkout.

Any other file in `.synck/` is outside the format, and a reader ignores it. Nothing
outside the format contributes to any hash, and verification neither requires nor
trusts it. Treat repository paths as untrusted; do not follow object or database
symlinks outside the repository.

Configuration is an object with required `format_version: 8`, `repo_id`,
`authority` (`LOCAL`, `FOLLOWER`, or `INDEPENDENT_LOCAL`), optional
`storage_mode` (`WHOLE_FILE`, the default, or `CHUNKED`), `author` (string, required
unless `authority` is `FOLLOWER`), optional `encrypted` (boolean, default false),
optional `origin` and optional `hold` (boolean, default false).
Unknown fields are rejected. `encrypted` is fixed when the repository is created and
copies keep it. `author` is the default author ID for new commits, at most 256 bytes;
it is not history.

Working-tree files (`checkout.json`, `conflicts.json`, a clone's `.synck/`,
`held.json`, `withdrawn.json` and `pins/`) are local CLI state, not history: no
hash covers them and copies do not carry them. `gc` keeps every object a pin
names. The [local repository guide](../local-repository.md#working-tree-files)
defines them.

Authority is local operational configuration, not a cryptographic credential.
`FOLLOWER` is an imported copy, and it refuses writes. `INDEPENDENT_LOCAL` accepts
writes: a repository created here, or a copy given its own checkout. `LOCAL` accepts
writes too: a copy activated to succeed a source that has retired. Neither is a
synchronization or distributed fencing protocol.

Repository IDs, item IDs, physical `commit_pk` IDs, history generations and FILE
version IDs are UUIDv7. Commit IDs, object IDs, event digests and checkpoints are
SHA-256 digests; they are distinct domains despite sharing a type. Encoded history
holds raw bytes: 16 for a UUID, 32 for a digest. Text, such as JSON files,
bundles and SQLite TEXT columns, spells a UUID in lowercase hyphenated form and a
digest as `sha256:` plus 64 lowercase hexadecimal digits; SQLite BLOB columns hold
the raw 32 bytes. Authors and committers are application identity strings, not
UUIDs. A federation domain ID is a nonempty string, opaque to the format.

Sequences are positive signed 64-bit integers; ref bases also allow zero. Event
indexes start at zero and event counts are not negative; both fit signed 32-bit
storage/API integers. There is no smaller policy cap.

A time is a signed 64-bit count of seconds since the Unix epoch and a nanosecond
fraction below 10^9, for UTC years 0000 through 9999. Leap seconds are not
representable. Text spells a time in RFC 3339 UTC with nine fractional digits and
`Z`; a reader of text compares instants, not spellings.

## Canonical JSON

Metadata objects hold JSON, and nothing else in history does. That JSON is RFC
8785 JCS, encoded as UTF-8: no whitespace, object keys sorted by UTF-16 code
units, minimal prescribed string escapes, and no duplicate keys or invalid
Unicode. Strings are not normalized by JCS.

It is a strict subset of RFC 8785, so that no implementation has to serialize
floating-point numbers identically:

- A number is an integer from -(2^53 - 1) to 2^53 - 1, in plain decimal. Fractions,
  exponents and negative zero are rejected. A decimal value is a string.
- A string or key must not contain U+0000. Other control characters use the
  escapes RFC 8785 prescribes.
- Arrays and objects nest at most 64 levels, the outermost being level 1. A parser's
  own limit differs between implementations, so the format fixes one.

`vectors/json.json` lists accepted and rejected texts.

## Binary encoding

Envelopes and commit headers are binary. Each has one encoding: decoding is
verification, and an implementation needs no serializer to compare bytes. A
decoder rejects trailing bytes, a wrong kind byte, unknown flag bits, and any
value the field's rule forbids; it never normalizes. Hashes are SHA-256 over the
exact bytes, with no extra delimiter.

| Type | Bytes |
| --- | --- |
| `u8`, `u16`, `u32`, `u64`, `i64` | Fixed width, big-endian; `i64` is two's complement |
| `uuid` | 16 raw bytes, a UUIDv7: version 7 and the RFC 9562 variant bits `10` |
| `digest` | 32 raw bytes |
| `str` | `u16` byte length, then that many bytes of valid UTF-8 without U+0000 |
| `bool` | `u8` 0 or 1; any other value is invalid |
| `seq` | `u64` from 1 to 2^63 - 1 |
| `time` | `i64` seconds and `u32` nanoseconds below 10^9, within years 0000 through 9999 |
| `target` | `u8` 0 for none (the root, or no link), or 1 followed by a `uuid` |
| `source` | `uuid` item ID, then `seq` item sequence |
| `content` | `u8` role count, 0 to 32, then each role as `str`, `digest` and `u64` size up to 2^63 - 1, roles in strictly increasing byte order |

Optional fields are named by a flags field, and appear after it in the order of
its bits, from bit 0 up. A field is present exactly when its bit is set.

## Event envelopes

An item envelope is:

| Part | Encoding |
| --- | --- |
| Kind | `u8` 1 |
| `event_type` | `str` |
| `item_id` | `uuid` |
| Fields | `u16` flags, then the fields below that are present |

| Bit | Field | Encoding and meaning |
| --- | --- | --- |
| 0 | `create_item` | Creates a tree item or a record; see below |
| 1 | `base_seq` | `seq`: the item's head before this commit; absent for an item created by this commit |
| 2 | `metadata_id` | `digest`: ID of the event's erasable metadata object |
| 3 | `set_content` | `content`: complete role map, described below |
| 4 | `set_parent` | `target`: the new parent folder, or the root |
| 5 | `set_name` | `str`: new portable name |
| 6 | `set_link` | `target`: the new link folder, or none to clear it |
| 7 | `merged_from` | `source`: the revision this event takes in; see Item sources |
| 8 | `set_in_trash` | `bool`: true enters TRASH, false restores LIVE |
| 9 | `set_tombstoned` | No bytes: permanently enters TOMBSTONED |
| 10 | `set_is_federated` | `bool`: mode transition |
| 11 | `set_federation_stream_id` | `str`, nonempty: federation stream |

Bits 12 to 15 are zero. `create_item` is a `u8` of flags, the `item_type` as `str`,
then its fields, and its form is the item's class:

| Class | Flags | Fields in order |
| --- | --- | --- |
| Tree item | bit 0 clear; bit 1 parent; bit 3 source | `name` (`str`), parent folder (`uuid`), `derived_from` (`source`) |
| Record | bit 0 set; bit 1 link; bit 2 key; bit 3 source | link folder (`uuid`), `key` (`str`), `derived_from` (`source`) |

Bit 2 is clear for a tree item, bits 4 to 7 are clear for both, and only the
fields whose bits are set appear, in the order of the table. A tree item without
a parent is created at the root. Inline metadata, inline content, parent commit IDs
and repository sequences do not occur in envelopes. Metadata belongs to the
event; families define how successive metadata payloads affect presentation.
Missing metadata cannot block structural replay of names, parents or lifecycle.

An attached-ref envelope is the `u8` kind 2, `event_type` (`str`),
`owner_item_id` (`uuid`), `base_ref_seq` (`u64` up to 2^63 - 1, zero for a new
ledger), and a `u8` of flags: bit 0 `metadata_id` (`digest`), bit 1 `set_content`
(`content`), the other bits clear. Refs cannot change an owner's structure. Their
ledger starts at one and is independent of item sequence.

An event's digest is SHA-256 of its envelope. `vectors/events.json` gives
envelopes, their bytes and digests, and rejected encodings. `synck cat-event`
prints an envelope as JSON for people; that JSON is not an encoding of the
format.

### Item and event types

An item type matches `[A-Z][A-Z0-9_]*`, at most 64 bytes. `ITEM` and `FILE_VERSION`
are reserved: they begin event types and are never item types.

An event type is two or more segments joined by `::`, at most 128 bytes in all.
Each segment matches `[A-Z][A-Z0-9_.]*`. The first segment is the event's family.
An item event's family is the item's type, or `ITEM` for a family event that
applies to any type. A ref event's family is its ledger's name, such as
`FILE_VERSION`, and is not the owner's type. A later segment may name a subitem.
A reader replays an unknown event type from its structural fields alone.

These are the core events. Each has one name, the item's type followed by the name
below, and `ITEM::` followed by one of these names is rejected. An event carries
`base_seq` unless its item was created in that commit, so an event with `create_item`
never has one. `create_item` may appear on any event whose family is the item's own
type, and never on an `ITEM::` event.

| Name | Items | Fields |
| --- | --- | --- |
| `CREATE` | All | `create_item`, optional `metadata_id` and `set_content` |
| `SET_NAME` | Tree items | `set_name` |
| `SET_PARENT` | Tree items | `set_parent` |
| `SET_LINK` | Records | `set_link` |
| `SET_METADATA` | All | `metadata_id` |
| `SET_CONTENT` | All with content | `set_content`, and metadata where the family requires it |
| `TRASH` | All | `set_in_trash: true` |
| `RESTORE` | All | `set_in_trash: false` |
| `TOMBSTONE` | All | `set_tombstoned` |
| `FEDERATE` | All but FOLDER and LOCAL_COMPANION | `set_is_federated`, and `set_federation_stream_id` when true |

### Structural replay

Apply commits in published `repo_seq` order and their events in index order.
An item is created exactly once at item sequence one. Every next item event
increments its sequence by one. All events targeting an existing item in one
commit carry its head at the start of that commit as `base_seq`; all events for
an item created within that commit omit the base. Ref events similarly share
the pre-commit ref head and increment it consecutively. An owner has one ref
ledger. Refs require an existing owner that is not tombstoned, and the owner may
have been created earlier in that commit.

Creation fixes item identity, type and class, and starts lifecycle LIVE and
sequencing mode LOCAL unless federated bootstrap applies. Names, links, keys and
item types do not depend on metadata presence. Creation time and author come from
the creation commit.

Replay follows fields, not names. The core-event table is what writers emit and
what families admit; a structural field on any event of the item's family applies
with the same checks, so an unknown event type replays. The exceptions are the
content rules below, and two replay rules: an event that creates an item, and an
event that changes `set_is_federated` or `set_federation_stream_id`, sets none of
`set_parent`, `set_name`, `set_link`, `set_tombstoned` and `set_in_trash`.

There are two kinds of rule. A replay rule depends only on structure, so any
verifier applies it to any history, without metadata or family knowledge. An
admission rule is what a family lets a writer add, and needs data that may be erased
or encrypted, so it is checked only where that data is readable. The FILE revision
labels are admission rules.

#### Content

`set_content` on an event with exactly two segments replaces the item's content
snapshot. On a longer event type, such as `EMAIL::ATTACHMENT::CREATE`, it adds its
objects to closure and leaves the snapshot unchanged, because a subitem is not the
item. A `FILE::CREATE`, and any two-segment event with `set_content` on a FILE, must
hold exactly `bytes`; an empty map is invalid there. A FOLDER has no content.

#### Lifecycle

| Before | `set_in_trash: true` | `set_in_trash: false` | `set_tombstoned` |
| --- | --- | --- | --- |
| LIVE | TRASH | no change | TOMBSTONED |
| TRASH | no change | LIVE | TOMBSTONED |
| TOMBSTONED | invalid | invalid | invalid |

A repeated trash or restore changes nothing, so a retried history stays valid.
Tombstoning is terminal: the item receives no
more item or ref events, in that commit or later. It keeps its name, parent, link and
content. Trash is reversible and retains history.

#### Namespace

Parents, sibling names, keys and links are checked at the end of each commit, on the
state after all its events. So a child may precede its parent in one commit, and a
name swap is legal. A parent must exist in this repository and be a FOLDER, or be the root, and
parent cycles are forbidden. Live tree-item siblings have distinct name keys. A
[tagged name](#tagged-names) is its own name key, so replay needs no key and no
configuration.

`vcs_items` must reach that final state in one step. Its unique indexes are checked
per statement, so a writer replaying a swap deletes the touched rows and reinserts
them, or uses another way that never holds a duplicate.

A LIVE item under LIVE folders is a rule of the write path, not of replay. Federated
streams apply one item at a time, so a folder in the trash may hold a live child.
Replay accepts that, and reconstruction leaves such a child out.

#### Federation

Which items accept append and which accept ingest is a write-path rule: LOCAL items
accept append and FEDERATED items accept ingest. A transition to federated requires
`set_is_federated: true` and a nonempty stream; a return to local requires false and
no stream field. A `federation_stream_id` is unique among FEDERATED items.

A `CREATE` may carry `set_is_federated: true` and `set_federation_stream_id`, both or
neither. That is federated bootstrap: the item starts FEDERATED. A FEDERATED item
may take one `FEDERATE` per commit with the stream it is already on; it changes
nothing, so a source history replayed in pieces stays valid. Outside bootstrap,
switching to federated is a local append and switching to local is a federated
ingest. At most one mode change per item per commit is permitted. A FOLDER never
federates: a shared tree replicates as a whole repository through a federation domain.
A replica following a domain files through the domain, and append refuses to set a
link, because a local link would refuse a tombstone arriving through the domain. A
copy that left the domain keeps its cursors as provenance and, once writable, files
locally.

### Tree items and records

An item's class is the form of its `create_item`, so a reader needs no type
registry and replays unknown future types. FILE, FOLDER and FEDERATED_MOUNT, which
the core defines, are tree items; the record form is invalid for them. Every
other type takes its class from history; a family that defines a type rejects
the form it does not use, and must accept both if history holds both.

A tree item is what a file system holds. It has a parent folder or the root, and
a required portable name, unique among live siblings. Its name is exactly its
on-disk name: a checkout writes it under that name or skips it, and never renames
it. Tree-item names stay in history for ever. They are visible in an unencrypted
repository; an encrypted one holds [tagged names](#tagged-names).

A record is not on disk. It has no parent and no name, and its identity is its
item ID. A title is family metadata: erasable, encrypted where the repository is,
and free to repeat. The core rejects `set_name` or `set_parent` on a record. Records never compete
with tree items, or with each other, for a name. Relations such as task to list
or event to calendar are family metadata.

An optional `key` makes a record unique. It is a nonempty string of at most 256
bytes, compared as exact bytes, fixed at creation. At the end of every commit, at
most one LIVE record has a given (item type, key), the same rule as sibling names.
A record in the trash does not hold its key, and it cannot be restored while
another live record does. A tombstoned record never holds its key. A key is
visible in every repository, including encrypted ones, because the authority
compares keys.

An optional `link` files a record in one FOLDER. Any folder may be a target; which
folders a record type may use is product policy, not format. A link has no name
and no path, and a checkout ignores it. It is structural and never encrypted, so
any reader can find what is filed in a folder. `create_item.link` sets it and
`set_link` changes or clears it. The core rejects `link` and `set_link` on tree
items. At the end of a commit, for every record that is not tombstoned:

- A link set in that commit targets a LIVE FOLDER. It may target a folder created
  in the same commit.
- Its link does not target a TOMBSTONED folder, even from the trash. Tombstoning a
  folder must, in the same commit, clear or tombstone every record still linked to
  it.
- A FEDERATED record has no link. Its history is shared, so a local link cannot
  live on it. Federating a linked record clears the link in the same commit.

A tombstone is final and keeps the record's link as it stood.

Trash does not change links: a link may point to a folder in the trash, and
restoring the folder finds its records in place. Trashing a folder together with
its linked records is one atomic commit. A link is filing, not permission; what
it grants is product policy.

A LOCAL_COMPANION record carries filing for a record this repository does not
own. Its `key` is the target record's item ID, and its `link` is the folder. It is
always LOCAL, and federation never carries it, so each repository keeps its own
filing, as Git notes attach local data to objects you do not own. In a repository
following a federation domain it has no link.

`vectors/records.json` lists commit sequences with the expected result.

### Item sources

An item can name a revision of another item as its source. `create_item.derived_from`
says the item began as that revision, as a duplicate does. `merged_from` on a later
event says the item took that revision in. Both are a source: exactly
an item ID and a positive item sequence. Any item type may have them,
and they are part of the event, so they are hashed and never change. These are replay
rules:

- A source is another item in the same repository, of any type.
- Its revision exists before the commit that names it: `item_seq` is at most that
  item's head at the end of the previous commit. Sources therefore cannot form a
  cycle. The source may be in any lifecycle state, then or later.
- `merged_from` never appears on an event with `create_item`, and an event has at most
  one. A commit may hold several.
- An event of a FEDERATED item, or one with `set_is_federated: true`, has neither,
  because the source may not exist in another repository.

The merge base of an item and a source is the last `merged_from` naming that source,
or else `derived_from` if it names that source. The core stores and checks the relation
and does not merge. The family or application decides what merging means, and records
the result as an ordinary event. Sources are not item state and are not in the state
digest. A writer never infers a source from equal content: only an explicit duplicate
or merge records one, and a commit of a working tree records none.

### FILE and FOLDER events

Version 1 of the FILE family is exactly `FILE::CREATE`, `SET_NAME`, `SET_PARENT`,
`SET_METADATA`, `SET_CONTENT`, `TRASH`, `RESTORE`, `TOMBSTONE` and `FEDERATE`. The
FOLDER family is exactly the same list without `SET_CONTENT`. A typed FILE or FOLDER
reader reports any other event type on these items as unsupported, and never guesses.

Each event holds only the fields the core table gives it, with these additions:

- `SET_PARENT`, `TRASH`, `RESTORE` and `TOMBSTONE` carry no metadata, and neither
  does `SET_NAME` in an unencrypted repository. A move and a rename are two
  events, which may share a commit.
- In an encrypted repository, `SET_NAME` and both `CREATE` events carry metadata
  holding `name`, the display name its [tag](#tagged-names) was made from, and,
  as a [complete map](#metadata-payloads), the item's other keys.
- `FILE::CREATE` and `FILE::SET_CONTENT` carry `set_content` with exactly `bytes`,
  whose size is the file's length. Their metadata is optional: a writer includes
  it only when the item has other keys.
- `FILE::CREATE` and `FILE::SET_CONTENT` metadata may hold
  [`revision`](#metadata-payloads), with optional `origin` (`LOCAL` or
  `FEDERATED`) and optional `provenance`; any other key in it is rejected. Like
  `SET_METADATA`, both events may hold any other item key, and a complete map
  repeats them all.
- `provenance` holds `source` and `committed_by` (nonempty strings),
  `human_editor_ids` (array of distinct nonempty strings) and optional
  `primary_human_editor_id` (null, or a member of that array). Any other key is
  rejected.
- `FOLDER::CREATE` carries no content, and may carry metadata. `SET_METADATA` and
  both `CREATE` events may hold keys this family does not name.
- `SET_METADATA` carries `metadata_id`, its `base_seq`, and no other optional field.
- `FEDERATE` carries no metadata or content.

### FILE byte reconstruction

A FILE is created with its first bytes: `FILE::CREATE` at item sequence one always
carries `set_content`, and later revisions use `FILE::SET_CONTENT`. Its content
snapshot has exactly `bytes`, whose entry holds the object ID and its size. In an
unencrypted repository the object is PLAIN, so its size is the file's length.
Structural moves/renames do not create new byte content.

To export a tree at a published boundary, replay all complete commits through
that boundary, retain LIVE items with LIVE ancestry, and derive paths from the
parent IDs and names. For each FILE, select its latest `set_content` through
that boundary, retrieve `bytes`, verify the object's ID and size, and output its
plaintext (see [Objects](#objects)). Missing objects are unavailable, never
zero-length substitutes. Missing metadata does not make verified application
bytes undecodable, although typed operations may require metadata.
For a single historical revision, stop the FILE ledger at its requested item
sequence. Labels resolve that same revision as specified below.

## Portable names

These rules apply to tree-item names. A tree-item name is nonempty NFC-normalized
UTF-8, at most 255 bytes. It is not `.` or `..`, contains none of
`< > : " / \ | ? *`, U+0000–U+001F or U+007F, and does not end in ASCII space or
dot.

A name also contains none of these, because each can change what a terminal or file
manager shows:

- C1 controls, U+0080–U+009F, which include the terminal control introducer U+009B.
- Line and paragraph separators, U+2028 and U+2029.
- Bidirectional embeddings, overrides and isolates, U+202A–U+202E and U+2066–U+2069,
  which can make `cod.exe` read as `exe.doc`.

Other Unicode format characters are allowed, including emoji joiners and the
left-to-right and right-to-left marks. These rules apply to tree-item names only: a
title is display text, and record keys are compared as exact bytes.

Every code point in a name must be assigned in Unicode 18.0.0, meaning its
General_Category is not `Cn`. Private-use code points are assigned; noncharacters
are not. A later Unicode release could give an unassigned code point a
decomposition or a case folding, and implementations with different tables would
then disagree on validity. Format 8 therefore pins its Unicode version, and a
newer version of Unicode is adopted only by a new repository format.

Windows device stems are forbidden, ignoring the case of ASCII letters only,
including when followed by an extension: CON, PRN, AUX, NUL, CONIN$, CONOUT$,
COM1–COM9 and LPT1–LPT9.
COM and LPT followed by superscript ¹, ² or ³ are also forbidden. Spaces at the
end of the stem before its first dot do not escape the restriction.

A name is reserved, in every folder, when its control key is `.synck` or `synck~1`.
The control key is the name's case-folded key with U+200C–U+200F, U+206A–U+206F and
U+FEFF removed. `.synck` is the control directory. `SYNCK~1` is that directory's
NTFS short name, because it is created before any repository file, and HFS+ ignores
those code points when it compares names. A name that reached one of them would
write into the control directory, so the rule does not depend on the file system a
repository is on today. Names that merely begin with `.synck`, such as
`.synckignore`, are not reserved.

Sibling uniqueness uses Unicode 18.0.0 default full case folding of the NFC
name, using CaseFolding.txt entries C and F, never locale-sensitive/Turkic
folding. `name_key` is exactly the output of that fold and is not normalized again,
even where the fold leaves NFC, as U+01F0 does. Preserve the display spelling. A
case-only rename is allowed when it does not conflict with another item. NFC
normalization uses the Unicode 18.0.0 data. The name vectors are language-neutral
JSON in `vectors/names.json`.

A proposed portable spelling normalizes NFC, replaces each forbidden character
and each unassigned code point with `_`, truncates on a UTF-8 boundary to 255
bytes, strips trailing ASCII spaces/dots, substitutes `_` for an empty result, and
prefixes device and reserved names with `_`, truncating and stripping again when
the prefix overflows. The order matters: truncation can expose a reserved name or
leave nothing. For sibling collisions append
` (2)`, ` (3)`, etc., reserving suffix bytes before truncation. A local writer
renames a working-tree entry to this spelling, NFD names included, and reports the
rename; history only ever holds the portable name.
Non-UTF-8 names are errors, never guessed encodings.

Whole-path length is not a repository constraint: it depends on the checkout's
location.

### Tagged names

An encrypted repository never stores a tree-item name in plain. Its `name` and
`set_name` hold the name's tag: `tag:` followed by the 64 lowercase hexadecimal
digits of `HMAC-SHA256(K, name_key(name))`, with HMAC from RFC 2104 and `K` the
repository's 32-byte name tag key. The display name is in that event's metadata,
under `name`. `K` comes from the product's key specification, like the epoch
keys, and never rotates: a tag must keep meaning the same name.

- A tag is its own name key. Names whose keys are equal have equal tags, so
  sibling uniqueness, case-only renames and moves keep their meaning.
- `:` is never in a portable name, so a tag and a name never meet. A tag in an
  unencrypted repository is invalid, and so is a plain name in an encrypted one.
- The event that sets a tag carries metadata. That is a replay rule. That its
  `name` is a valid portable name whose tag is the one set is an admission
  rule, checked only with `K`: a writer checks before writing, and a reader
  refuses a mismatch and never repairs it.
- A verifier without `K` verifies structure, tags included, but not names.
  Erasing the metadata leaves the tag and makes the name unavailable.
- Equal names have equal tags anywhere in the repository, and the size of the
  metadata object shows the name's length.

`vectors/name-tags.json` gives tags under a fixed key and histories that replay
without it.

## Source attribution

Normalization, case folding and assigned code points use the Unicode 18.0.0 data
from <https://www.unicode.org/Public/18.0.0/ucd/>, under the
[Unicode License V3](unicode-license.txt). A language's built-in Unicode tables
follow its own release, not this one, and they disagree on real names.
[unicode-tables.json](unicode-tables.json) holds everything a name needs from these
files, so an implementation need not parse them:

| Field | Holds |
| --- | --- |
| `unassigned` | `[low, high]` ranges of code points with General_Category Cn |
| `case_folding` | `[code, target...]` for each status C or F mapping |
| `decompositions` | `[code, target...]` for each canonical, single-level decomposition |
| `combining_classes` | `[low, high, class]` ranges of nonzero Canonical_Combining_Class |
| `composition_exclusions` | `[low, high]` ranges of Full_Composition_Exclusion |

Code points are integers. Hangul syllables compose and decompose by arithmetic and
are in no table. `scripts/generate_unicode_tables.py` derives the file from:

| File | SHA-256 |
| --- | --- |
| `UnicodeData.txt` | `0736451de439ae7baf1425136617da495e09ee5afbe6e394374db7009ea08950` |
| `DerivedNormalizationProps.txt` | `98ac7f67d985fe781e317f6182e885e94cabb0c314769e6dd73e48b226931ccd` |
| `CaseFolding.txt` | `a004797658a457bec4dc11683e39f69249ea3b595b752dbea6721c4c9f587b0d` |
| `extracted/DerivedGeneralCategory.txt` | `d6b151d2d40ee9b1876d26f417980f45ffae47b6055ccf7203cb31f07a030f94` |

## Publication checkpoints

Acceptance remains atomic per touched items and folder guards. Accepted commits
have no repository position until the separate sequencer publishes them.
Publication assigns the sequence and its checkpoint in the same transaction;
waiting for publication must not hold acceptance locks.

The checkpoint is SHA-256 over the concatenation, without separators, of:

1. The 19 ASCII bytes `synck-checkpoint-v1`.
2. The preceding checkpoint's raw 32 bytes, or the genesis value for the first commit.
3. The positive repository sequence as an unsigned 64-bit big-endian integer.
4. The commit ID's raw 32 bytes.
5. The acceptance timestamp's Unix seconds as a signed 64-bit big-endian integer.
6. Its nanosecond fraction as an unsigned 32-bit big-endian integer, below 10^9.

The genesis value is SHA-256 over the concatenation of:

1. The 16 ASCII bytes `synck-genesis-v1`.
2. The repository ID's raw 16 bytes.
3. The history generation's raw 16 bytes.
4. The repository format, 8, as an unsigned 32-bit big-endian integer.
5. One byte: 1 for an encrypted repository, otherwise 0.

Every checkpoint therefore belongs to one history of one repository. History from
another repository or generation fails at its first commit, and so does a
configuration whose `encrypted` value was changed.

Sequences start at 1 and are consecutive: 1, 2, 3, and so on. Gaps are invalid.
Leap-second timestamps are not representable by this framing. Author time is
distinct from acceptance time.
Vectors in `vectors/checkpoints.json` fix the genesis value and include an invalid
sequence gap and valid fractional time. The chain binds publication order relative to a trusted checkpoint. It does not
by itself authenticate authority or detect the removal of an unknown suffix.
A checkpoint is not a regulatory retention seal.

Publication order must be causal: a commit is never published before a commit
whose result it depends on. Acceptance time carries that order, so an
acceptance timestamp MUST be later than the acceptance time of every accepted,
unpublished commit in the repository that the accepting transaction could
depend on, even when the wall clock stepped back (take the later of the clock
and that time plus one nanosecond). Before assigning a position, the sequencer
MUST verify that the commit holding the previous event of every item and
reference ledger the commit extends is already published, and fail the batch
otherwise; it never publishes history that cannot replay.

Sequencer implementations MUST serialize publication transactions per repository:
only one such transaction may run for a repository at a time. Take that
repository's sequencer lock, select committed unsequenced commits, then assign
a consecutive batch after the last published position, each commit's position
and checkpoint in one update, in the same transaction. Hold the lock until
commit or rollback. Rollback leaves no assigned positions or checkpoints, so it
creates no gap. The lock is the implementation's: a row lock in a server
database, `BEGIN IMMEDIATE` in SQLite; it is not part of the format. Backends
with row locks may select unsequenced commits using `FOR UPDATE SKIP LOCKED`;
that does not replace the per-repository sequencer lock. Use a deterministic
batch order (acceptance timestamp, then `commit_pk`). Never allocate `repo_seq`
with `nextval()` or its equivalent in the acceptance transaction; acceptance
itself does not access the sequencer lock or checkpoints.

SQLite stores each published commit's checkpoint, as raw 32 bytes, in its
`vcs_commits` row, with its `repo_seq`. `vcs_history_generations(repo_id, generation)`
holds a UUIDv7 identifying this history generation. Copies preserve that value.
A future administrative rewrite must replace the generation, validate its new
history and rebuild checkpoints before publishing the replacement atomically.
Client cursors must bind repository ID, generation, position and checkpoint;
a generation mismatch cannot be treated as an ordinary forward extension.

## Objects

Object IDs have exactly the spelling `sha256:` followed by 64 lowercase
hexadecimal digits; readers reject uppercase rather than normalize it. All
object roles use the same identifier type. An object's ID is always SHA-256 over
its stored bytes, so any reader verifies any object without knowing its form.
There are three forms:

| Form | Used for | Stored bytes |
| --- | --- | --- |
| PLAIN | Received bytes: files, email sources, attachments, provider originals, in unencrypted repositories | The exact bytes, unframed |
| SALTED | Metadata and UTF-8 bodies, in unencrypted repositories | The salted frame |
| ENCRYPTED | Every metadata and content object, in encrypted repositories | The encrypted frame |

The repository's `encrypted` setting and the role select the form, never the
bytes: a PLAIN file can begin with any byte sequence, including a frame's magic.
In an unencrypted repository, metadata is SALTED and a `set_content` role's name
alone gives its form, whatever the item type:

| Role | Form |
| --- | --- |
| `bytes`, `source`, `provider` (received bytes) | PLAIN |
| `body`, `data` (inline text and records) | SALTED |
| `sealed_key`, and any other role | Opaque |

In an encrypted repository every role except `sealed_key` is ENCRYPTED. An opaque
object is verified by its ID alone, and so is an object nothing references. An
object referenced under two forms is checked against each.

### PLAIN

A PLAIN object's ID is the SHA-256 of the file itself, so `sha256sum` confirms it,
and identical bytes have one ID and are stored once. Plain objects are valid at any
size, including empty.

Removing a PLAIN object removes it for every item that references it, so a
writer deletes the bytes only when no live reference remains. Erasing one
reference leaves the bytes for the others. A legal erasure can override this, and
the other references then read as missing.

A writer that reuses a stored object and a collector that found it unreferenced
MUST NOT both succeed, or an accepted event would reference removed bytes. A store
with one writer at a time holds one lock across the collector's scan and removal, and
across the writer's check, reference and commit. A store with concurrent writers
keeps an object for a grace period after it last appeared to be needed: longer than
any acceptance can run, and restarted by a writer before it reuses the object.
Acceptance then proves the object durable in the same transaction as the reference,
as it does for new content.

Bytes are named by their hash, so storing the same bytes again makes every reference
to them readable again, including one that was erased. A store that must keep an
erasure refuses that object ID.

A PLAIN ID is the hash of the whole content, so whoever holds it can confirm a guess
at bytes that were erased. A short templated email is such a guess. This is the cost
of IDs that `sha256sum` confirms and of storing identical bytes once.

### Erasure

Erasing an object removes its bytes and leaves every event as it was. The reference
then reads as missing, which is valid. To make the bytes gone, delete them, then
compact: run `VACUUM` on each SQLite database that held them, followed by
`PRAGMA wal_checkpoint(TRUNCATE)`. A WHOLE_FILE object is its file. Chunks shared
with other objects stay. A list of refused IDs is the store's own state, kept outside
`repository.sqlite3`, because it is neither history nor a canonical table.

### SALTED

Guessable short text needs a salt: without one, the ID alone would confirm a guess
at a message after its content is erased. A salted object is the concatenation of:

1. The 15 ASCII bytes `SYNCK_OBJECT_V1` followed by a zero byte.
2. One codec byte: 1 for `UTF8`. Other values are invalid.
3. A fresh 32-byte salt from the operating system's cryptographic random source.
4. The application bytes, extending to the end of the object.

The header is 49 bytes. `UTF8` requires valid UTF-8 and preserves exact bytes,
including NUL, line endings and normalization. Empty objects are valid. The frame
has no size cap; individual item profiles can bound their payloads. Metadata uses
objects whose application bytes are canonical RFC 8785 JSON.

The salt is stored only inside the object, not in event envelopes. Removing an
object prevents its ID alone from confirming a content guess; this says nothing
about copies in chunks, caches or archives. Language-neutral vectors for PLAIN and
SALTED are in `vectors/objects.json`.

A conversion that rewrites an object's application bytes keeps its salt, so a
rerun gives the same IDs. The source object leaves the converted history, so no
kept object shares the salt.

### ENCRYPTED

An encrypted repository stores no plaintext metadata or content. An encrypted
object is the concatenation of:

1. The 15 ASCII bytes `SYNCK_ENCRYPTED` followed by a zero byte.
2. The encryption scheme as an unsigned 16-bit big-endian integer.
3. The key epoch as an unsigned 64-bit big-endian integer.
4. The scheme's nonce.
5. The scheme's ciphertext, extending to the end of the object.

No salt is needed: the random nonce makes each object unique and its ID
unguessable. Scheme 2 is the only scheme; schemes 0 and 1 are invalid. A new
scheme needs a new repository format. A verifier checks IDs and frames without
any key. A reader with no key for an epoch reports the content as unavailable, not
corrupt.

Scheme 2 is XChaCha20-Poly1305 (`draft-irtf-cfrg-xchacha-03`: HChaCha20 over the
ChaCha20-Poly1305 of RFC 8439) in segments, so a reader holds one segment, not the
whole object. Its nonce is 19 random bytes. Its plaintext is what the SALTED or
PLAIN form would hold after its header: file bytes, UTF-8 text or canonical JSON.
The plaintext is cut into segments of 65,536 bytes, except the last, which holds 1
to 65,536; an empty plaintext is one empty last segment. Each segment is sealed
with a 24-byte nonce made of the stored 19 bytes, the segment's index from zero as
a `u32`, and one byte that is 1 for the last segment and 0 otherwise, and adds a
16-byte tag. Every segment's associated data is the concatenation of:

1. The 18 ASCII bytes `synck-encrypted-v1`.
2. The scheme, as in the frame.
3. The key epoch, as in the frame.
4. The repository ID's raw 16 bytes.
5. The item ID's raw 16 bytes; for a ref event, the owner's.
6. The role's length as one byte, then its ASCII bytes. An event's metadata object
   uses the role `metadata`, which is reserved: no `set_content` map may use it.

An object is therefore valid at only one place: it cannot move to another
repository, item, role or epoch. Deduplication does not apply. A reader releases a
segment's plaintext only after its tag verifies, and has the whole plaintext only
after the last segment verifies; the index and the last flag refuse reordered,
dropped, truncated or extended segments.

A verifier checks a frame from its header and total length alone. Let `n` be the
ciphertext's length and `r` its remainder modulo 65,552. The frame is valid when
`n` is at least 16, `r` is 0 or at least 17, and `r` is 16 only when `n` is 16. The
plaintext is `n - 16 * ceil(n / 65,552)` bytes.

Key epoch 1 is the first repository key. Key epoch 0 means the object does not use
a repository key: its item has a `sealed_key` role, whose bytes are a sealed key
for that item alone, so that a writer can store an object it can never read
afterwards. A `sealed_key`
object is opaque and is the one stored object that is not in a frame. Keys, epochs,
rotation and sealing belong to the product's key specification, not to this format.

An encrypted repository exposes item IDs, types, tree-item [tags](#tagged-names) and parents,
record keys and links, item sources, lifecycle, operation IDs, federation stream IDs, times,
actors, order, object IDs and exact sizes, since an object is 45 bytes plus 16 per
segment longer than its plaintext. Operation IDs and stream IDs must hold no content. A commit
message in an encrypted repository must be null. `vectors/encrypted.json` fixes the
frame, associated data and ciphertext, and lists rejected variants. It gives two
verdicts: `frame_valid` needs no key, and `opens` needs one.
`vectors/encrypted-segments.json` adds objects of several segments and the
length rule.

### Object use

Working-file change detection compares plain SHA-256 digests. In an unencrypted
repository such a digest is the file's object ID, so capturing unchanged bytes
writes nothing. Chunk deduplication works below the object and never changes object
identity. Exports return the plaintext; repository copies preserve the exact
stored bytes and their IDs.

### Metadata payloads

A metadata payload is a SALTED or, in an encrypted repository, ENCRYPTED object
containing exactly one canonical JSON object, at most 1,048,576 application bytes.
Scalars and arrays at the root, noncanonical JSON, duplicate keys and numbers
outside the hashed JSON rule are rejected. A verifier cannot check the JSON of an
encrypted payload without its key. A writer uses a fresh salt even for identical JSON.
`vectors/metadata.json` fixes the JSON, stored bytes and digest independently.

An event with exactly two segments carries item metadata; a longer event type,
such as `EMAIL::ATTACHMENT::SET_METADATA`, carries its subitem's, as with
[content](#content).

Every item metadata payload is the item's complete map: every key the item holds
after the event. A key the event removes is absent, never null. An item's current
metadata is the payload of its latest event that has one; events without metadata,
such as `SET_PARENT` or `TRASH`, leave it current. An erased latest payload makes
every key unavailable, never an older value. A reader therefore needs one payload,
not the history, and there is one rule for every repository, encrypted or not.
A family may name keys that only its `CREATE` holds; see [EMAIL](#email-provider-profile-and-titles).

One key is not item state: `revision`, an object of facts about its own event
alone, such as where a FILE revision came from. The current metadata is the
latest payload without `revision`, and no later payload repeats it. Only FILE
names it, on `CREATE` and `SET_CONTENT`; elsewhere it is rejected.
A subitem's metadata follows the same rule within its own events. A subitem edit
whose metadata is erased no longer names its target, so a reader cannot hide the
value it replaced.

SQLite stores available payloads in
`vcs_metadata_objects(repo_id TEXT, object_id BLOB, payload BLOB)`, with the first
two columns forming the primary key. `object_id` is the raw 32-byte digest;
`payload` holds the complete stored object. No foreign key makes
an event depend on object presence. Missing payloads return “not present”; present
payloads with invalid framing, codec, JSON or digest are corrupt. Verification
does not repair them. Re-inserting the same ID is idempotent only for exact bytes.

Bundle format 7 permits `metadata` records immediately after its header and
before history. Each has exactly `type` (`metadata`), `object_id` (canonical
`sha256:` spelling), and `payload_hex` (lowercase hexadecimal of the whole stored
object). Duplicate IDs and metadata records after history are rejected. Import
validates the payload and installs it in the same transaction as history; an
invalid record rolls back both. Complete SQLite copies preserve these rows.
Item and ref envelopes name metadata only by `metadata_id`. Acceptance installs supplied metadata bytes in the same transaction as the
event. Readers may resolve the ID to verified bytes without changing the
envelope or its digest. Missing bytes leave the ID intact and are explicitly
reported as absent; typed interpretation requiring those fields cannot proceed.
## Content roles and closure

`set_content`, when present, is a complete map from role names to entries. An
entry is an object ID and the object's size: the length of its stored bytes, the
whole frame for a SALTED or ENCRYPTED object, at most 2^63 - 1. The map
contains at most 32 distinct roles. Each role matches `[a-z][a-z0-9_]{0,31}`. An
absent field leaves the previous content snapshot unchanged, and an empty map
clears it, under the [content rules](#content) of replay. Inline text is not event
content. In JSON, as in the state digest, a role's entry is
`{"id": "sha256:…", "size": n}` with exactly those keys.

FILE content uses exactly `bytes`; CHAT message content uses `body`. EMAIL uses
`source` and optionally `provider`; EMAIL attachments use `source`. Their metadata contains descriptors, never object references. Unknown
families use the same structural map without requiring a typed reader.

Closure scans all retained item and attached-ref events, including deleted
items and superseded revisions. It collects each `metadata_id` and every value
in `set_content`. Neither current heads nor fields inside metadata establish
closure. Resolving content into a read view cannot change the serialized event.

A present object, stored or supplied with a proposal, has exactly the size every
entry naming it declares. A mismatch is corruption, or an invalid proposal, as a
digest mismatch is. A missing object is valid thin history: its entry still gives
its size.

Verification checks canonical history independently of the disposable checkout
cache. It reports `objects`, `metadata`, `missing_objects`, and
`missing_metadata`; missing references are valid. Present invalid payloads are
corruption. A complete archive requires both missing-reference lists to be empty
and copies exact stored bytes. Reader operations needing absent payloads fail
explicitly rather than substituting empty content.

### EMAIL originals and rendering

An EMAIL is a record: an immutable message plus its state. It is not on disk, and
a link files it in a folder. An email file in a working tree is a FILE. Turning
it into an EMAIL is an explicit import that creates a linked record, and writing
an email to disk is an explicit export of its `source`. Committing a working tree
never converts one into the other.

An import preserves the original in `source`, a PLAIN object whose ID is the
SHA-256 of the original file. The metadata payload contains `mailcal` header
fields, parser version, source format/name/media type/size, attachment count and
import time. MIME attachments are subitems with their own `source` role. Generated
Markdown, HTML and RFC 822 are not persisted. The local `render_email` reader
recomputes presentation with the current parser without changing history.
Content-ID header strings in attachment metadata are MIME identifiers, not Synck
object references. Read state and flags stay outside history, per principal.

### EMAIL provider profile and titles

Version 1 of the EMAIL family is exactly `EMAIL::CREATE`, `SET_TITLE`, `SET_LINK`,
`SET_METADATA`, `TRASH`, `RESTORE`, `TOMBSTONE` and `FEDERATE`, and
`EMAIL::ATTACHMENT::CREATE`, `SET_METADATA`, `TRASH`, `RESTORE` and `TOMBSTONE`. There
is no `EMAIL::SET_CONTENT`, because a message never changes.

An email never changes, so its facts are written once. The `EMAIL::CREATE`
metadata payload holds a `mailcal` object, which only `EMAIL::CREATE` holds and no
later event repeats, and optionally `title`. The email's other metadata, today
only `title`, is a complete map in each later `SET_TITLE` or `SET_METADATA`, as for
any item; a reader takes `mailcal` from `CREATE` and the rest from the latest
payload. Renderings, repaired or not, are never history: a parser fix reaches
every email through `render_email`, with no new event.

| `mailcal` key | Value |
| --- | --- |
| `v` | 2 |
| `parser_version` | Integer |
| `parser_limits` | Object of integers: `source_bytes`, `header_bytes`, `header_count`, `mime_nesting`, `part_count`, `attachment_count`, `decoded_attachment_bytes`, `normalized_body_bytes`, `generated_rfc822_bytes`, `parser_concurrency` |
| `origin` | `{"kind": "local_import"}` for a local import, `{"kind": "upload"}` for an upload to a Hub, `{"kind": "file_conversion", "source_file_item_id", "source_content_item_seq"}` for a FILE converted in place; absent for a provider message |
| `source` | `format` (`eml` or `msg`), `filename`, `mime_type`, `size_bytes`; absent for a provider message, whose `source` is raw RFC 822 |
| `provider`, `account_id`, `provider_message_id`, `provider_thread_id` | String, or null for a local file |
| `internet_message_id`, `subject`, `in_reply_to` | String or null |
| `references` | Array of message IDs, empty when missing |
| `importance` | `low`, `normal` or `high`; absent when the source has none |
| `from`, `sender` | An address or null |
| `to`, `cc`, `bcc`, `reply_to` | Array of addresses, empty when missing |
| `sent_at`, `received_at` | RFC 3339 string or null |
| `has_attachments` | Boolean |
| `attachment_count` | Integer |
| `imported_at` | RFC 3339 string |

An address has `name` and `email`, each a string or null. Metadata describes the
content; it must not embed Synck object references.

A provider archive uses `source` for the original raw MIME and `provider` for
its original provider-native JSON, including provider snippet/body preview,
labels, categories and importance. These are received originals, not Synck's
renderings. Attachments use `EMAIL::ATTACHMENT::CREATE` events with `source` and
the EMAIL owner as `item_id`. Their metadata has a `meta` object: `filename`,
`mime_type`, `size_bytes`, `is_inline` (boolean), `content_id` and
`provider_attachment_id` (each a string or null), and optional `origin`, which is
`{"kind": "html_data_image", "version": 1}` for an image taken from the HTML body.
They are family subrecords within that item's ledger, not folder children.

An email's title is the `title` in its current metadata, and a reader shows it
exactly as stored. With none, a
reader shows the subject, or `(no subject)` when it is missing, and may show the
email's date before it. That date is display only and is never stored, so a
viewer's setting and time zone apply to it. A title is nonempty, and two emails
may share one.
Erasing an email's metadata and source erases its subject, title and text. Its
events stay, so the fact that it existed stays, and so does the source's PLAIN ID,
which can confirm a guess at the whole message. Generated provider-import commit
messages use counts and source account, never message subjects or addresses.

### FOLDER workspaces

A workspace is a FOLDER whose current metadata has one `workspace` object, with
`kind` (string) and `workflow` (object, its state). Becoming or ceasing to be a
workspace is one `FOLDER::SET_METADATA`; no other event marks it. Erasing that
metadata leaves an ordinary folder, and links to it stay valid. Workflow history
is the folder's own history: each state change is one event in it, like any other
metadata change, and no separate log is written.

### PROFILE records

A PROFILE record holds the definitions needed to interpret other items, so that
a copy of a repository carries them. Its `key` is `<item type>/<subtype>`, and its
metadata holds that subtype's field definitions. A workspace kind's profile also
holds its lifecycles. Each change is a new revision of the record, so items keep
their meaning as definitions evolve. User preferences such as saved views are not
definitions and stay outside history.

### FILE revision labels

Named versions are `FILE_VERSION::*` events in a FILE's independent ref ledger.
There are no version-set items, synthetic item identities or item-ledger version
subitems. A ref's `set_content` must be absent. Its metadata holds a UUIDv7
`version_id`, and never repeats a fact its envelope or commit holds: the owner is
the FILE, and the commit has the operation ID. The rules in this section are
admission rules, checked only where the metadata is readable.

`CREATE` adds `source_item_seq` (the positive sequence of a FILE content
revision), `kind` (`MAJOR` or `MINOR`), `label_source` (`INTERNAL` or `EXTERNAL`),
signed integer `order_key`, and optional string/null `description`. The revision
must precede the ref in accepted order, including within one commit; its commit
and event index are the revision's own, so the label never stores them. Only one
live label may reference any given revision of a FILE.

`SET_DESCRIPTION`, `SET_KIND`, `SET_LABEL_SOURCE` and `SET_ORDER` change their
named field. `SET_REVISION` requires a new `source_item_seq` and may include a
description. `TOMBSTONE` permanently removes the label from the live set.
Mutations require an existing live version.
All events in one commit use the ref head before that commit as `base_ref_seq`.
Content ID and size are read from the referenced FILE event's `bytes` entry, never
stored again in the label. Erasing that content therefore affects the revision and its label
without changing either ledger. Erasing label metadata makes the label's
presentation unavailable; structural history remains verifiable.

A writer that labels a new FILE's first revision does so in the commit that creates
it, with sequence 1, `MINOR`, `INTERNAL` and order key 1024. Live labels sort by `(order_key, creation_ref_seq, version_id)`; starting
at 0.0, MAJOR increments the major and resets the minor, MINOR increments the
minor. UUIDs and sequences remain the identities; computed numbers are display.

## Commit size

The format imposes no fixed per-commit event-count or aggregate byte cap.
Storage integers still bound representable commits.

## Commit identity

A commit header is:

| Part | Encoding |
| --- | --- |
| Kind | `u8` 3 |
| `author_id` | `str` |
| `committer_id` | `str` |
| Fields | `u8` flags: bit 0 `occurred_at` (`time`), bit 1 `message` (`str`); the other bits clear |
| `event_count` | `u32`: item and attached-ref events combined |
| `events_digest` | `digest` |

The commit ID is SHA-256 of the header. `occurred_at` is the author's time, when
the writer has one.

Each event digest is SHA-256 of its envelope. `events_digest` is SHA-256 of the
event digests concatenated in commit-event-index order. Item events precede
attached-ref events, and indexes are contiguous from zero across both; an
envelope's kind byte tells them apart.

A writer never adds a commit with no events, which is an admission rule. A reader
accepts one, because converting history can leave a commit whose position has to
stay. Such a commit changes no state. With no events to tell it from another, its
`events_digest` is SHA-256 of its `repo_seq` as a `u64`, so two of them never share
a commit ID; eight bytes are never a run of 32-byte digests.

Repository ID, repository sequence, acceptance timestamp, operation ID, and the
physical commit-row surrogate do not participate in commit identity. There is
no parent-commit field. The publication checkpoint authenticates sequence and
acceptance time separately. Changing author, committer, origin timestamp,
message, or events changes the commit ID. Provenance of an import, when
needed, goes in the message or in event metadata, never in the header. The independently
calculated vectors in `vectors/commits.json` include item-only, reference-only,
and mixed commits, distinct committer identity and nanosecond timestamps.

## History bundle boundary

The NDJSON history bundle accepts version 7 only. Its header
carries `history_generation`, a canonical UUIDv7, alongside `repo_id`, `encrypted`,
export time and source-schema provenance. Each commit record carries its `checkpoint` as a
`sha256:` digest; it has no parent-commit field. `operation_id` is a required key
whose value may be null. A non-null operation ID must be nonblank. The importer preserves the generation and reconstructs and verifies
every checkpoint from the accepted commit IDs, positions and acceptance times.

Import is atomic into an empty repository. Invalid history, a checkpoint mismatch
or a missing or wrong end record rolls back both canonical history and generation. Imported copies keep
the same generation; an empty imported history still installs the generation.
A history bundle by itself is not a complete archival export: object bytes and
all data required for reconstruction must accompany it for that purpose.

## SQLite schema version policy

Core SQLite is UTF-8, with `PRAGMA application_id = 0x53594E4B` (1398361675),
`PRAGMA user_version = 8` and `PRAGMA journal_mode = WAL`. All three persist in the
file, and a read-only reader cannot set them, so a new database sets them. A reader
refuses any other schema version and leaves it unchanged. There are no migrations.

Schema compatibility compares owned table names, STRICT typing, named columns
and their declared types, nullability, defaults, primary-key positions, unique
keys (including collations, sort direction, expressions and partial predicates),
and foreign-key columns and actions. Physical column order, index names, DDL
whitespace/comments/identifier quoting, and extra nonunique performance indexes
are not format identity. `INT` and `INTEGER` are equivalent declared types only when their SQLite key
and nullability semantics match; `INTEGER PRIMARY KEY` rowid behavior matters.
Owned views and triggers are rejected. Uniqueness expressions and defaults are
compared as SQL tokens with string literals preserved exactly. Record validators
remain responsible for semantic bounds; CHECK-clause spelling is not compared.

## State digest

The state digest is one hash of the structural state at a published position.
Replay is the only source of state, so two implementations can agree on every
commit and checkpoint hash and still disagree on what a history means. Comparing
digests finds that. Verification reports it. It is not stored in history, is not
part of any commit or checkpoint, and a reader does not need it.

For each item created through that position, in ascending order of raw item ID
bytes, form canonical JSON with exactly these fields, using null where a value is
absent: `item_id`, `item_type`, `parent_folder_id`, `name`, `name_key`, `record_key`,
`link_folder_id`, `content` (the current role map in its JSON form, `{}` when there is none),
`head_seq`, `ref_head_seq`, `lifecycle_state`, `sequencing_mode` and
`federation_stream_id`. The digest is SHA-256 over the 14 ASCII bytes
`synck-state-v1` and a line feed, followed by each item's JSON and a line feed.
`vectors/records.json` gives the digest after every accepted commit.

## Ref sequence origin

An item's attached ref ledger starts at sequence 1. Every subsequent ref event
advances it by exactly one, independently of the item's ordinary event sequence.
All ref events for one owner in a commit carry the same pre-commit
`base_ref_seq`. `initial_ref_seq` is not a field and must be rejected as unknown.

## Canonical SQLite relations

[history.sql](history.sql) defines the complete column, key, type, default and
foreign-key contract. All tables are STRICT. Each repository container contains
one `repo_id`; every canonical row belongs to it. No `local_*` or state-tree
tables belong in this database. Additional nonunique indexes are allowed, but
additional owned tables, unique constraints, views or triggers are not. A new
repository has its `vcs_history_generations` row.

- `vcs_commits` contains accepted commit headers. `commit_pk` is only a physical
  join key. A durable accepted commit has non-null `commit_id` and `event_count`;
  the nullable fields allow transaction-local construction, not incomplete
  durable commits. `events_digest` is not stored; a reader computes it from the
  events. `repo_seq` and `checkpoint` are null until publication, and set
  together.
- `vcs_events` and `vcs_ref_events` contain envelope bytes in `payload`. No
  digest is stored; a reader computes it. Row identity, event type and accepted
  sequence must agree with the envelope and its replay. An event's acceptance
  time is its commit's `created_at`. `parent_folder_id` records the parent
  after the event, as replay gives it; the parent before an event is that of
  the item's previous event.
- `vcs_items` is the canonical accepted structural head, including accepted but
  unpublished changes. Replay must reproduce its name, name key, type, parent,
  record key, link, lifecycle, item/ref heads, creation attribution and federation
  fields. A tree item has `name` and `name_key` and no `record_key` or
  `link_folder_id`. A record has no `name`, `name_key` or `parent_folder_id`.
- The next position is one greater than the last published `repo_seq` (1 before
  first publication); no stored counter repeats it. Acceptance never reads or
  locks sequencer state and never allocates `repo_seq` in the commit transaction:
  accepted commits remain durable with `repo_seq = NULL` until asynchronous
  publication. Explicit `repository finalize` verifies the published checkpoint
  chain before publishing pending commits with their original IDs. It never
  renumbers published commits. `vcs_history_generations` has one generation.
- `vcs_metadata_objects` stores available metadata objects. Absence is
  allowed; every present object must validate even if currently unreferenced.
- `vcs_federation_domains` records the accepted federation-domain cursor;
  `last_domain_seq` is nonnegative. Local-only repositories normally have no
  rows. Domain cursor metadata is operational provenance, not commit ancestry.

A published snapshot has no unsequenced commits. A live repository can validly hold
accepted, unsequenced commits; a copy of one is not a published snapshot until the
sequencer has finished.

### WHOLE_FILE storage

Every available content object is `.synck/objects/<64-lowercase-hex-digest>`.
There is no `sha256:` prefix in a filename and no fan-out directory. The file
contains the complete stored object. Validate the file as a regular file and hash its
entire contents against its name before trusting decoded application bytes.
Metadata stays in `vcs_metadata_objects` in either storage mode.

### CHUNKED storage

`object-store.sqlite3` uses `PRAGMA application_id = 0x534F424A`, `PRAGMA user_version = 2` and
[object-store.sql](object-store.sql). `store_identity` contains exactly row
`singleton = 1` with the same `repo_id` as configuration. `objects/` still exists
but content is read from the object database.

`blobs.digest` is the object's raw 32-byte ID; the historical SQL name `blobs`
does not introduce another ID type. `size_bytes` is the **stored** size.
`complete = 0` is private incomplete capture; it is not a readable object.
For `complete = 1`, join `blob_chunks` to `chunks`, sort by `ordinal`, require
consecutive ordinals starting at zero, decode each chunk and concatenate them.
The result must have exactly `size_bytes` bytes and hash to `blobs.digest`,
then pass validation for its form. A missing referenced blob is thin history;
a claimed complete blob with missing or invalid chunks is corruption.

A chunk's digest is SHA-256 of its decoded bytes.
`raw_size` is 1..262144. Codec `RAW` stores exactly that many bytes. Codec `ZSTD`
stores exactly one Zstandard frame: no trailing bytes or concatenated frames,
decoded output exactly `raw_size`, frame window at most 262144 bytes. A stored
compressed payload is nonempty and its size multiplied by 100 is less than
`raw_size * 95`. These chunk codecs are separate from a salted object's codec byte.

A chunk with a `chunk_bases` row is a delta: its codec is `ZSTD`, and its frame
is decoded with the decoded bytes of `base_digest` as a raw content dictionary.
Decode the base first, which may itself be a delta. A chain holds at most 24
deltas above a chunk without a base. A base is any stored chunk; a writer uses
the chunk in the same place in the previous version of the same file. A chunk
that is a base stays while any delta needs it, even when no blob lists it.
Version 1 is version 2 without `chunk_bases` and its index; a writer adds them
when it opens one.

Chunk boundaries have no semantic meaning, and a reader never reproduces a writer's
chunking. Copying or changing storage modes must preserve the exact stored
bytes and hence object IDs. Re-encoding chunks is allowed; replacing salts is not.

## Bundle record details

A bundle is UTF-8 newline-delimited JSON. Records need not use JCS. A
`payload_hex` is the lowercase hexadecimal of an envelope's exact bytes, and a
commit's `commit_id` must equal the hash of the header its fields and events
encode. A nonempty
header comes first, followed by zero or more metadata records, then
commit/event groups, then federation-domain records, then
exactly one end record. That section order is fixed. Within the metadata and
domain sections a writer sorts by ID, so a history has one bundle; a reader does not
depend on that order. Every record ends in a line feed. A blank body line is an empty
line and is ignored, and nothing follows the end record, not even a blank line.
Unknown fields and record types are rejected. No event may appear outside its commit
group.

All records contain `type`. Other fields are:

| Type | Fields |
| --- | --- |
| `synck_vcs_bundle_header` | `bundle_format_version: 7`, `repo_id`, `history_generation`, `encrypted`, `exported_at`, `schema: {subsystem, schema_id}` |
| `metadata` | `object_id`, `payload_hex` |
| `commit` | `commit_id`, `checkpoint`, `repo_seq`, `operation_id`, `author_id`, `committer_id`, `message`, `occurred_at`, `created_at`, `event_count` |
| `event` | `commit_event_idx`, `item_seq`, `payload_hex` |
| `ref_event` | `commit_event_idx`, `ref_seq`, `payload_hex` |
| `federation_domain` | `domain_id`, `last_domain_seq`, `created_at`, `updated_at` |
| `synck_vcs_bundle_end` | `record_count`, `checkpoint`, `digest` |

An event belongs to the commit record it follows and takes that commit's ID, position and
acceptance time. Its item or owner and its event type are those of its envelope. A record
is at most 8 MiB, line feed included. Digest IDs are lowercase.

Optional commit message, author time and operation IDs are JSON null when absent.
Digest fields use prefixed hexadecimal spelling, except `payload_hex`, which
is unprefixed lower hex of an envelope or of a complete metadata frame. Times
are text. `schema` is source
provenance, not a schema instruction: a reader records it and does not act on it.
The vectors name `subsystem: "synck_vcs_sqlite", schema_id: "core-v8"`.

The end record is how a reader knows it has the whole bundle, as a Git packfile
has a count and a trailer. `record_count` is the number of records before it,
including the header and not counting blank lines. `checkpoint` is the last commit's
checkpoint, or the genesis value for an empty history. `digest` is the `sha256:`
hash of every byte before the end record, blank lines included. A reader checks all
three before it accepts anything, and treats a bundle without an end record as
truncated. `vectors/bundle.json` fixes a bundle and lists rejected variants.

Commits are ordered by consecutive `repo_seq`, starting at 1; gaps are invalid.
Exactly `event_count` event/ref records immediately follow each commit, with contiguous
indexes from zero, `event` records first and `ref_event` records after. The domain section
must not duplicate identities. A bundle carries no item heads: the importer builds
them from structural replay, taking each item's creation time and author from its
first commit.

A repository, archive or export is complete when it holds every object its history
references, together with the container and configuration needed to interpret it. An
NDJSON bundle alone is not. A repository after erasure is valid but not complete, and
its bundle is still a valid bundle.

## Independent verification procedure

1. Check format/configuration, database identities and schema structure; select
   a consistent database snapshot. Distinguish accepted unpublished work from
   a complete published snapshot before claiming a terminal checkpoint.
2. Decode every stored event/ref envelope strictly and compute its SHA-256.
   Match its row fields.
3. For each commit, check contiguous combined indexes, item-before-ref ordering,
   exact event count, the events digest and the header hash.
   Reject duplicate commit IDs, operation IDs or publication positions.
4. Replay structure and item/ref bases, including final namespace uniqueness,
   record classes, key uniqueness, link rules, parent acyclicity and terminal
   tombstones. Compare canonical item heads and each event's stored parent, and
   report the state digest.
5. Recompute every checkpoint in order from the genesis value and acceptance
   timestamps. Compare its commit, sequence and digest, the generation and any
   externally trusted checkpoint. A locally self-consistent replacement is not
   proof of continuity with a previously trusted generation.
6. Collect all historical metadata IDs and role values. Hash every present
   object against its ID and validate its form; report absent IDs separately. Check chunk-store integrity independently
   of object availability. Do not consult private caches to establish validity.
7. Decode requested FILE revisions from verified objects. Publish extracted
   files only after validation; a streaming decoder can discover corruption late.

The language-neutral vectors under `vectors/` cover names, hashed JSON, object
forms, encryption, metadata, envelopes, tree and record rules, state digests,
commits, checkpoints, bundles and FILE labels. [vectors/README.md](vectors/README.md)
describes each file. `vectors/file-folder/` is outside the format: it holds one
writer's intent and error vocabulary, and an application view of events. The top-level `events.json` fixes accepted bytes and hashes. These
canonicalization cases follow the event type grammar but need not satisfy family
admission; they are not whole valid histories. Neither example IDs nor synthetic
digests imply object presence.

## History generations

Normal writes append. A history generation names one unbroken history: replacing
history under the same repository ID allocates a new generation, and every event,
commit and checkpoint it changes has a new hash. A format conversion is not a
replacement: it re-encodes the same facts at the same positions and keeps the
generation; its genesis differs because the genesis hashes the format. Verification never makes corrupt
data acceptable by recalculating its hashes. Old cursors fail continuity checks.
A checkpoint chain authenticates continuity only relative to a trusted checkpoint;
it neither imposes immutability nor proves who accepted a record.
