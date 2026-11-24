# Central synchronization protocol 2

This transport-neutral contract uses [format 8](format.md) and its objects.
`synck-sync` provides the wire encoding, streaming verification and
admission-policy helpers. Authentication, durable staging, cleanup, transaction
installation and Core acceptance belong to the authority/client adapters.
`portable::ServedRepository` in `synck-local` is the repository side of a
server: acceptance, history pages, objects and leases. Library tests are not
evidence that a deployed service exists.

## Identity and complete cursors

`RepositoryIdentity` is a repository ID and a history generation, both UUIDv7.
Generation identifies one ordered history. Administrative history replacement
changes the generation; it never silently resets a client's state or erases
pending work.

A `Cursor` is a repository identity, a `repo_seq` and the checkpoint at it.
Genesis has sequence zero and the repository's genesis value as its checkpoint.
Published positions start at 1, with no gaps. Every next header must have the
previous sequence plus one; reject missing positions even if the checkpoint
hash matches. A cursor authenticates continuity from an already trusted
checkpoint, not the remote party's identity. Validate it against stored history.

## Trust model

The authority orders history and clients trust it for what they cannot check.
A client holding a cursor can prove:

- **No rewrite.** Every later commit extends that cursor's checkpoint. An edited,
  dropped or reordered commit already seen fails continuation with `CHECKPOINT`.
- **No silent replacement.** Replacing history needs a new generation, which the
  client sees.
- **Exact content.** Each envelope decodes strictly and hashes to its digest;
  each commit ID is the hash of its header.

A client cannot prove:

- **One history for everyone.** The authority can show two clients histories that
  split after a common checkpoint. Each is valid alone; only comparing
  checkpoints reveals the split.
- **Completeness.** The authority can withhold later commits and keep serving a
  valid older prefix.
- **Authorship.** `author_id` and `committer_id` are the authority's record.
  Commits carry no author signature, so the authority, or anyone who can write
  its storage, can record a valid commit under any author.

Signed checkpoints and author signatures over commit IDs would close these gaps
without changing commit identity. Neither is part of protocol 2.

## Routes

All routes are under `/v2/repositories/{repo_id}/sync`. A Hub serves every
route; the reference `synck serve` answers the state reads (`/state`,
`/items/histories`, `/commits`) `NOT_FOUND`:

| Method and suffix | Request / result |
| --- | --- |
| `GET /capabilities` | `Capabilities` |
| `PUT /uploads/{operation_id}/pages/{offset}` | `ProposalPage` / `PageAck` |
| `POST /proposals` | `SubmitRequest` / `SubmitResult` |
| `GET /receipts/{operation_id}` | `Receipt`, or `NOT_FOUND` |
| `POST /history` | `HistoryRequest` / `HistoryPage` |
| `POST /state` | `StateRequest` / `StatePage` |
| `POST /items/histories` | `ItemHistoriesRequest` / `ItemHistoriesPage` |
| `POST /commits` | `CommitLogRequest` / `CommitLogPage` |
| `PUT /objects` | object pack / 204 |
| `POST /objects` | `ObjectRequest` / object pack |
| `GET /leases` | `Leases` |
| `POST /leases` | `LeaseRequest` / `Lease` |
| `POST /leases/renew` | `LeaseRenewal` / 204 |
| `POST /leases/release` | `LeaseToken` / `Released` |
| `POST /leases/break` | `LeasePaths` / `Released` |

Bodies are `application/octet-stream` in the encoding below. HTTP deployments
require authenticated TLS and authorization on every route, including receipts
and objects. IDs are not access tokens. Operations and staging are scoped to
repository, generation and authenticated owner. An operation ID follows the
format's rule (nonblank UTF-8, at most 256 bytes); it travels percent-encoded as
one URL path segment and is never a filesystem path. Do not log content.

## Encoding

Messages use the format's [binary encoding](format.md#binary-encoding) and its
strict decoding: a receiver refuses trailing bytes, unknown tags and flag bits,
and any value a field's rule forbids. Beyond the format's types:

| Type | Bytes |
| --- | --- |
| `opt T` | `u8` 0 for absent, or 1 followed by `T` |
| `list T` | `u32` count, then each `T` |
| `blob` | `u32` length, then that many bytes |
| `envelope` | `blob` holding one item or attached-ref envelope; its kind byte tells which |
| `header` | `blob` holding one commit header |
| `limit` | `u16` from 1 to 512 |

