# Central synchronization protocol 1

This transport-neutral contract uses [format 7](format.md), commit format 6 and
salted objects. `synck-sync` provides wire values, streaming verification and
admission-policy helpers. Authentication, durable staging, cleanup, transaction
installation and Core acceptance belong to the authority/client adapters.
The generic `synck serve`, clone/pull integration, remote lease commands and
resumable import executor remain work in the central-sync plan. Library tests
are not evidence that those executables or a deployed service exist.

This replaces the initial unshipped protocol-1 draft with pages within commits.
There is no 0.1 fallback or downgrade. Consumers pin the same public revision.

## Identity and complete cursors

`RepositoryIdentity` is `{repo_id, generation}`: both are UUIDv7. Generation
identifies one ordered history. Administrative history replacement changes the
generation; it never silently resets a client's state or erases pending work.

`Cursor` is `{repository, repo_seq, checkpoint}`. Genesis has sequence zero and
a `sha256:` digest of 64 zeroes. Published positions are positive signed 64-bit
values starting at 1, with no gaps. Every next header must have the previous
sequence plus one; reject missing positions even if the checkpoint hash matches.
Checkpoints use the canonical framing and exact
acceptance timestamp. A cursor authenticates continuity from an already trusted
checkpoint, not the remote party's identity. Validate it against stored history.

## Discovery, transfer and admission policy

All routes are under `/v1/repositories/{repo_id}/sync`:

| Method and suffix | Request / result |
| --- | --- |
| `GET /capabilities` | `Capabilities` |
| `PUT /uploads/{operation_id}/pages/{offset}` | `ProposalPage` / durable next offset and expiry |
| `POST /proposals` | `SubmitRequest` / `SubmitResult` |
| `GET /receipts/{operation_id}` | exact accepted `Receipt`, or not found |
| `POST /history` | `HistoryRequest` / `HistoryPage` |
| `PUT /objects/{object_id}` | exact canonical salted bytes, immutable installation |
| `GET /objects/{object_id}` | exact framed bytes, or explicit missing |

HTTP deployments require authenticated TLS and authorization on every route,
including receipts and immutable object downloads. IDs are not access tokens.
Operations/staging are scoped to repository, generation and authenticated owner.
Operation IDs use the format's 256-byte nonblank UTF-8 rule, not an ASCII-only
subset. Encode an operation ID as one URL path segment; it is never a filesystem
path. Install object metadata through Core's metadata API. Do not log content.

`Capabilities` contains `protocol_version: 1`, `repository`, `latest` complete
cursor and `policy`. Fixed transfer bounds are **512 records per page** and
**8 MiB of serialized JSON per page**. These are not commit limits. The byte
bound fits any format-valid individual event even with JSON numeric-byte history
encoding. Producers stop a page before either bound, and paginate again. They
bound the source query and construction, not just the final serialization.
Receivers cap network bytes before parsing; in-memory validation alone cannot
protect an unbounded parser. Headers are small because format limits are fixed.

`policy` has nullable `max_events_per_commit`, `max_object_bytes` and `quota_bytes`.
Null means unrestricted. Object/quota byte counts refer to canonical framed
object bytes. The authority checks quota against its durable usage and new,
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
`ref` (`RefEventProposal`), then `guard` (`ItemHead`). An upload has one ordered
stream across all pages. Every event supplies a stable item ID. Head guards are
unique, sorted by item ID, and assert a positive pre-commit head. There is no
aggregate event, byte or guard-count cap. Guards are paged so they cannot become
a hidden limit on the number of affected items.

The client persists exact events, object bytes, salt/object identity, operation
ID and final metadata before reporting a saved pending change. Upload pages at
the next acknowledged offset. The server durably installs a page before replying;
a same-offset retry returns the original acknowledgement only for identical
canonical records. Changed contents or gaps are rejected, never overwritten.
A dropped reply does not justify a fresh operation ID. The authority may rebuild
`ProposalVerifier` by replaying durable pages; it keeps only streaming digests,
counts and one previous guard ID, not the whole proposal in memory.

The final `SubmitRequest` contains:

```text
repository, operation_id
metadata: {author_id, committer_id, message, occurred_at, import_job_id}
event_count, events_digest
guard_count, guards_digest
```

