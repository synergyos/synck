# Central synchronization protocol 1

This transport-neutral contract uses [format 7](format.md), commit format 6 and
its objects. `synck-sync` provides wire values, streaming verification and
admission-policy helpers. Authentication, durable staging, cleanup, transaction
installation and Core acceptance belong to the authority/client adapters.
`portable::ServedRepository` in `synck-local` is the repository side of a
server: acceptance, history pages, objects and leases. HTTPS clone/pull
integration and the resumable import executor remain work in the central-sync
plan. Library tests are not evidence that those executables or a deployed
service exist.

This replaces the initial unshipped protocol-1 draft with pages within commits.
There is no 0.1 fallback or downgrade. Consumers pin the same public revision.

## Identity and complete cursors

`RepositoryIdentity` is `{repo_id, generation}`: both are UUIDv7. Generation
identifies one ordered history. Administrative history replacement changes the
generation; it never silently resets a client's state or erases pending work.

`Cursor` is `{repository, repo_seq, checkpoint}`. Genesis has sequence zero and
the repository's genesis value as its checkpoint. Published positions are positive signed 64-bit
values starting at 1, with no gaps. Every next header must have the previous
sequence plus one; reject missing positions even if the checkpoint hash matches.
Checkpoints use the canonical framing and exact
acceptance timestamp. A cursor authenticates continuity from an already trusted
checkpoint, not the remote party's identity. Validate it against stored history.

## Trust model

The authority orders history and clients trust it for what they cannot check.
A client holding a cursor can prove:

- **No rewrite.** Every later commit extends that cursor's checkpoint. An edited,
  dropped or reordered commit already seen fails continuation with `Checkpoint`.
- **No silent replacement.** Replacing history needs a new generation, which the
  client sees.
- **Exact content.** Each event re-encodes canonically and matches its digest;
  each commit ID matches its envelope.

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
without changing commit identity. Neither is part of protocol 1.

## Discovery, transfer and admission policy

All routes are under `/v1/repositories/{repo_id}/sync`:

| Method and suffix | Request / result |
| --- | --- |
| `GET /capabilities` | `Capabilities` |
| `PUT /uploads/{operation_id}/pages/{offset}` | `ProposalPage` / `PageAck` |
| `POST /proposals` | `SubmitRequest` / `SubmitResult` |
| `GET /receipts/{operation_id}` | exact accepted `Receipt`, or not found |
| `POST /history` | `HistoryRequest` / `HistoryPage` |
| `POST /state` | `StateRequest` / `StatePage` (see State read) |
| `POST /items/history` | `ItemHistoryRequest` / `ItemHistoryPage` (see State read) |
| `PUT /objects/{object_id}` | exact stored bytes, immutable installation |
| `GET /objects/{object_id}` | exact stored bytes, or explicit missing |
| `PUT /objects` | an object pack, immutable installation of each object |
| `POST /objects` | `ObjectRequest` / an object pack, or explicit missing |
| `GET /leases` | `Leases` |
| `POST /leases` | `LeaseRequest` / `Lease` |
| `POST /leases/renew` | `LeaseRenewal` / 204 |
| `POST /leases/release` | `LeaseToken` / `Released` |
| `POST /leases/break` | `LeasePaths` / `Released` |

HTTP deployments require authenticated TLS and authorization on every route,
including receipts and immutable object downloads. IDs are not access tokens.
Operations/staging are scoped to repository, generation and authenticated owner.
Operation IDs use the format's 256-byte nonblank UTF-8 rule, not an ASCII-only
subset. Encode an operation ID as one URL path segment; it is never a filesystem
path. Do not log content.

An object ID names content or a metadata object, and both travel through the
object routes. History rows carry metadata by ID only, so a client fetches each
metadata object it lacks from `GET /objects` and installs it through Core's
metadata API before replaying the commit. A proposal requires every content and
metadata object it names: missing ones are `OBJECTS_MISSING`, and the authority
installs uploaded metadata through Core's metadata API before accepting the
commit that points to it.