An envelope or header inside a message is verified as history verifies it, and
its digest is SHA-256 of its exact bytes. A `SubmitRequest` carries its commit
header, so its commit ID is computed, never sent. History carries a commit's ID
with the header's other fields; the receiver rebuilds the header once it has
every event and refuses the commit unless its hash is that ID.

| Shape | Fields in order |
| --- | --- |
| `RepositoryIdentity` | `uuid` repo_id, `uuid` generation |
| `Cursor` | `RepositoryIdentity`, `u64` repo_seq, `digest` checkpoint |
| `Capabilities` | `u16` protocol_version (2), `bool` encrypted, `Cursor` latest, `ServerPolicy` |
| `ServerPolicy` | `opt u64` max_events_per_commit, `opt u64` max_object_bytes, `opt u64` quota_bytes |
| `ProposalPage` | `RepositoryIdentity`, `str` operation_id, `u64` offset, `list Record` |
| `Record` | `u8` tag, then: 1 `envelope` (item); 2 `envelope` (attached ref); 3 `uuid` item_id, `seq` head_seq (head guard); 4 `uuid` root_item_id, `bool` require_empty_live_descendants, `list (uuid, seq)` descendants (subtree guard) |
| `PageAck` | `u64` next_offset, `time` expires_at |
| `SubmitRequest` | `RepositoryIdentity`, `str` operation_id, `opt u64` repository_head, `header`, `u64` guard_count, `digest` guards_digest |
| `SubmitResult` | `u8` status, then: 0 `Receipt`; 1 `list (uuid, u64)` conflicts; 2 `u8` limit, `u64` maximum, `u64` actual |
| `Receipt` | `str` operation_id, `digest` request_digest, `digest` commit_id, `time` accepted_at, `opt Cursor` published |
| `HistoryRequest` | `HistoryPosition` after, `limit` |
| `HistoryPosition` | `Cursor` completed, `opt (digest commit_id, u32 next_event_index)` pending |
| `HistoryPage` | `HistoryPosition` after, `list Fragment`, `bool` has_more |
| `Fragment` | `PublishedHeader`, `list Event` |
| `PublishedHeader` | `digest` commit_id, `u64` repo_seq, `time` created_at, `digest` checkpoint, `str` author_id, `str` committer_id, `u8` flags (bit 0 `time` occurred_at, bit 1 `str` message, as in the commit header), `u32` event_count |
| `Event` | `envelope`, `seq` item_seq or ref_seq; a receiver takes parents from replay |
| `StateRequest` | `StatePosition` after, `limit` |
| `StatePosition` | `RepositoryIdentity`, `u64` changed_seq, `opt uuid` item_id |
| `StatePage` | `StatePosition` after, `list StateEntry`, `list (uuid item_id, u64 changed_seq)` departed, `Cursor` through, `bool` has_more |
| `StateEntry` | `uuid` item_id, `str` item_type, `u64` changed_seq, `seq` head_seq, `u64` ref_head_seq, `target` parent_folder_id, `opt str` name, `opt str` record_key, `target` link_folder_id, `u8` lifecycle_state (1 LIVE, 2 TRASH, 3 TOMBSTONED), `opt str` federation_stream_id (present exactly for a FEDERATED item), `content`, `opt digest` metadata_id, `opt digest` name_metadata_id, `time` created_at, `str` created_by, `time` changed_at, `str` changed_by |
| `ItemHistoriesRequest` | `RepositoryIdentity`, `list (uuid item_id, u64 after_seq, u64 after_ref_seq)`, `limit` |
| `ItemHistoriesPage` | `RepositoryIdentity`, `list ItemHistoryPage` |
| `ItemHistoryPage` | `uuid` item_id, `list ItemEvent` events, `list ItemEvent` refs, `list ItemCommit` commits, `bool` has_more |
| `ItemEvent` | `u64` repo_seq, `u32` commit_event_idx, `Event` |
| `ItemCommit` | `u64` repo_seq, `digest` commit_id, `str` author_id, `time` occurred_at (its acceptance time when it has none), `time` created_at |
| `CommitLogRequest` | `RepositoryIdentity`, `opt u64` before_seq, `limit` |
| `CommitLogPage` | `list CommitSummary`, `bool` has_more |
| `CommitSummary` | `u64` repo_seq, `digest` commit_id, `str` author_id, `time` occurred_at, `u32` event_count, `list (uuid item_id, seq item_seq, u32 commit_event_idx, str event_type)` |
| `ObjectRequest` | `list digest`, 1 to 512 object IDs |
| `LeaseRequest` | `list str` paths, `str` owner, `u32` ttl |
| `Lease` | `str` token, `list LeasedPath` |
| `LeasedPath` | `str` path, `str` owner, `i64` expires_at (Unix seconds) |
| `Leases` | `list LeasedPath` |
| `LeaseRenewal` | `str` token, `u32` ttl |
| `LeaseToken` | `str` token |
| `LeasePaths` | `list str` |
| `Released` | `u64` released |