`events_digest` is the existing format digest over each event's family byte and
SHA-256 of its canonical payload, with all ordinary events before refs.
`guards_digest` is SHA-256 of the concatenation of SHA-256 digests of each guard's
canonical RFC 8785 JSON, in sorted order; an empty guard stream hashes empty
bytes. `request_digest` hashes canonical RFC 8785 JSON of the final typed request.
It binds generation, operation, metadata and both stream summaries. Page size or
boundaries do not affect these identities. `import_job_id` is null or the format's
canonical UUIDv4 and participates in commit identity, exactly as local Core does.

`validate_submit` checks only the header and computes identity. The authority
must call `ProposalVerifier::finish` against the actual staged pages, then prove
referenced objects durable and perform normal Core acceptance in one transaction.
All item bases, ref bases, head guards, namespace and subtree constraints remain
mandatory. Use checked FILE/FOLDER append paths and the existing import-job
append API when applicable. A digest is not a substitute for these checks.
No event becomes accepted before the entire commit is accepted atomically.

The authority stores request digest and receipt atomically with accepted history.
Same operation plus same digest returns the original receipt; another digest is
operation reuse. Core uniqueness alone does not establish an identical retry.
`SubmitResult` is `accepted`, `conflict` (item IDs/current heads, zero for absence),
or `POLICY_LIMIT`. The server never merges or substitutes newer bases silently.
The client preserves pending bytes and reconciles explicit conflicts.

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
checkpoint) and nonempty ordered `events`. Continuations repeat the identical
header. The first fragment may continue the pending commit, followed by any
number of complete commits and at most one incomplete final fragment. A page
may also contain only a portion of the pending commit. Never begin another
commit before finishing the current one, or split one commit into multiple
fragments within the same page.

The **sum** of events across fragments must not exceed 512, and the entire
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

## Objects, encryption and bulk import

Objects transfer as `application/octet-stream` with exact salted framing. Validate
framing, codec and digest before immutable installation. Missing is different from
empty. Thin replicas may install verified envelopes without every content body;
new content-bearing acceptance requires referenced objects durable. Encryption,
keys and signatures belong to the product profile, not a competing sync format.

Large imports follow the existing SynckHub client-side job model: a durable,
disk-backed inventory before upload, folders first, then independently atomic
batches sized from advertised policy. Each batch has stable item IDs, operation
ID and `import_job_id`. Independent batches run in parallel after parent
prerequisites are durable. Reconcile exact receipts before advancing progress.
Transport paging does not split a pre-existing commit: the importer deliberately
constructs new bounded commits from the inventory. This preserves the model for
two million files without loading the inventory into memory or altering history.

Store each file's object ID and salt in the import state database. Plaintext
SHA-256 cannot reconstruct a salted object ID for `adopt-existing`. Surviving
state permits exact reuse; lost state means uploading bytes again. Do not infer
ownership of a same-name remote file or overwrite unrelated work.

## Reservations and clones

Support shared checkouts with cooperative reservations and isolated clones per
agent. Use one CLI family: `synck lock acquire/renew/release/list/break`.
Standalone repositories use SQLite; clones use server-enforced leases. In a
shared clone the lease also guards local writes, pull and materialization, not
just the acquisition request. Direct filesystem writes bypass these locks, so
all agents sharing files must participate. Receipt recovery and preserving
unfinished local work remain necessary after expiry or loss of a server lease.
Remote lease adapters and clone command integration are pending plan steps.

## Evidence

[The small vector](vectors/sync.json) preserves the initial commit ID while
specifying the paged final request. [The 10,001-event vector](vectors/sync-large.json)
contains an explicit deterministic ASCII event-generation recipe and independently
computed hashes, a 16 KiB message, a 256-byte operation ID and an import-job ID.
`cargo test --locked -p synck-sync --test protocol` checks these vectors, upload
expiry, page ordering, retries, partial-history restart, tampering, exact receipt
binding and separate policy refusals. These are shared-library checks, not a
claim of a deployed service or completed clone/import executor.

[The packed-history vectors](vectors/sync-history.json) independently specify
128 small commits transferred in one page and a page that continues one commit,
completes intervening commits, and starts another. Protocol tests verify their
checkpoint chains, whole-page limits, rollback on late corruption, and restart
from the last completed cursor with only the trailing prefix staged.