A server whose `Capabilities` has `object_packs: true` also moves objects in
packs, many per request. A pack is a sequence of entries, each the object ID's
71 ASCII bytes, the object's size as 8 big-endian bytes, then its exact stored
bytes; the body ends after the last entry. `PUT /objects` takes at most **512
entries**, checks each against its ID, and stores them only when all are valid.
`POST /objects` takes an `ObjectRequest` of 1 to 512 IDs and answers a pack of
those objects in request order, ending after the entry that brings it to **64
MiB**; the client asks again for the rest. One object that is not stored refuses
the whole request with `OBJECT_MISSING`. Uploads aim for the same 64 MiB; neither
bound limits an object's size. A client of a server without `object_packs` uses
the per-object routes.

`Capabilities` contains `protocol_version: 1`, `repository`, `encrypted` (a client
needs it to compute the genesis value), `latest` complete cursor, `policy`,
`object_packs` and `state_read`. A server without `state_read: true` has neither
`/state` nor `/items/history`. Fixed transfer bounds are **512 records per page** and
**8 MiB of serialized JSON per page**. These are not commit limits. The byte
bound fits any format-valid individual event, with `payload_json` escaped.
Producers stop a page before either bound, and paginate again. They
bound the source query and construction, not just the final serialization.
Receivers cap network bytes before parsing; in-memory validation alone cannot
protect an unbounded parser. Headers are small because format limits are fixed.

`policy` has nullable `max_events_per_commit`, `max_object_bytes` and `quota_bytes`.
Null means unrestricted. Object/quota byte counts refer to stored object bytes. The authority checks quota against its durable usage and new,
nonduplicated object bytes transactionally. Policy may refuse admission of valid
content; it never makes existing accepted history invalid or untransferable.
The reference `synck serve` advertises all three values as null and can serve any
valid repository. Limits must not be smuggled into format constants.

A refusal is non-retryable without changing the work or policy:

```json
{"status":"POLICY_LIMIT","limit":"events_per_commit","maximum":500,"actual":1200}
```

The other limit names are `object_bytes` and `quota_bytes`. Never split or rewrite
a submitted proposal. Clone commit preflights policy before constructing a
commit, telling the agent to commit selected paths or use `synck import`.
Publishing an existing repository first inventories **all** offending commit
IDs, object-size violations and quota requirements, before any event/object
upload. History is not rewritten to fit. `ServerPolicy` supplies these checks;
the publication caller must finish the complete inventory before network writes.

## Paged proposal upload

`ProposalPage` contains `repository`, `operation_id`, zero-based record `offset`
and `records`. Records are tagged `{kind, value}`: `item` (`EventProposal`), then
`ref` (`RefEventProposal`), then `guard` (`ItemHead`), then optional
`subtree_guard` (`SubtreeClosureGuard`). An upload has one ordered
stream across all pages. Every event supplies a stable item ID. Head guards are
unique, sorted by item ID, and assert a positive pre-commit head. There is no
aggregate event, byte or guard-count cap. Guards are paged so they cannot become
a hidden limit on the number of affected items.

The client persists exact events, object bytes, object identity, operation
ID and final metadata before reporting a saved pending change. Upload pages at
the next acknowledged offset. The server durably installs a page before replying;
a same-offset retry returns the original acknowledgement only for identical
canonical records. Changed contents or gaps are rejected, never overwritten.
A dropped reply does not justify a fresh operation ID. The authority may rebuild
`ProposalVerifier` by replaying durable pages; it keeps only streaming digests,
counts and the previous item/subtree guard IDs, not the whole proposal in memory.

The final `SubmitRequest` contains:

```text
repository, operation_id
repository_head?
metadata: {author_id, committer_id, message, occurred_at}
event_count, events_digest
guard_count, guards_digest
```

`events_digest` is the existing format digest over each event's family byte and
SHA-256 of its canonical payload, with all ordinary events before refs.
`guards_digest` is SHA-256 of the concatenation of SHA-256 digests of each guard's
canonical RFC 8785 JSON, in sorted order. Subtree guards follow head guards,
are unique and sorted by root ID, and hash their complete tagged record rather
than the untagged value. They bind the exact sorted non-tombstoned descendant
IDs and positive heads, or an explicit empty-live requirement. Both guard kinds
count toward `guard_count`. An empty guard stream hashes empty bytes. Legacy
head-only requests and commit identities remain unchanged. Older readers reject
the new record; producers require an authority supporting subtree guards. `request_digest` hashes canonical RFC 8785 JSON of the final typed request,
with `occurred_at` in the format's one spelling.
It binds generation, operation, metadata and both stream summaries. Page size or
boundaries do not affect these identities.