A `u64` sequence or count is at most 2^63 - 1. A `repo_seq` of zero is genesis;
`after_seq`, `after_ref_seq` and a `ref_head_seq` of zero mean no events. A
conflict's head zero means the item is absent. The policy `limit` codes are 1
`events_per_commit`, 2 `object_bytes` and 3 `quota_bytes`.

## Admission policy

`ServerPolicy` fields that are absent are unrestricted. Object and quota byte
counts refer to stored object bytes. The authority checks quota against its
durable usage and new, nonduplicated object bytes transactionally. Policy may
refuse admission of valid content; it never makes accepted history invalid or
untransferable. The reference `synck serve` advertises no limits. Limits are
never format constants.

A policy refusal (`SubmitResult` status 2) is non-retryable without changing
the work or policy. Never split or rewrite a submitted proposal. Clone commit
preflights policy before constructing a commit, telling the agent to commit
selected paths or use `synck import`. Publishing an existing repository first
inventories **all** offending commit IDs, object-size violations and quota
requirements, before any upload. History is not rewritten to fit.

## Transfer bounds

A page holds at most **512 records** and **8 MiB** of encoded message. These are
not commit limits; 8 MiB fits any format-valid event. Producers stop a page
before either bound and bound their source queries, not just the final
encoding. Receivers cap network bytes before decoding.

## Paged proposal upload

A proposal is one ordered record stream across all pages: item envelopes, then
attached-ref envelopes, then head guards, then subtree guards. A page's
`operation_id` and `offset` equal its URL's. Head guards are unique, sorted by
item ID, and assert a pre-commit head. Subtree guards are unique and sorted by
root ID; each binds the exact sorted non-tombstoned descendants and their heads,
or requires that the root has no live descendants, in which case it lists none.
There is no aggregate event, byte or guard-count cap; guards are paged so they
are no hidden limit on the number of affected items.

The client persists the exact envelopes, object bytes, operation ID and commit
header before reporting a saved pending change. It uploads pages at the next
acknowledged offset. The server durably installs a page before replying; a
same-offset retry returns the original acknowledgement only for identical
records. Changed contents or gaps are refused, never overwritten. A dropped
reply does not justify a fresh operation ID. The authority may rebuild
`ProposalVerifier` by replaying durable pages; it keeps only running digests,
counts and the previous guard IDs, not the whole proposal.

`SubmitRequest` ends the upload. Its `header` is the commit header, so the
commit ID is SHA-256 of it, and its `event_count` and `events_digest` must match
the staged envelopes. `guards_digest` is SHA-256 of the concatenated SHA-256
digests of each guard record's encoded bytes, tag included, in stream order;
an empty stream hashes empty bytes. `guard_count` counts both guard kinds.
`request_digest` is SHA-256 of the encoded `SubmitRequest`. Page size and
boundaries change none of these.

`repository_head` fences the complete state used to plan an opaque application
command: the published repository sequence, 0 for genesis. The authority
compares it while holding its exclusive repository write gate, and refuses it
too while an accepted commit is not yet published; checking only the published
maximum would miss such a phantom. Every other append writer shares that gate.
An existing receipt for the operation is returned before this fence, so retries
stay valid after later commits. It does not change the commit ID.

`validate_submit` checks the header and computes identity. The authority then
calls `ProposalVerifier::finish` against the staged pages, proves referenced
objects durable and performs normal Core acceptance, all in one transaction.
Item bases, ref bases, head guards, namespace and subtree constraints remain
mandatory. A proposal requires every content and metadata object it names:
missing ones are `OBJECTS_MISSING`, a content object whose length differs from
its entry's size makes the proposal invalid, and the authority installs uploaded metadata
through Core's metadata API before accepting the commit that points to it. No
event is accepted before the whole commit is.

