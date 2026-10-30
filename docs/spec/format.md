# Synck 0.2 repository format

This is the authoritative format-7 specification. A 0.2 reader accepts only
this repository format, commit format 6 and history bundle format 5. Schema
versions are independent of release numbers. There are no older-format readers
or migrations. The normative SQL and JSON vectors linked here are part of this
specification; no Rust source is needed to interpret a repository.

Only bytes are specified: what a repository, an event, an object and a bundle
contain, and what makes them valid. Commands, caches, checkouts and recovery belong
to the [guides](../README.md). Every rule here binds a writer, a reader and a
verifier unless it names one of them; examples and rationale do not add rules. A
present-tense rule is as binding as one spelled MUST.

## Core and item families

The core is everything a reader needs without knowing any item type: this
container, canonical JSON, event envelopes, tree and record rules, objects,
commits and checkpoints. After 1.0 it changes only with a new repository format.

Item families define event types, metadata keys and content roles. Each family
is versioned separately, in this document; no family version is stored in history.
Adding a family or an event type never changes the core. All families below are at
version 1.

Migration-only event types are forbidden. A conversion between formats happens
offline and writes the resulting history; it leaves no conversion, repair or
backfill events behind.

## Fixed validity limits

These limits belong to format 7, not to a writer configuration or a hosted
service. Every writer, reader, replay/import path and `fsck` enforces them.
Lengths are UTF-8 bytes (not characters); event size is its exact canonical
JSON payload, including escaping.

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
| Hashed JSON nesting | 64 levels |

Author and committer IDs are nonempty. A stored operation ID is null or
nonblank UTF-8, and nonblank means at least one code point that is not Unicode
White_Space. The schema's `trim()` check is a weaker backstop, not the rule. A
commit submitted through the [protocol](protocol.md) requires one; a local commit
may store null. Preserve its exact bytes, including whitespace; do not trim or
derive a different retry identity. A message is null or nonempty; an empty message
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
- `tmp/`: optional; holds the recovery intents of interrupted operations.

Any other file in `.synck/` is outside the format, and a reader ignores it. Nothing
outside the format contributes to any hash, and verification neither requires nor
trusts it. Treat repository paths as untrusted; do not follow object or database
symlinks outside the repository.

Configuration is an object with required `format_version: 7`, `repo_id`,
`authority` (`LOCAL`, `FOLLOWER`, or `INDEPENDENT_LOCAL`), optional
`storage_mode` (`WHOLE_FILE`, the default, or `CHUNKED`), `author` (string, required
unless `authority` is `FOLLOWER`), and optional `encrypted` (boolean, default false).
Unknown fields are rejected. `encrypted` is fixed when the repository is created and
copies keep it. `author` is the default author ID for new commits, at most 256 bytes;
it is not history.

`.synck/checkout.json` holds mutable checkout state, apart from the file that
identifies the repository. It is an object with `paths` (array of selected
relative paths; empty means all), `hidden_files` (`ignore`, default, or `track`)
and optional `pending`, which holds `previous_paths` (null or an array) while
transitioning its materialized paths. A missing file means the defaults. It is not
canonical history and copies do not carry it.

Authority is local operational configuration, not a cryptographic credential.
`FOLLOWER` is an imported copy, and it refuses writes. `INDEPENDENT_LOCAL` accepts
writes: a repository created here, or a copy given its own checkout. `LOCAL` accepts
writes too: a copy activated to succeed a source that has retired. Neither is a
synchronization or distributed fencing protocol.

Repository IDs, item IDs, physical `commit_pk` IDs, history generations and FILE
version IDs are UUIDv7, stored in lowercase hyphenated form. Commit IDs, object IDs, event digests and checkpoints are
`sha256:` plus 64 lowercase hexadecimal digits in JSON, and raw 32-byte digests
in SQLite BLOB columns. They are distinct domains despite sharing spelling.
Authors and committers are application identity strings, not UUIDs. A federation
domain ID is a nonempty string, opaque to the format.

Sequences are positive signed 64-bit integers in SQLite; ref bases also allow
zero. In hashed JSON they stop at 2^53 - 1, like every number. Event indexes start
at zero and event counts are positive; both fit signed 32-bit storage/API integers.
There is no smaller policy cap.