An optional `repository_head` fences the complete state used to plan an opaque
application command. It is the non-negative published repository sequence
(`0` for genesis), bounded by `i64::MAX`. The authority compares it while holding
its exclusive repository write gate and also rejects any sealed accepted commit
that has not yet been published. Checking only the published maximum would miss
such a phantom. Every other append writer must share that gate. The check and
append are one protected operation. An existing accepted operation receipt is
returned before this fence, so retries remain valid after later commits. The
field participates in `request_digest`, including device signatures over it;
it does not change the canonical commit ID. Omission preserves old request
bytes. Authorities may require the fence for application profiles whose opaque
relations cannot be checked by the authority; generic public local acceptance
keeps it optional. Older authorities reject the new field and must be upgraded
before producers use it.

`validate_submit` checks only the header and computes identity. The authority
must call `ProposalVerifier::finish` against the actual staged pages, then prove
referenced objects durable and perform normal Core acceptance in one transaction.
All item bases, ref bases, head guards, namespace and subtree constraints remain
mandatory. Use checked FILE/FOLDER append paths. A digest is not a substitute for
these checks.
No event becomes accepted before the entire commit is accepted atomically.

The authority stores request digest and receipt atomically with accepted history.
Same operation plus same digest returns the original receipt; another digest is
operation reuse. Core uniqueness alone does not establish an identical retry.
`SubmitResult` is `accepted`, `conflict` (item IDs/current heads, zero for absence),
or `POLICY_LIMIT`. The server never merges or substitutes newer bases silently.
The client preserves pending bytes and reconciles explicit conflicts.

## JSON shapes

Every value is one JSON object with exactly the fields below, in any order; a
receiver refuses an unknown field. A `?` marks a field whose value may be null, and
the field is still present, except `repo_seq` in commit and event rows, which is
omitted on an unpublished row. UUIDs are canonical lowercase strings. Digests and
object IDs are `sha256:` plus 64 lowercase hex digits. Timestamps are RFC 3339 UTC
with up to nine fractional digits, and a receiver compares instants. Integers are
JSON integers.

| Shape | Fields |
| --- | --- |
| `RepositoryIdentity` | `repo_id`, `generation` |
| `Cursor` | `repository`, `repo_seq`, `checkpoint` |
| `Capabilities` | `protocol_version`, `repository`, `encrypted`, `latest` (`Cursor`), `policy`, `object_packs` (absent means false), `state_read` (absent means false) |
| `StateRequest` | `after` (`StatePosition`), `limit` |
| `StatePosition` | `repository`, `changed_seq`, `item_id?` |
| `StatePage` | `after`, `entries` (list of `StateEntry`), `through` (`Cursor`), `has_more` |
| `StateEntry` | `item_id`, `item_type`, `changed_seq`, `head_seq`, `ref_head_seq`, `parent_folder_id?`, `name?`, `record_key?`, `link_folder_id?`, `lifecycle_state`, `sequencing_mode`, `federation_stream_id?`, `content` (role map of object IDs), `metadata_id?`, `name_metadata_id?`, `created_at`, `created_by` |
| `ItemHistoryRequest` | `repository`, `item_id`, `after_seq`, `limit` |
| `ItemHistoryPage` | `item_id`, `events` (item events, tagged as in fragments), `has_more` |
| `ObjectRequest` | `objects` (object IDs) |
| `ServerPolicy` | `max_events_per_commit?`, `max_object_bytes?`, `quota_bytes?` |
| `ItemHead` | `item_id`, `head_seq` |
| `ProposalPage` | `repository`, `operation_id`, `offset`, `records` |
| `PageAck` | `next_offset`, `expires_at` |
| `CommitMetadata` | `author_id`, `committer_id`, `message?`, `occurred_at?` |
| `SubmitRequest` | `repository`, `operation_id`, `metadata`, `event_count`, `events_digest`, `guard_count`, `guards_digest`, optional `repository_head` |
| `Receipt` | `operation_id`, `request_digest`, `commit_id`, `accepted_at`, `published?` (`Cursor`) |
| `HistoryRequest` | `after` (`HistoryPosition`), `limit` |
| `HistoryPosition` | `completed` (`Cursor`), `pending?` |
| `PendingCommit` | `commit_id`, `next_event_index` |
| `HistoryPage` | `after`, `fragments`, `has_more` |
| `HistoryFragment` | `header`, `events` |
| `PublishedHeader` | `commit` (`CommitRow`), `checkpoint` |
| `CommitRow` | `repo_id`, `commit_id`, `repo_seq`, `format_version`, `author_id`, `committer_id`, `message?`, `occurred_at?`, `created_at`, `event_count`, `events_digest` |
| `LeaseRequest` | `paths`, `owner`, `ttl` |
| `Lease` | `token`, `locks` (list of `LeasedPath`) |
| `LeasedPath` | `path`, `owner`, `expires_at` (UTC Unix seconds) |
| `Leases` | `locks` |
| `LeaseRenewal` | `token`, `ttl` |
| `LeaseToken` | `token` |
| `LeasePaths` | `paths` |
| `Released` | `released` |