The authority stores the request digest and receipt atomically with accepted
history. The same operation with the same digest returns the original receipt;
another digest is `OPERATION_REUSED`. A conflict lists item IDs and current
heads. The server never merges or substitutes newer bases. The client keeps
pending bytes and reconciles explicit conflicts.

## Refusals

`SubmitResult` values, policy refusals included, are answers with status 200;
`PUT /objects` and `POST /leases/renew` answer 204. A refused request has another
status and a body of one `u8` code, followed by its details:

| HTTP | Code | Meaning and remedy |
| --- | --- | --- |
| 400 | 1 `INVALID` | Malformed request, page or object, or different records at an installed page offset |
| 401 | 2 `UNAUTHENTICATED` | No valid credential |
| 403 | 3 `FORBIDDEN` | The credential may not do this |
| 404 | 4 `NOT_FOUND` | No such route, repository for this credential, or receipt |
| 404 | 5 `OBJECT_MISSING` | A requested object is not stored |
| 409 | 6 `CHECKPOINT` | The request's cursor is not on this history |
| 409 | 7 `LOCKED` | Another lease holds a path; `str` path, `str` owner follow |
| 409 | 8 `LOCK_LOST` | A presented lease token is expired, released or replaced, or does not cover a path written |
| 409 | 9 `GENERATION` | The repository's generation is not the request's; reconcile first |
| 409 | 10 `OPERATION_REUSED` | The operation ID was submitted with another request digest |
| 409 | 11 `OBJECTS_MISSING` | The proposal names objects that are not stored; `list digest` follows. Upload them and submit again |
| 410 | 12 `UPLOAD_EXPIRED` | No live upload for the operation; upload its pages again from offset 0 |
| 503 | 13 `SERVER_OVERLOADED` | Retry later |

## Expiry, receipts and restart

A staging session has a server-issued `expires_at`. Every page and the final
submission must arrive before it; `ProposalVerifier` refuses expired sessions.
Rejected requests do not extend it. The authority's cleanup deletes expired
**unaccepted** pages and their staging files, excluding in-flight acceptance
under the same transactional fence. Cleanup never deletes history or referenced
objects.

After staging expiry the client can upload the same stream again under the same
operation ID. The server checks for a receipt before treating staging as
expired. Receipts are not upload leftovers: the authority keeps each one, with
its operation ID, at least 90 days after acceptance, the retry-recovery
lifetime. Expiry never changes an accepted result. A generation change requires
reconciliation before pending uploads resume.

A receipt's `published` cursor is absent while the commit awaits its sequence
and checkpoint: durable acceptance can precede publication. Release acceptance
locks before waiting for the sequencer; a failed wait is not failed acceptance.
Validate a recovered receipt against the exact pending request with
`Receipt::validate_for`.

## History continuation and atomic installation

A `HistoryRequest` reads after a position: a completed cursor and, inside a
commit, the commit ID and the next event index. A pending index is positive and
counts item and ref events. The completed cursor never advances inside a commit.

A `HistoryPage` repeats the requested `after`, then ordered fragments and
`has_more`. Each fragment has its commit's published header and its events in
commit order: the first fragment's begin at the pending index, every other one's
at zero. A fragment is empty only for a commit with no events. Continuations
repeat the identical header. The first fragment may continue the pending commit,
followed by any number of complete commits and at most one incomplete final
fragment. A page may also hold only a portion of the pending commit. Never begin
another commit before finishing the current one, or split one commit into
several fragments in one page.

The events of all fragments together are at most the request's limit, a
fragment of no events counting as one, and the page fits 8 MiB. The server packs
commits until a bound is reached; it does not stop because a commit ended. An
empty fragment list is allowed only at a complete cursor with `has_more` false.
An incomplete final fragment requires `has_more` true. Later publication never
changes a published prefix.

`HistoryVerifier` checks exact positions, unchanged continuation headers,
strict envelopes, item and ref sequences, format limits, each checkpoint as its
header arrives, and each commit ID once the commit's last event has. A failed page leaves all verifier state unchanged, even
when an earlier fragment in it was valid. `push` returns the **last completed
cursor in the page**, or none if no commit completed; the pending index stays
private. A commit has no transfer-size limit.