Time strings are RFC 3339 UTC. A writer uses one spelling: nine fractional digits
and `Z`. A reader compares instants, not spellings, and decodes acceptance time to
seconds and nanoseconds for checkpoint hashing. The commit's `occurred_at` is hashed,
so there only the writer's spelling is valid.

## Canonical JSON

Hashed JSON is RFC 8785 JCS, encoded as UTF-8: no whitespace, object keys sorted
by UTF-16 code units, minimal prescribed string escapes, and no duplicate keys
or invalid Unicode. Strings are not normalized by JCS. Name normalization is a
separate admission rule.

Hashed JSON is a strict subset of RFC 8785, so that no implementation has to
serialize floating-point numbers identically:

- A number is an integer from -(2^53 - 1) to 2^53 - 1, in plain decimal. Fractions,
  exponents and negative zero are rejected. A decimal value is a string.
- A string or key must not contain U+0000. Other control characters use the
  escapes RFC 8785 prescribes.
- Arrays and objects nest at most 64 levels, the outermost being level 1. A parser's
  own limit differs between implementations, so the format fixes one.

`vectors/json.json` lists accepted and rejected texts.

For stored events, parse the strict shape below, serialize that shape canonically,
and require equality with the stored bytes. Unknown fields, explicit nulls where
an optional field must be omitted, noncanonical IDs and alternative spellings
cannot be silently removed or normalized before verifying their hash. Hashes
are SHA-256 without any extra delimiter or newline except where explicitly framed.

## Event envelopes

Every accepted item envelope is a JSON object with required `item_id` and
`event_type`. These are the only optional fields:

| Field | Meaning |
| --- | --- |
| `create_item` | Creates a tree item or a record; see below |
| `base_seq` | The item's positive head sequence before this commit; absent for an item created by this commit |
| `metadata_id` | ID of the event's erasable metadata object |
| `set_content` | Complete role map described below |
| `set_parent` | Folder UUIDv7, or explicit null to move to root; absence leaves the parent unchanged |
| `set_name` | New portable name |
| `set_link` | Folder UUIDv7, or explicit null to clear the link; absence leaves it unchanged |
| `merged_from` | Source revision this event takes in; see Item sources |
| `set_in_trash` | Boolean; true enters TRASH, false restores LIVE |
| `set_tombstoned` | Boolean; true permanently enters TOMBSTONED |
| `set_is_federated` | Boolean mode transition |
| `set_federation_stream_id` | Nonempty federation stream string |

`create_item` has one of two forms, and the form is the item's class:

| Class | Form |
| --- | --- |
| Tree item | `{ "item_type": string, "name": string, "parent_folder_id"?: UUIDv7, "derived_from"?: source }`; omitted parent means root |
| Record | `{ "item_type": string, "key"?: string, "link"?: UUIDv7, "derived_from"?: source }` |

All other absent optional values are omitted, not null. Nested `create_item`
also rejects unknown fields. Inline metadata, inline content, parent commit IDs
and repository sequences do not occur in envelopes. Metadata belongs to the
event; families define how successive metadata payloads affect presentation.
Missing metadata cannot block structural replay of names, parents or lifecycle.

An attached-ref envelope has required `owner_item_id`, `event_type`,
`base_ref_seq`, and optional `metadata_id` and `set_content`. No other fields
are valid. Refs cannot change an owner's structure. Their ledger starts at one
and is independent of item sequence; an initial ref base is zero.

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
| `TOMBSTONE` | All | `set_tombstoned: true` |
| `FEDERATE` | All | `set_is_federated`, and `set_federation_stream_id` when true |

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
content rules below.

There are two kinds of rule. A replay rule depends only on structure, so any
verifier applies it to any history, without metadata or family knowledge. An
admission rule is what a family lets a writer add, and needs data that may be erased
or encrypted, so it is checked only where that data is readable. `content.size` and
the FILE revision labels are admission rules.

#### Content