`SubmitResult` is tagged by `status`. `accepted` has `receipt`. `conflict` has `items`,
a list of `ItemHead`, where `head_seq` zero means the item is absent. `POLICY_LIMIT`
has `limit`, `maximum` and `actual`. `limit` is `events_per_commit`, `object_bytes` or
`quota_bytes`.

An event in a fragment is tagged by `event_family`. `item` has `repo_id`, `item_id`,
`item_seq`, `event_type`, `commit_id`, `commit_event_idx`, `repo_seq`, `payload_json`,
`event_payload_digest`, `created_at`, `parent_folder_id?` and
`parent_folder_id_before?`. `ref` has `repo_id`, `owner_item_id`, `ref_seq`,
`event_type`, `commit_id`, `commit_event_idx`, `repo_seq`, `payload_json`,
`event_payload_digest` and `created_at`. A `payload_json` is one JSON string whose
UTF-8 encoding is the event's exact canonical bytes, as in a history bundle. It is a
string and not a nested object so that a JSON parser cannot change those bytes.

A proposal record is `{"kind": "item" | "ref" | "guard" | "subtree_guard", "value": ...}`. An `item`
value is an event envelope with its `item_id`, a `ref` value is a ref envelope, and a
`guard` value is an `ItemHead`. A `subtree_guard` value has `root_item_id`,
`expected_non_tombstoned_descendants` (sorted `[item_id, head_seq]` pairs),
and `require_empty_live_descendants`. The empty-live mode cannot list descendants.
Authorities pass these exact authenticated guards to checked Core acceptance;
missing, moved-in, moved-out, or changed descendants reject the whole proposal.

## HTTP refusals

`SubmitResult` values, `POLICY_LIMIT` included, are answers with status 200. Both
`PUT /objects` routes answer 204. A refused request has another status and the body
`{"status": CODE}`; a client acts on the code:

| HTTP | `status` | Meaning and remedy |
| --- | --- | --- |
| 400 | `INVALID` | Malformed request, page or object, or different contents at an installed page offset |
| 401 | `UNAUTHENTICATED` | No valid credential |
| 403 | `FORBIDDEN` | The credential may only read |
| 404 | `NOT_FOUND` | No such route, no such repository for this credential, or no receipt |
| 404 | `OBJECT_MISSING` | The object is not stored |
| 409 | `CHECKPOINT` | The history request's cursor is not on this history |
| 409 | `LOCKED` | Another lease holds a path; the body's `path` and `owner` name it |
| 409 | `LOCK_LOST` | A presented lease token is expired, released or replaced, or does not cover a path written |
| 409 | `GENERATION` | The repository's generation is not the request's; reconcile first |
| 409 | `OPERATION_REUSED` | The operation ID was submitted with another request digest |
| 409 | `OBJECTS_MISSING` | The submission names content that is not stored; the body's `objects` lists it. Upload it and submit again |
| 410 | `UPLOAD_EXPIRED` | No live upload for the operation; upload its pages again from offset 0 |
| 503 | `SERVER_OVERLOADED` | Retry later |

## Expiry, receipts and restart