Clients stage incomplete commits privately. After restart they rebuild the
verifier from the last installed complete cursor and replay the staged prefix
before resuming at the next index. Semantic replay, namespace checks,
projections, history and the new complete cursor install atomically. Never
expose a partial commit, promote a pending index to a cursor, or trust an
unverified persisted hash state. Failure leaves accepted state and unsent local
work intact.

History rows name metadata by ID only. A client fetches each metadata object it
lacks through `POST /objects` and installs it through Core's metadata API before
replaying the commit.

## State read

A client that does not keep the full history follows the repository's current
structural state instead of replaying it, as IMAP clients follow a mailbox.
History stays authoritative and complete on the authority; the state read is a
view of it.

`POST /state` is a changes feed over items. Each `StateEntry` is one item's
structural state after its last **published** event, item or ref: the fields of
`vcs_items` except `name_key`, which a client derives from `name`, the role map
of its current content (empty when none, under the format's content rules), the
ID of the metadata object of its latest two-segment item event that has one
(`metadata_id`; a subitem event's metadata is not the item's, format.md,
Metadata payloads), and the ID of the metadata object of the event that last set
its name (`name_metadata_id`, absent for a record or a naming event without
metadata). `changed_seq` is the `repo_seq` of the commit holding that event;
`changed_at` is that commit's `occurred_at`, or its acceptance time when it has
none, and `changed_by` its `author_id`. Accepted but unpublished events are
never served.

Entries are ordered by `(changed_seq, item_id)`. A request returns the entries
strictly after its position: `changed_seq` greater than the position's, or equal
with a larger item ID. Bootstrap starts at `changed_seq` 0 and no item ID. A page
holds at most `limit` entries and 8 MiB. The client resumes from its last entry.
An item that changes while a client pages moves to a later `changed_seq` and is
returned again, so no item is skipped.

`through` is the published cursor the page was read at. When a page has
`has_more` false, the client holds every item's state at `through`: its state
map equals the structural state replaying history to `through` gives. Tombstoned
and trashed items are entries like any other, so deletions arrive in the feed.
An item never leaves the feed.

An authority may limit a credential to subtrees: it then serves only the entries
of items inside them, and in `departed` each item that left them, with the
`changed_seq` of the commit it left in. The item's descendants left with it.
Departures share the entries' order and position: a page holds every entry and
departure in its range, and the client resumes from the later of its last entry
and its last departure. A client drops a departed item and its descendants,
unless a later entry brings the item back, and ignores a departure of an item it
never held. A credential that reads the whole repository receives no
departures.

A client keeps, per item, the highest `head_seq` it has accepted and treats an
entry with a lower one as an integrity failure, not a retryable error. The
authority cannot forge objects: their frames bind repository, item, role and
epoch. It can withhold, as with history. A client that also replays history can
compare the state digest of its state map at `through` with the digest of
replayed history; a mismatch is an integrity failure.

In an encrypted repository `name` is the name's tag; the display name is in the
object `name_metadata_id` names. Every metadata-bearing item event holds the
item's complete metadata (format.md, Metadata payloads), so `metadata_id` alone is
the item's current metadata. A family reads an item's history only for its
subitems or for state at an earlier event, such as the participants a chat
message was sent to; current state never needs it.

`POST /items/histories` reads the published history of 1 to 512 distinct items.
For each item it returns the item events with `item_seq` above `after_seq`,
ascending, and the attached-ref events the item owns with `ref_seq` above
`after_ref_seq`, ascending, each with its commit's `repo_seq`. `commits` lists
the commit of every row in the page once, by ascending `repo_seq`: its ID,
author, `occurred_at` or else acceptance time, and acceptance time. The pages answer a prefix of the requested
items, in order; every page but the last has `has_more` false. The rows of all
pages together are at most `limit`, an item with no rows counting as one, and
the message fits 8 MiB. The client asks again for the items not answered and,
when the last page has `has_more`, for that item after its last rows. An item the
credential cannot read refuses the whole request.

`POST /commits` lists published commits newest first, for a client without the
full copy, such as an Activity view. `before_seq` absent reads from the newest.
A `CommitSummary` has the commit's `repo_seq`, ID, author, `occurred_at` (its
acceptance time when it has none), `event_count` of item and ref events, and its
first 32 item events in commit order. A page holds commits below `before_seq`,
descending, at most `limit` and 8 MiB; `has_more` is true while older commits
remain, and the client resumes before the last `repo_seq`. Envelopes, ref events
and messages are not served. A credential limited to subtrees is refused
(`FORBIDDEN`): the commit log is the whole repository.

