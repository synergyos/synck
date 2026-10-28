# Central synchronization protocol 1

This is the shared, transport-neutral contract for new Synck 0.2 authorities and
clients. It uses [repository format 7, commit format 6 and salted objects](format.md).
It is incompatible with the removed 0.1 remote protocol. `synck-sync` owns its
Rust wire values, limits and verification; product authentication, encryption,
tenancy, quotas and storage adapters remain with their owning services.

The protocol library is implemented. A generic `synck serve` executable and CLI
clone/pull/push integration are separate work; this document does not claim they
exist. No product may replace transactional Core invariants with wire validation.

## Repository identity and cursors

`RepositoryIdentity` is `{repo_id, generation}`. Both are UUIDv7. The generation
identifies one ordered history; copying a repository preserves it. A replacement
history has a new generation even if its repository ID is unchanged.

`Cursor` is `{repository, repo_seq, checkpoint}`. Positions are nonnegative signed
64-bit values on storage, represented as JSON integers. The genesis cursor has
position zero and a checkpoint of `sha256:` followed by 64 zeroes. Published
positions are positive, strictly increasing, and may have gaps. Checkpoint digest
bytes use the canonical format's framing and the authority's exact acceptance
timestamp, never a rounded timestamp or a proposal's author date.

The authority verifies the supplied cursor against stored history. Generation
mismatch requires reconciliation, including revalidation of pending proposals;
it must never cause a silent cursor reset or data deletion. A client verifies
every next page from its exact last durable cursor. A checkpoint authenticates
continuity from a trusted cursor, not an unknown server or user identity.

## Discovery and HTTP binding

The canonical route prefix is `/v1/repositories/{repo_id}/sync`:

| Method and suffix | Request / result |
| --- | --- |
| `GET /capabilities` | `Capabilities` |
| `POST /history` | `HistoryRequest` / `HistoryPage` |
| `POST /proposals` | `SubmitRequest` / `SubmitResult` |
| `GET /receipts/{operation_id}` | `Receipt`, or not found |
| `PUT /objects/{object_id}` | canonical framed bytes, immutable installation |
| `GET /objects/{object_id}` | canonical framed bytes, or explicit missing |

An HTTP deployment requires authenticated TLS. Authorization applies to every
route, including immutable object downloads and receipt lookups. A digest is not
an access capability. Object keys and receipt identities are repository scoped.
Error responses and account admission are service-owned; they must distinguish
invalid input, authorization, generation mismatch, concurrency conflict, operation
reuse, unavailable/missing objects, limits and retryable service failure. Do not
put private application plaintext into error strings or request logs.

`Capabilities` contains `protocol_version` (1), `repository`, `latest` cursor,
`max_events`, `max_proposal_bytes`, `max_history_commits`, `max_history_bytes`
and `max_object_bytes`. Services may choose lower limits than the library bounds.
Clients validate version, identity and limits before use. Object-size policy is
service owned and must be enforced while streaming before committing an upload.

## Proposals, acceptance and receipts

`SubmitRequest` contains:

```text
repository: RepositoryIdentity
proposal: CommitProposal
ref_events: [RefEventProposal]
head_guards: [{item_id, head_seq}]
```

Every item event has an explicit stable `item_id`, including creates. A create
has no `base_seq`; all events for an existing item carry its exact pre-commit
base. Multiple events for a newly created item use no base. Ref events carry the
exact pre-commit `base_ref_seq`, independent of the ordinary item head. Ordinary
events precede ref events in commit identity. Metadata and content objects are
references only; missing plaintext never blocks structural interpretation.

Head guards are unique and sorted by item ID. A submitted guard has a positive
head and asserts an existing item's exact head under the same transaction and
lock set as acceptance. Guards are preconditions, not commit events. Core owns
namespace and structural guards for affected moves and renames; applications
must select its checked FILE/FOLDER acceptance path where subtree closure is
required. Protocol-level head guards do not replace those checks.

`proposal.operation_id` is mandatory: 1–128 ASCII letters, digits, `-`, `_`, `.`
or `:`. Persist the exact request and object bytes before acknowledging a local
save. An offline proposal is pending, not a local accepted commit or a remote
receipt. A new retry ID must not be generated after a dropped reply.

`validate_submit` computes:

- `commit_id`: the existing Core hash of canonical content, independent of
  repository placement, acceptance time, operation ID or guards;
- `request_digest`: SHA-256 of the canonical RFC 8785 JSON of the entire typed
  request, including generation, operation ID and guards;
- `events_digest` and the complete set of referenced metadata/content objects.