A staging session has an explicit server-issued `expires_at`. Every page and
final submission must arrive before that deadline; `ProposalVerifier` refuses
expired sessions. Do not silently extend expiry on rejected requests. Expired
**unaccepted** page rows and their staging files are deleted by the authority's
cleanup job, excluding in-flight acceptance under the same transactional fence.
Page deletion must not delete canonical history or referenced objects.

After staging expiry the client can reupload the same pending stream and
operation ID. The server checks an accepted receipt before treating staging as
missing/expired. Accepted receipts are not upload leftovers and remain durable
for the promised retry-recovery lifetime. Expiry never changes an accepted result.
Generation changes require reconciliation before pending uploads resume.

`Receipt` contains `operation_id`, `request_digest`, `commit_id`, `accepted_at`
and nullable `published` cursor. Durable acceptance can precede asynchronous
sequence/checkpoint publication. Release acceptance locks before waiting for the
sequencer; a failed wait is not failed acceptance. Validate recovered receipts
against the exact pending request using `Receipt::validate_for`.

## History continuation and atomic installation

`HistoryRequest` contains `after: HistoryPosition` and an event `limit` from 1 to
512. `HistoryPosition` contains `completed: Cursor` and optional
`pending: {commit_id, next_event_index}`. A pending offset is positive and counts
both item and ref events. The completed cursor never advances inside a commit.

A `HistoryPage` contains the exact requested `after`, ordered `fragments` and
`has_more`. Each `HistoryFragment` has its own `header` (`CommitRow` and resulting
checkpoint) and ordered `events`, empty only for a commit with no events.
Continuations repeat the identical
header. The first fragment may continue the pending commit, followed by any
number of complete commits and at most one incomplete final fragment. A page
may also contain only a portion of the pending commit. Never begin another
commit before finishing the current one, or split one commit into multiple
fragments within the same page.

The **sum** of events across fragments must not exceed 512, with a fragment of no
events counting as one, and the entire
serialized page, including every header, must fit 8 MiB. The server packs commits
until the requested event limit or byte bound is reached; it does not stop just
because a commit ended. An empty fragment list is allowed only at a complete
cursor with `has_more=false`. An incomplete final fragment requires
`has_more=true`. Concurrent later publication never changes an already published
prefix.

`HistoryVerifier` checks exact positions, unchanged continuation headers,
canonical payloads and format limits, event indexes/families/digests, and each
commit ID and checkpoint in order. A failed page leaves all verifier state
unchanged, including when an earlier fragment in that page was valid. `push`
returns the **last completed cursor in the page**, or `None` if no commit
completed. That cursor covers all completed commits even when a trailing
fragment is incomplete; the pending offset remains private. A full commit has
no transfer-size limit; `verify_published_commit` also accepts any format-valid
event count.

Clients durably stage incomplete commits privately. After restart rebuild the
verifier from the last installed complete cursor and replay the staged prefix
before resuming the next event offset. Semantic Core replay, namespace checks,
projections, accepted history and the new complete cursor install atomically.
Never expose a partial commit, promote a pending offset to an accepted cursor,
or trust an unverified persisted hash accumulator. Failure leaves previously
accepted state and unsent local work intact.

## State read

A client that does not keep the full history follows the repository's current
structural state instead of replaying it, as IMAP clients follow a mailbox.
History stays authoritative and complete on the authority; the state read is a
view of it. The reference `synck serve` does not offer it; the SynckHub Personal
service does.