`set_content` on an event with exactly two segments replaces the item's content
snapshot. On a longer event type, such as `EMAIL::ATTACHMENT::CREATE`, it adds its
objects to closure and leaves the snapshot unchanged, because a subitem is not the
item. A `FILE::CREATE`, and any two-segment event with `set_content` on a FILE, must
hold exactly `bytes`; `{}` is invalid there. A FOLDER has no content.

#### Lifecycle

| Before | `set_in_trash: true` | `set_in_trash: false` | `set_tombstoned: true` |
| --- | --- | --- | --- |
| LIVE | TRASH | no change | TOMBSTONED |
| TRASH | no change | LIVE | TOMBSTONED |
| TOMBSTONED | invalid | invalid | invalid |

A repeated trash or restore changes nothing, so a retried history stays valid.
`set_tombstoned: false` is invalid. Tombstoning is terminal: the item receives no
more item or ref events, in that commit or later. It keeps its name, parent, link and
content. Trash is reversible and retains history.

#### Namespace

Parents, sibling names, keys and links are checked at the end of each commit, on the
state after all its events. So a child may precede its parent in one commit, and a
name swap is legal. A parent must exist in this repository and be a FOLDER; root is null, and
parent cycles are forbidden. Live tree-item siblings have distinct name keys.

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
neither. That is federated bootstrap: the item starts FEDERATED, and one more
`FEDERATE` with the same stream is allowed in that commit. Outside bootstrap,
switching to federated is a local append and switching to local is a federated
ingest. At most one mode change per item per commit is permitted.

### Tree items and records

Every item type belongs to exactly one class. A type in the table below has that
class, and the other form is invalid for it. A type outside the table has the class
of its `create_item` form, so a reader needs no type registry and replays unknown
future types. A later family that adopts such a type must accept both forms if
history holds both, or it needs a new repository format.

| Class | Item types |
| --- | --- |
| Tree item | FILE, FOLDER, FEDERATED_MOUNT |
| Record | EMAIL, TASK_LIST, TASK, CHAT, LOCAL_COMPANION, CALENDAR, CALENDAR_EVENT, CALL, POST, DATABASE, TABLE, TIME_SETTINGS, TIME_BOOK, TIME_ENTRY, PROFILE, CONTACT, USER_STATE, FEDERATION_SCOPE |

A tree item is what a file system holds. It has a parent folder or the root, and
a required portable name, unique among live siblings. Its name is exactly its
on-disk name: a checkout writes it under that name or skips it, and never renames
it. Tree-item names are visible in every repository and stay in history for ever.

A record is not on disk. It has no parent and no name, and its identity is its
item ID. A title is family metadata: erasable, encrypted where the repository is,
and free to repeat. The core rejects `parent_folder_id` in a record's
`create_item` and `set_name` or `set_parent` on a record. Records never compete
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
items. At the end of a commit:

- A link set in that commit targets a LIVE FOLDER. It may target a folder created
  in the same commit.
- No record that is not tombstoned links to a TOMBSTONED folder, including a
  record in the trash. Tombstoning a folder must, in the same commit, clear or
  tombstone every record still linked to it.
- No FEDERATED item has a link. A federated record's history is shared, so a
  local link cannot live on it. Federating a linked record clears the link in the
  same commit.

Trash does not change links: a link may point to a folder in the trash, and
restoring the folder finds its records in place. Trashing a folder together with
its linked records is one atomic commit. A link is filing, not permission; what
it grants is product policy.

A LOCAL_COMPANION record carries filing for a record this repository does not
own. Its `key` is the target record's item ID, and its `link` is the folder. It is
always LOCAL, and federation never carries it, so each repository keeps its own
filing, as Git notes attach local data to objects you do not own.

`vectors/records.json` lists commit sequences with the expected result.

### Item sources

An item can name a revision of another item as its source. `create_item.derived_from`
says the item began as that revision, as a duplicate does. `merged_from` on a later
event says the item took that revision in. Both are a source: exactly
`{ "item_id": UUIDv7, "item_seq": positive integer }`. Any item type may have them,
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

- `SET_NAME`, `SET_PARENT`, `TRASH`, `RESTORE` and `TOMBSTONE` carry no metadata.
  A move and a rename are two events, which may share a commit.