The authority checks authenticated identities, scope, object durability and
persisted bases/structure in the same acceptance transaction. Install referenced
metadata objects through Core's metadata API. Missing content objects cause an
explicit refusal, not an empty-content substitution. Never mix VCS types from
different public revisions inside one authority graph.

Store the request digest and receipt atomically with accepted history. If an
operation ID already exists with that digest, return the original receipt; with
another digest, reject operation reuse. Core's operation-ID uniqueness alone
does not establish that a different request is an identical retry.

`SubmitResult` is tagged by `status`:

```text
{"status":"accepted", "receipt": Receipt}
{"status":"conflict", "items":[{"item_id":..., "head_seq":...}]}
```

Conflict heads are positive for existing items and zero for absence. A conflict
is not acceptance. Reconcile against verified history and preserve unsent bytes;
the authority does not merge application content. Clients must never silently
overwrite another device's changed item after substituting a newer base.

`Receipt` contains `operation_id`, `request_digest`, `commit_id`, `accepted_at`
and nullable `published` cursor. Acceptance is durable even if publication is
pending. Release acceptance locks before waiting for the sequencer. A failed
publication wait must not be reported as an unaccepted write. A client validates
the receipt against its exact pending request (`Receipt::validate_for`) before
marking it accepted. Publication is verified by fetching checkpoint history.

Receipt lookup must be efficient and durable across restart. Never answer an
unknown accepted outcome by blindly creating another operation. A receipt is not
garbage-collectable while the service still promises retry recovery for its
operation identity; lifecycle changes must preserve that boundary explicitly.

## Bounded history and local installation

`HistoryRequest` has an exact `after` cursor and `limit`. `HistoryPage` contains
the same `after`, complete `commits`, a `next` cursor and `has_more`. Each
`PublishedCommit` contains one complete Core `CommitRow`, ordered tagged
`CommitEventRow` values, and the resulting checkpoint. Never split a commit
between pages or return an empty page with `has_more=true`. Concurrent later
publication does not alter the meaning of a previously served prefix.

The public hard bounds are 512 events per commit, 1 MiB per canonical event,
4 MiB per canonical submission, 64 commits per page, 32 MiB of serialized JSON
per page, 1,024 head guards, 256-byte identity/event-type strings and a 4 KiB
commit message. The history byte bound accommodates Core byte-vector JSON
expansion for a complete maximum-size commit. Services bound the producer query
and page construction, not only a final response after an unbounded history read.
HTTP clients cap response bytes before deserialization; request handlers cap
request bytes before deserialization. Do not rely on an in-memory validator to
protect an unbounded network parser.

`verify_history_page` checks exact cursor identity, limits, canonical payloads,
event/commit links, digests, commit IDs, increasing publication and the checkpoint
chain. `verify_published_commit` performs the envelope/hash checks alone.
These helpers do not prove cached parent columns, per-item heads or application
semantics. The client must additionally replay Core transitions against its last
verified state, validate available decrypted events, and install history,
projections and the next cursor in one transaction. Failure leaves the previous
durable cursor intact. Never expose a partially projected page as accepted UI.

## Immutable objects and encryption

Transfer the exact salted object framing as `application/octet-stream`. Validate
framing, digest and codec before immutable installation; a same-ID retry must not
replace different bytes. Downloads are bounded or streamed and verified before
their application output is trusted. A missing body is explicitly unavailable.
Thin replicas need not fetch every referenced content body before installing
verified envelope history. The authority must prove referenced objects durable
before accepting a new content-bearing proposal.

An encrypted product profile uses these same public objects and envelopes. Its
own contract defines encryption suites, authenticated context, keys, visible
fields, signatures and enrollment. This protocol does not invent a competing
cryptographic object format or permit an authority to inspect encrypted domain
data. Missing metadata remains different from metadata that decrypted incorrectly.

## Compatibility and evidence

The protocol has no migration from 0.1, no implicit downgrade and no peer merge
DAG. Product consumers pin the same exact public revision for `synck-vcs`, its
adapter dependencies and `synck-sync`; old SynckHub graphs may remain separately
pinned. Discovery must reject an incompatible protocol/profile clearly.

[The independent ASCII JSON vector](vectors/sync.json) fixes a submitted request,
canonical event bytes, event digest, commit identity and full request digest.
`cargo test --locked -p synck-sync --test protocol` exercises the vector,
receipt recovery matching, generation/guard binding, invalid shapes, publication
gaps, replay and tampered history. These are library checks, not a deployed
service, provider, native UI or scale qualification.