A client writes as before, with `base_seq` from its state map; a stale base is a
conflict and the client rereads that item.

## Objects, encryption and bulk import

Objects move in packs. A pack is a sequence of entries, each the object ID's 32
digest bytes, the object's size as a `u64`, then its exact stored bytes; the
body ends after the last entry. `PUT /objects` takes 1 to 512 entries, verifies
each against its ID and, where its use selects a frame, its frame, and installs
them only when all are valid. `POST /objects` answers a pack of the requested
objects in request order, ending after the entry that brings it to **64 MiB**;
the client asks again for the rest. One object that is not stored refuses the
whole request with `OBJECT_MISSING`. Uploads aim for the same 64 MiB; neither
bound limits an object's size. Missing is different from empty.

Thin replicas may install verified envelopes without every content body; new
content-bearing acceptance requires referenced objects durable. Encryption,
keys and signatures belong to the product profile, not a competing sync format.
An encrypted repository's history and objects travel as stored: a client
without keys verifies envelopes, checkpoints, object IDs and encrypted frames,
as it does any history, and can keep a complete copy it cannot read.

Large imports follow the client-side job model: a durable, disk-backed inventory
before upload, folders first, then independently atomic batches sized from
advertised policy. Each batch has stable item IDs and an operation ID.
Independent batches run in parallel after parent prerequisites are durable.
Reconcile exact receipts before advancing progress. Transport paging does not
split an existing commit: the importer constructs new bounded commits from the
inventory, so two million files need neither the inventory in memory nor
altered history.

Store each file's object ID in the import state database. In an unencrypted
repository a file's ID is its plain SHA-256, so `adopt-existing` can recompute
it and needs no upload for bytes the authority already holds. In an encrypted
repository each upload is a new object, and lost state means uploading again. Do
not infer ownership of a same-name remote file or overwrite unrelated work.

## Reservations and clones

Support shared checkouts with cooperative reservations and isolated clones per
agent. Use one CLI family: `synck lock acquire/renew/release/list/break`.
Standalone repositories use SQLite; clones use server-enforced leases. In a
shared clone the lease also guards local writes, pull and materialization, not
just the acquisition request. Direct filesystem writes bypass these locks, so
all agents sharing files must participate. Receipt recovery and preserving
unfinished local work remain necessary after expiry or loss of a server lease.

The lease routes follow the local rules of `docs/local-repository.md`
("Local agents sharing a checkout"): repository paths with `/`, compared by the
VCS's portable name keys; parent and child conflict; all paths of one request
succeed or fail together; `ttl` is 1 to 86400 seconds; an expired lease still
fences until another request covers its whole scope; listing never shows
tokens; `break` removes every lease overlapping its paths. The server cannot see
a client's working files, so the client checks that the paths are clean before
it asks. A reference server keeps leases in the repository's own reservation
table, so they fence the repository's local working trees too, and theirs fence
remote writers.

A submission carries the lease tokens its writer holds as `synck-lock-token`
headers, one per token; they are credentials, not part of the request digest.
The authority refuses a submission that writes a path another lease holds
(`LOCKED`), or, when tokens are presented, one that writes a path they do not
cover or that presents a dead token (`LOCK_LOST`). A proposal writes each item it
touches where the item is and where the proposal puts it. A retry of an accepted
operation returns its receipt without these checks. `synck clone https://…`
uses these routes; see `docs/local-repository.md` ("HTTPS clones").

## Evidence

[The small vector](vectors/sync.json) gives one proposal, its pages, its
`SubmitRequest` bytes, request digest and commit ID. [The 10,001-event
vector](vectors/sync-large.json) contains a deterministic event-generation
recipe and independently computed hashes, a 16 KiB message and a 256-byte
operation ID. [The wire vector](vectors/sync-wire.json) gives the bytes of one
value of each shape and bytes a receiver must refuse. [The packed-history
vectors](vectors/sync-history.json) specify 128 small commits in one page and a
page that continues one commit, completes intervening commits, and starts
another.

`cargo test --locked -p synck-sync --test protocol` checks these vectors, upload
expiry, page ordering, retries, partial-history restart, tampering, exact
receipt binding and policy refusals. These are library checks, not a claim of a
deployed service.