- `FILE::CREATE` and `FILE::SET_CONTENT` carry `set_content` with exactly `bytes`, and
  metadata with `content` holding exactly `size`.
- `FILE::SET_CONTENT` metadata may also hold `upload_intent_id` (nonempty string),
  `revision_origin` (`LOCAL` or `FEDERATED`) and `provenance`. Any other key is
  rejected.
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
snapshot has exactly `bytes: object_id`. Available revision metadata has
`content.size`, the nonnegative application-byte length. Structural moves/renames
do not create new byte content.

To export a tree at a published boundary, replay all complete commits through
that boundary, retain LIVE items with LIVE ancestry, and derive paths from the
parent IDs and names. For each FILE, select its latest `set_content` through
that boundary, retrieve `bytes`, verify the object's ID, and output its plaintext
(see [Objects](#objects)). Verify `content.size`
when the corresponding metadata is present. Missing objects are unavailable,
never zero-length substitutes. Missing size metadata does not make verified
application bytes undecodable, although typed operations may require metadata.
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
then disagree on validity. Format 7 therefore pins its Unicode version, and a
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
and each unassigned code point with `_`, strips trailing ASCII spaces/dots, substitutes `_` for an empty result,
prefixes device and reserved names with `_`, and truncates on a UTF-8 boundary to 255 bytes.
Strip any space or dot exposed by truncation. For sibling collisions append
` (2)`, ` (3)`, etc., reserving suffix bytes before truncation. Filesystem
renaming requires explicit `--fix-names`; validation alone never renames files.
Non-UTF-8 names are errors, never guessed encodings.

A working-tree name in NFD is not an error. macOS and Mac-made archives produce
such names, and refusing or renaming them would fight the file system. The
repository stores the NFC name, and the disposable local index records the
on-disk spelling. Checkout writes new files as NFC and leaves an existing NFD file
in place, never creating a twin. A file system that keeps the NFC and NFD spellings
of one name apart holds two files with one repository name, and that is a name
conflict. The other name rules still fail as they do for any file.

Whole-path length is not a repository constraint: it depends on the checkout's
location.

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
4. The commit ID's raw 32 digest bytes, without its `sha256:` prefix.
5. The acceptance timestamp's Unix seconds as a signed 64-bit big-endian integer.
6. Its nanosecond fraction as an unsigned 32-bit big-endian integer, below 10^9.

The genesis value is SHA-256 over the concatenation of:

1. The 16 ASCII bytes `synck-genesis-v1`.
2. The repository ID's raw 16 bytes.
3. The history generation's raw 16 bytes.
4. The repository format, 7, as an unsigned 32-bit big-endian integer.
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
only one such transaction may run for a repository at a time. Lock that
repository's activity head, select committed unsequenced commits, validate the
counter against the last published position, then assign a consecutive batch,
publish every checkpoint and advance the counter in the same transaction. Hold
the activity-head lock until commit or rollback. Rollback leaves no assigned
positions, counter advancement or checkpoints, so it creates no gap. Backends
with row locks may select unsequenced commits using `FOR UPDATE SKIP LOCKED`;
that does not replace the per-repository sequencer lock. Use a deterministic
batch order (acceptance timestamp, then `commit_pk`). Never allocate `repo_seq`
with `nextval()` or its equivalent in the acceptance transaction. SQLite uses
`BEGIN IMMEDIATE` to serialize its writers; acceptance itself does not access
the activity head or checkpoints.

SQLite publishes `vcs_checkpoints(repo_id, repo_seq, commit_id, checkpoint)`
with raw 32-byte digest values. `vcs_history_generations(repo_id, generation)`
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
| PLAIN | File bytes, email sources and attachments, in unencrypted repositories | The exact bytes, unframed |
| SALTED | Metadata and UTF-8 bodies, in unencrypted repositories | The salted frame |
| ENCRYPTED | Every metadata and content object, in encrypted repositories | The encrypted frame |

The repository's `encrypted` setting and the role select the form, never the
bytes: a PLAIN file can begin with any byte sequence, including a frame's magic.
In an unencrypted repository, metadata is SALTED and a `set_content` role has the
first form that applies:

| Item type | Role | Form |
| --- | --- | --- |
| Any | `sealed_key` | Opaque |
| FILE | `bytes` | PLAIN |
| EMAIL, CHAT | `source`, including on attachments | PLAIN |
| FILE, FOLDER, EMAIL, CHAT, TASK_LIST, TASK, POST, CALENDAR, CALENDAR_EVENT, CALL | Any other | SALTED |
| Any other | Any | Opaque |

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

### ENCRYPTED

An encrypted repository stores no plaintext metadata or content. An encrypted
object is the concatenation of:

1. The 15 ASCII bytes `SYNCK_ENCRYPTED` followed by a zero byte.
2. The encryption scheme as an unsigned 16-bit big-endian integer.
3. The key epoch as an unsigned 64-bit big-endian integer.
4. The scheme's nonce.
5. The scheme's ciphertext, extending to the end of the object.

No salt is needed: the random nonce makes each object unique and its ID
unguessable. Schemes are versioned apart from the core. A new scheme needs no core
change, and an old scheme stays readable for ever, because history is permanent.
Scheme 0 is invalid. A verifier checks IDs and frames without any key. A reader
with no key for a scheme or epoch reports the content as unavailable, not corrupt.

Scheme 1 is XChaCha20-Poly1305 (`draft-irtf-cfrg-xchacha-03`: HChaCha20 over the
ChaCha20-Poly1305 of RFC 8439), with a 24-byte random nonce and a 16-byte tag at the
end of the ciphertext. Its plaintext is what the SALTED or PLAIN form would
hold after its header: file bytes, UTF-8 text or canonical JSON. Its associated
data is the concatenation of:

1. The 18 ASCII bytes `synck-encrypted-v1`.
2. The scheme, as in the frame.
3. The key epoch, as in the frame.
4. The repository ID's raw 16 bytes.
5. The item ID's raw 16 bytes; for a ref event, the owner's.
6. The role's length as one byte, then its ASCII bytes. An event's metadata object
   uses the role `metadata`, which is reserved: no `set_content` map may use it.

An object is therefore valid at only one place: it cannot move to another
repository, item, role or epoch. Deduplication does not apply.

Key epoch 1 is the first repository key. Key epoch 0 means the object does not use
a repository key: its item has a `sealed_key` role, whose bytes are a sealed key
for that item alone, so that a writer can store an object it can never read
afterwards. A `sealed_key`
object is opaque and is the one stored object that is not in a frame. Keys, epochs,
rotation and sealing belong to the product's key specification, not to this format.

An encrypted repository exposes item IDs, types, tree-item names and parents,
record keys and links, item sources, lifecycle, operation IDs, federation stream IDs, times,
actors, order, object IDs and exact sizes, since a scheme 1 object is 66 bytes longer
than its plaintext. Operation IDs and stream IDs must hold no content. A commit
message in an encrypted repository must be null. `vectors/encrypted.json` fixes the
frame, associated data and ciphertext, and lists rejected variants. It gives two
verdicts: `frame_valid` needs no key, and `opens` needs one.

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

A key's current value is found by walking back from the item's latest payload, among
the events a family names for it: the first payload holding the key wins. An erased
payload stops the walk and the key is unavailable, never an older value.

SQLite stores available payloads in
`vcs_metadata_objects(repo_id TEXT, object_id BLOB, payload BLOB)`, with the first
two columns forming the primary key. `object_id` is the raw 32-byte digest;
`payload` holds the complete stored object. No foreign key makes
an event depend on object presence. Missing payloads return “not present”; present
payloads with invalid framing, codec, JSON or digest are corrupt. Verification
does not repair them. Re-inserting the same ID is idempotent only for exact bytes.

Bundle format 5 permits `metadata` records immediately after its header and
before history. Each has exactly `type` (`metadata`), `object_id` (canonical
`sha256:` spelling), and `payload_hex` (lowercase hexadecimal of the whole stored
object). Duplicate IDs and metadata records after history are rejected. Import
validates the payload and installs it in the same transaction as history; an
invalid record rolls back both. Complete SQLite copies preserve these rows.
Item and ref envelopes use the optional `metadata_id` field, containing the
canonical object ID string. Inline `set_metadata` is rejected on the wire.
Acceptance installs supplied metadata bytes in the same transaction as the
event. Readers may resolve the ID to verified bytes without changing the
envelope or its digest. Missing bytes leave the ID intact and are explicitly
reported as absent; typed interpretation requiring those fields cannot proceed.
`vectors/events.json` defines canonical envelopes, event digests, tagged event
sequences and commit hashes, including envelopes with metadata references.

## Content roles and closure

`set_content`, when present, is a complete map from role names to canonical
object ID strings. It contains at most 32 distinct roles. Each role matches
`[a-z][a-z0-9_]{0,31}`; duplicate names, invalid IDs, null values and nested values
are rejected. An omitted field leaves the previous content snapshot unchanged, and
an empty map clears it, under the [content rules](#content) of replay. Inline text
and JSON are not event content.

FILE content uses exactly `bytes`; CHAT message content uses `body`. EMAIL uses
`source` and optionally `provider`; EMAIL attachments use `source`. Their metadata contains descriptors, never object references. Unknown
families use the same structural map without requiring a typed reader.

Closure scans all retained item and attached-ref events, including deleted
items and superseded revisions. It collects each `metadata_id` and every value
in `set_content`. Neither current heads nor fields inside metadata establish
closure. Resolving content into a read view cannot change the serialized event.

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

The `EMAIL::CREATE` metadata payload holds a `mailcal` object, and optionally `title`:

| `mailcal` key | Value |
| --- | --- |
| `v` | 2 |
| `parser_version` | Integer |
| `parser_limits` | Object of integers: `source_bytes`, `header_bytes`, `header_count`, `mime_nesting`, `part_count`, `attachment_count`, `decoded_attachment_bytes`, `normalized_body_bytes`, `generated_rfc822_bytes`, `parser_concurrency` |
| `origin` | `{"kind": "local_import"}` for a local import |
| `source` | `format` (`eml` or `msg`), `filename`, `mime_type`, `size_bytes` |
| `provider`, `account_id`, `provider_message_id`, `provider_thread_id` | String, or null for a local file |
| `internet_message_id`, `subject` | String or null |
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

An email's title is the last `title` string in its `EMAIL::CREATE` or
`EMAIL::SET_TITLE` metadata, and a reader shows it exactly as stored. With none, a
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
subitems. A ref's `set_content` must be absent. Its metadata contains
`file_item_id` (equal to its envelope owner) and a UUIDv7 `version_id`. The rules
in this section are admission rules, checked only where the metadata is readable.

`CREATE` adds `operation_id` (a nonempty string unique in the repository),
`source_item_seq` (the positive sequence of a FILE content revision), `kind`
(`MAJOR` or `MINOR`), `label_source` (`INTERNAL` or `EXTERNAL`), signed integer
`order_key`, and optional string/null `description`. Optional `source_commit_id`
and `source_commit_event_idx` must each agree with the referenced revision. A
revision in the same commit has no `source_commit_id`, because a commit cannot name
itself. The revision must precede the ref in accepted order, including within one
commit.
Only one live label may reference any given revision of a FILE.

`SET_DESCRIPTION`, `SET_KIND`, `SET_LABEL_SOURCE` and `SET_ORDER` change their
named field. `SET_REVISION` requires a new `source_item_seq` and may include
matching commit coordinates and a description. `TOMBSTONE` permanently removes
the label from the live set. Mutations require an existing live version.
All events in one commit use the ref head before that commit as `base_ref_seq`.
Content ID and size are read from the referenced FILE event, never stored again
in the label. Erasing that content therefore affects the revision and its label
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

Commit format 6 is the only accepted encoding. The canonical header is RFC 8785
JSON with exactly these fields (including explicit nulls for absent values):
`object_type` (`vcs_commit`), `format_version` (6), `author_id`, `committer_id`,
`occurred_at`, `message`, `event_count`, and `events_digest`.
The optional origin timestamp is UTC RFC 3339 with exactly nine fractional digits
and `Z`; leap seconds are rejected. The positive event count is the number of item and attached-reference
events combined. The commit ID is SHA-256 of these canonical UTF-8 bytes,
represented as `sha256:` followed by 64 lowercase hexadecimal digits.

Each event digest is SHA-256 of its canonical envelope bytes. The aggregate
`events_digest` hashes the concatenation, in commit-event-index order, of one
family byte followed by the raw 32-byte event digest. The family byte is 0 for
an item event and 1 for an attached-reference event. Item events precede attached
reference events, and indexes are contiguous from zero across both families.
Every commit uses this framing, including commits with no references.

Repository ID, repository sequence, acceptance timestamp, operation ID, and the
physical commit-row surrogate do not participate in commit identity. There is
no parent-commit field. The publication checkpoint authenticates sequence and
acceptance time separately. Changing author, committer, origin timestamp,
message, or events changes the commit ID. Provenance of an import, when
needed, goes in the message or in event metadata, never in the header. The independently
calculated vectors in `vectors/commits.json` include item-only, reference-only,
and mixed commits, distinct committer identity and nanosecond timestamps.

## History bundle boundary

The NDJSON history bundle accepts version 5 only, with commit format 6. Its header
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
`PRAGMA user_version = 7` and `PRAGMA journal_mode = WAL`. All three persist in the
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
`link_folder_id`, `content` (the current role map, `{}` when there is none),
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
repository has its `vcs_history_generations` row, and a `vcs_repo_activity_heads`
row with `next_repo_seq` 1.

- `vcs_commits` contains accepted commit headers. `commit_pk` is only a physical
  join key. A durable accepted commit has non-null `commit_id`, `events_digest`
  and `event_count`; the nullable fields allow transaction-local construction,
  not incomplete durable commits. `repo_seq` remains null until publication.
- `vcs_events` and `vcs_ref_events` contain canonical envelope bytes in `payload`,
  with their raw SHA-256 in `event_payload_digest`. Row identity, event type and
  accepted sequence must agree with the envelope and its replay. `created_at`
  equals the owning commit's acceptance time. `parent_folder_id` records the
  parent after the event. `parent_folder_id_before` records the old parent only
  when the envelope contains `set_parent`; otherwise it is null. For creation
  it is null. A move from root also has a null before-parent. This column is a
  move notification field, not an unconditional snapshot of the prior parent.
- `vcs_items` is the canonical accepted structural head, including accepted but
  unpublished changes. Replay must reproduce its name, name key, type, parent,
  record key, link, lifecycle, item/ref heads, creation attribution and federation
  fields. A tree item has `name` and `name_key` and no `record_key` or
  `link_folder_id`. A record has no `name`, `name_key` or `parent_folder_id`.
- `vcs_repo_activity_heads.next_repo_seq` is exactly one greater than the last
  published position (1 before first publication). Acceptance never reads or
  locks sequencer state and never allocates `repo_seq` in the commit transaction:
  accepted commits remain durable with `repo_seq = NULL` until asynchronous
  publication. Only the sequencer checks the counter against published history
  before assigning a batch. A mismatch stops publication with an integrity
  error; it does not invalidate or reject acceptance. Explicit `repository
  finalize` verifies the published checkpoint chain and recomputes this derived
  counter transactionally before publishing pending commits with their original
  IDs. It never renumbers published commits. `updated_at` is operational allocation
  time. `vcs_checkpoints` has exactly one matching row per published commit and
  none for unpublished commits. `vcs_history_generations` has one generation.
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

`object-store.sqlite3` uses `PRAGMA application_id = 0x534F424A`, `PRAGMA user_version = 1` and
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

Chunk boundaries have no semantic meaning, and a reader never reproduces a writer's
chunking. Copying or changing storage modes must preserve the exact stored
bytes and hence object IDs. Re-encoding chunks is allowed; replacing salts is not.

## Bundle record details

A bundle is UTF-8 newline-delimited JSON. Outer records need not use JCS; embedded
`payload_json` strings must contain exact canonical envelope bytes. A nonempty
header comes first, followed by zero or more metadata records, then
commit/event groups, then item-head records, then federation-domain records, then
exactly one end record. That section order is fixed. Within the metadata, item and
domain sections a writer sorts by ID, so a history has one bundle; a reader does not
depend on that order. Every record ends in a line feed. A blank body line is an empty
line and is ignored, and nothing follows the end record, not even a blank line.
Unknown fields and record types are rejected. No event may appear outside its commit
group or after the item section.

All records contain `type`. Other fields are:

| Type | Fields |
| --- | --- |
| `synck_vcs_bundle_header` | `bundle_format_version: 5`, `repo_id`, `history_generation`, `encrypted`, `exported_at`, `schema: {subsystem, schema_id}`, `commit_format_version: 6` |
| `metadata` | `object_id`, `payload_hex` |
| `commit` | `commit_id`, `checkpoint`, `repo_seq`, `operation_id`, `format_version: 6`, `author_id`, `committer_id`, `message`, `occurred_at`, `created_at`, `event_count`, `events_digest` |
| `event` | `commit_id`, `repo_seq`, `commit_event_idx`, `item_id`, `item_seq`, `event_type`, `payload_json`, `event_payload_digest`, `created_at`, `parent_folder_id`, `parent_folder_id_before` |
| `ref_event` | `commit_id`, `repo_seq`, `commit_event_idx`, `owner_item_id`, `ref_seq`, `event_type`, `payload_json`, `event_payload_digest`, `created_at` |
| `item` | `item_id`, `item_type`, `parent_folder_id`, `name`, `name_key`, `record_key`, `link_folder_id`, `head_seq`, `ref_head_seq`, `lifecycle_state`, `created_at`, `created_by`, `sequencing_mode`, `federation_stream_id` |
| `federation_domain` | `domain_id`, `last_domain_seq`, `created_at`, `updated_at` |
| `synck_vcs_bundle_end` | `record_count`, `checkpoint`, `digest` |

Optional commit message, author time, operation IDs, parents, names, keys, links, sources
and stream IDs are JSON null when absent. `ref_head_seq` may be omitted when zero.
Digest fields use prefixed hexadecimal spelling, except `payload_hex`, which
is unprefixed lower hex of the complete metadata frame. `schema` is source
provenance, not a schema instruction: a reader records it and does not act on it.
The vectors name `subsystem: "synck_vcs_sqlite", schema_id: "core-v7"`.

The end record is how a reader knows it has the whole bundle, as a Git packfile
has a count and a trailer. `record_count` is the number of records before it,
including the header and not counting blank lines. `checkpoint` is the last commit's
checkpoint, or the genesis value for an empty history. `digest` is the `sha256:`
hash of every byte before the end record, blank lines included. A reader checks all
three before it accepts anything, and treats a bundle without an end record as
truncated. `vectors/bundle.json` fixes a bundle and lists rejected variants.

Commits are ordered by consecutive `repo_seq`, starting at 1; gaps are invalid.
Exactly `event_count` event/ref records immediately follow each commit, with contiguous
indexes from zero, item records first and ref records after. Their repeated
commit ID, position and acceptance instant must match. Item and domain sections
must not duplicate identities. Every replayed item has exactly one item record, and
its fields must match structural replay.

A repository, archive or export is complete when it holds every object its history
references, together with the container and configuration needed to interpret it. An
NDJSON bundle alone is not. A repository after erasure is valid but not complete, and
its bundle is still a valid bundle.

## Independent verification procedure

1. Check format/configuration, database identities and schema structure; select
   a consistent database snapshot. Distinguish accepted unpublished work from
   a complete published snapshot before claiming a terminal checkpoint.
2. Parse every stored event/ref envelope with the strict shape and JCS rules;
   require exact canonical bytes and recompute its SHA-256. Match its row fields.
3. For each commit, check contiguous combined indexes, item-before-ref ordering,
   positive exact event count, tagged aggregate digest and canonical header hash.
   Reject duplicate commit IDs, operation IDs or publication positions.
4. Replay structure and item/ref bases, including final namespace uniqueness,
   record classes, key uniqueness, link rules, parent acyclicity and terminal
   tombstones. Compare canonical item heads and report the state digest.
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
commit and checkpoint it changes has a new hash. Verification never makes corrupt
data acceptable by recalculating its hashes. Old cursors fail continuity checks.
A checkpoint chain authenticates continuity only relative to a trusted checkpoint;
it neither imposes immutability nor proves who accepted a record.