`POST /state` is a changes feed over items. Each `StateEntry` is one item's
structural state after its last **published** event, item or ref: the fields of
`vcs_items` except `name_key`, which a client derives from `name`, the role map
of its current content (`{}` when none, under the format's content rules), the
ID of the metadata object of its latest item event that has one
(`metadata_id`), and the ID of the metadata object of the event that last set
its name (`name_metadata_id`, null for a record or a naming event without
metadata). `changed_seq` is the `repo_seq` of the commit holding that event.
Accepted but unpublished events are never served; the entry still shows the
published state.

Entries are ordered by `(changed_seq, item_id)`. A request returns the entries
strictly after its position: `changed_seq` greater than the position's, or equal
with a larger `item_id`. Bootstrap starts at `changed_seq` 0 and a null
`item_id`. A page holds at most 512 entries and 8 MiB of serialized JSON. The
client resumes from its last entry. An item that changes while a client pages
moves to a later `changed_seq` and is returned again, so no item is skipped.

`through` is the published cursor the page was read at. When a page has
`has_more=false`, the client holds every item's state at `through`: its state map
equals the structural state that replaying history to `through` would give.
Tombstoned and trashed items are entries like any other, so deletions arrive in
the feed. An item never leaves the feed. This version serves credentials that
read the whole repository or, as below, limited subtrees.

An authority may limit a credential to subtrees: it then serves only the
entries of items inside them, and a `departed` list of `StateDeparture
{item_id, changed_seq}`, each an item that left them in the commit at
`changed_seq`. The item's descendants left with it. Departures share the
entries' order and position: a page holds every entry and departure in its
range, and the client resumes from the later of its last entry and its last
departure. A client drops a departed item and its descendants, unless a later
entry brings the item back. A departure of an item the client never held is
ignored. `departed` is absent when empty, and a credential that reads the whole
repository never receives it.

A client keeps, per item, the highest `head_seq` it has accepted and treats an
entry with a lower one as an integrity failure, not a retryable error.
The authority cannot forge objects: their frames bind repository, item, role and
epoch. It can withhold, as with history (see Trust model). A client that also
replays history can compare the state digest of its state map at `through` with
the digest of replayed history; a mismatch is an integrity failure.

In an encrypted repository `name` is the name's tag; the display name is in the
object `name_metadata_id` names. A client that needs every name offline fetches
those metadata objects through the object routes as entries arrive. A family
whose presentation needs older metadata reads the item's history.

`POST /items/history` returns one item's published events with `item_seq`
greater than `after_seq`, ascending, in the same event shape as history
fragments. `refs` holds the published ref events the item owns with `ref_seq`
greater than `after_ref_seq` (default 0), ascending, tagged `ref`. `authors`
maps the commit of every row in the page to its `author_id`. A page holds at
most `limit` (1 to 512) rows of both lists together and 8 MiB; `has_more` is
true while either list has more, and the client resumes from the last
`item_seq` and `ref_seq` it received. It serves version views and families that
walk back metadata, without the commit log. It does not replace `/history` for
a client that keeps the full copy.

A client writes as before, with `base_seq` from its state map; a stale base is
`conflict` and the client rereads that item.

## Objects, encryption and bulk import

Objects transfer as `application/octet-stream` with their exact stored bytes.
Validate the digest and, where the role selects a frame, the frame, before
immutable installation. Missing is different from
empty. Thin replicas may install verified envelopes without every content body;
new content-bearing acceptance requires referenced objects durable. Encryption,
keys and signatures belong to the product profile, not a competing sync format.

Large imports follow the existing SynckHub client-side job model: a durable,
disk-backed inventory before upload, folders first, then independently atomic
batches sized from advertised policy. Each batch has stable item IDs and an
operation ID. Independent batches run in parallel after parent
prerequisites are durable. Reconcile exact receipts before advancing progress.
Transport paging does not split a pre-existing commit: the importer deliberately
constructs new bounded commits from the inventory. This preserves the model for
two million files without loading the inventory into memory or altering history.

Store each file's object ID in the import state database. In an unencrypted
repository a file's ID is its plain SHA-256, so `adopt-existing` can recompute it
and needs no upload for bytes the authority already holds. In an encrypted
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

[The small vector](vectors/sync.json) preserves the initial commit ID while
specifying the paged final request. [The 10,001-event vector](vectors/sync-large.json)
contains an explicit deterministic ASCII event-generation recipe and independently
computed hashes, a 16 KiB message and a 256-byte operation ID.
`cargo test --locked -p synck-sync --test protocol` checks these vectors, upload
expiry, page ordering, retries, partial-history restart, tampering, exact receipt
binding and separate policy refusals. These are shared-library checks, not a
claim of a deployed service or completed clone/import executor.

[The wire vector](vectors/sync-wire.json) gives one value of each shape above,
values a receiver must refuse, and one proposal with two head guards.

[The packed-history vectors](vectors/sync-history.json) independently specify
128 small commits transferred in one page and a page that continues one commit,
completes intervening commits, and starts another. Protocol tests verify their
checkpoint chains, whole-page limits, rollback on late corruption, and restart
from the last completed cursor with only the trailing prefix staged.
