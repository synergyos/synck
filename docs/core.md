# Synck Specification

This document defines the engine data model, API semantics, structural
invariants and canonical history formats. The engine is independent of its
database backend: `Repo` uses the [`StorageBackend` and `StorageTxn`
interfaces](../crates/synck-vcs/src/storage/mod.rs) through a shared `Storage`
handle. Backends supply persistence, transactions and conflict synchronization;
Core owns canonical bytes, commit identities and structural validation.

This repository includes the `synck-vcs-sqlite` adapter used by the local library
and CLI. SynckHub uses a separate PostgreSQL adapter, `synck-vcs-postgres`, which
is not included here. Both implement the same engine storage interfaces.
[Local repositories](local-repository.md) describes the SQLite-based local
distribution. SQL type notation below describes logical storage fields, not a
requirement to use PostgreSQL or SQLite. The Rust API is pre-1.0; persisted
formats and wire protocols have their own versioned compatibility contracts.

The source and checked-in canonical vectors remain the authority for exact bytes:
[`synck-vcs` types](../crates/synck-vcs/src/types/mod.rs) and
[canonical vectors](../crates/synck-vcs/tests/canonical_vectors_test.rs).
This engine contract does not promise complete typed inspection or mutation
support for every application-defined item family.

---

## 1. Overview

**Synck VCS** is a **portable, embeddable version control engine** for structured data. It provides:

* **Append-only event logs** for items (files, chats, task lists, databases, tables, and other platform-defined item kinds)
* **Content-addressed commits** grouping events atomically
* **Linear history** per item via `item_seq`
* **Derived repo-wide activity order** via `repo_seq` (for scans/projections; not VCS ancestry truth)

Synck is inspired by Git’s content-addressed thinking, but the history model is intentionally **canonical and linear** at the item level: one authoritative event chain per item (no branching/merging workflows).
The core engine stays small and policy-free; higher layers add item semantics.

Per-item linear history does not impose one writer for an entire repository.
Multiple callers may submit writes; transactional conflict locks and head
comparisons serialize conflicting changes. PostgreSQL can run independent
transactions concurrently, while the SQLite adapter uses `BEGIN IMMEDIATE` to
serialize database writers. Neither backend creates divergent item branches or
provides a merge operation.

---

## 1.3 Blob Scope

**Blob content storage is out of scope for Synck VCS.**
Events may reference blobs; storing, transporting, and syncing blob content is the responsibility of the embedding platform or higher-level distributions.

---

## 1.4 Boundary of Responsibility

Synck VCS handles **sequencing, structural correctness, and persistence of the
versioned graph**. It stores opaque author identifiers and checks their required
shape and configured byte limits; it does not authenticate those identities.
The following responsibilities belong outside the engine:

* authentication and authorization
* application edit-lock ownership and mutation policy beyond Core's conflict
  locks and sequence/head checks
* application-specific item-type semantics beyond structural rules
* federation policy and governance
* permissions and organizational controls
* application search, domain read models, and their secondary indexes

Core does provide the derived historical-state tree and projection contracts
used for exact reads at sequenced commits. Those are distinct from application
search and domain projections.

This keeps the engine stable, auditable, and hard to misuse, and prevents the core from turning into a platform by accident.

---

## 1.5 Design Principles

1. **Small Core**
   Synck VCS defines invariants and sequencing, and persists the versioned state in a database.
   No platform logic, no blob persistence, no network protocol.

2. **One semantic storage contract**
   PostgreSQL and SQLite implement the same `StorageBackend` and `StorageTxn`
   semantics. Backend-specific physical schemas do not change VCS behavior.

3. **Append-only**
   History is immutable. Events are only added, never modified or deleted.

4. **Platform-agnostic**
   Application event verbs and payload semantics stay outside Core. Built-in
   structural rules still recognize folders and the checked FILE/FOLDER profile;
   access control and business rules belong to the embedding application.

5. **Embeddable**
   Usable as a library (Rust crate) or via optional CLI/HTTP wrappers and distributions.

6. **Portable**
   Runs against PostgreSQL in server deployments or self-contained SQLite in
   the standalone local distribution.

---

## 1.6 What Synck VCS Does

| Responsibility                                                       | Synck VCS |
| -------------------------------------------------------------------- | --------- |
| Store items and their metadata                                       | ✓         |
| Append events to items                                               | ✓         |
| Assign `item_seq` (per item, per event)                              | ✓         |
| Group events into commits                                            | ✓         |
| Assign `repo_seq` (repo-wide activity cursor; may be derived asynchronously) | ✓         |
| Compute content-addressed `commit_id`                                | ✓         |
| Enforce structural invariants required by an item kind (for example folder cycles or folder-child name uniqueness for hierarchical items) | ✓         |
| Track blob references in events                                      | ✓         |
| Store/retrieve blob content                                          | ✗         |
| Provide locks table                                                  | ✗         |


---

## 1.7 What Synck VCS Does NOT Do

| Responsibility                                                                | Platform Layer / Optional Modules |
| ----------------------------------------------------------------------------- | --------------------------------- |
| Authentication (who is the user?)                                             | Platform                          |
| Authorization, ACLs / permissions (can user X do Y?)                          | Platform                          |
| Application mutation policy and edit-lock ownership beyond Core's head checks | Platform                          |
| Item type registry and semantics (FILE vs CHAT behavior)                      | Platform                          |
| Application event payload schema and verb validation                           | Platform                          |
| Lock table and enforcement (require lock before append)                       | Platform                          |
| Federation protocol and peer governance                                       | Extensions / Platform             |
| Invitations, peer discovery, blob sync across repos                           | Extensions / Platform             |
| Application search, domain projections and secondary indexes                   | Platform                          |
| Admin CLI (export/import/verify/migrate)                                      | Synck CLI                         |
| HTTP API                                                                      | Embedding application             |
| Name reservations                                                             | Platform                          |

---

### 1.8 Federation Support

**Synck VCS** is a **local storage and sequencing engine**. It has no knowledge of network protocols, other repositories, or any Federated Item Sequencer. Federation is implemented above the engine, typically via **Synck Extensions** (open or closed) and/or platform code.

However, Synck VCS provides the hooks that federation-capable platforms and extensions need:

1. **Tracks federation metadata**
   Items can be marked as federated via `sequencing_mode = 'FEDERATED'` and `federation_stream_id`.

2. **Accepts pre-sequenced commits**
   For federated items, the platform (or a federation extension) supplies the `item_seq` assigned by an external Federated Item Sequencer rather than Synck VCS generating it.

3. **Enforces local invariants**
   Even for federated proposals (ingested into local commits), Synck VCS validates structural constraints (folder cycles, name uniqueness) and records blob references; blob storage and availability remain platform responsibilities.

For items with `sequencing_mode = 'LOCAL'`, platforms MUST use `append()`.
For items with `sequencing_mode = 'FEDERATED'`, platforms MUST use `ingest()`; `append()` MUST reject events for federated items.

**Federation orchestration lives outside Synck VCS**, in the embedding application or its protocol adapters:

* Communicating with the Federated Item Sequencer
* Sending/receiving invitations
* Fetching commit bodies from other homeservers
* Orchestrating blob availability and transfer (if the platform requires it)
* Calling Synck VCS `ingest()` with pre-sequenced commits

```
┌─────────────────────────────────────────────────────────────────┐
│                        PLATFORM / EXTENSIONS                     │
│  ┌──────────────┐  ┌──────────────────────┐  ┌───────────────┐  │
│  │ Federated    │  │  Blob sync policy    │  │ Other homes   │  │
│  │ Sequencer    │  │  + transport logic   │  │ / peers       │  │
│  └──────┬───────┘  └──────────┬───────────┘  └──────┬────────┘  │
│         │                     │                     │           │
│         └─────────────────────┼─────────────────────┘           │
│                               │                                 │
│               Federation + platform logic                       │
│     (auth, invitations, sequencer protocol, policy, sync)       │
├─────────────────────────────────────────────────────────────────┤
│                           Synck VCS                              │
│                    (local storage, no network)                   │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ append()                     - local items (assigns seq)  │  │
│  │ ingest()                     - federated items (accepts   │  │
│  │                               pre-assigned item_seq)      │  │
│  │ Tracks: sequencing_mode, federation_stream_id             │  │
│  └───────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

---

### Compliance Scope

* **Synck VCS compliance** means implementing the items/events/commits schema, append/ingest operations, structural invariants, and the defined identifier/sequence semantics.
* The CLI, HTTP server, federation modules, authentication helpers, and export/import are **wrappers and extensions** built on Synck VCS and are **not required** for Synck VCS compliance.

---

### 1.9 Engine API surface and compatibility contract

This section makes the engine boundary explicit for third-party embeddings.
The goal is simple: if you build on Synck VCS, you should know what you can rely on,
what can change, and what you must not do.

#### Public engine surface (normative)

This contract is **semantic**. The names below reflect the current Rust API, but callers MUST treat
the behavior defined in this spec as the stable interface (not Rust symbol names, module layout, or
backend implementation details).

The supported general write operations are:

* `append(repo_id, CommitProposal) -> AcceptedCommit`
* `append_with_refs(repo_id, CommitProposal, [RefEventProposal]) -> AcceptedCommit`
* `ingest(repo_id, FederatedProposal) -> IngestedCommit`
* `ingest_domain(repo_id, FederatedDomainProposal) -> IngestedDomainCommit`

The engine MAY also expose narrower semantic write operations whose contracts are
strict subsets of those general operations. Such operations MUST be named after
their semantic restriction, not after performance claims or hidden mode flags.

Additional specialized write operations include:

* `append_materializing_base_seq(repo_id, CommitProposal) -> AcceptedCommit`
* `ingest_content_only_existing_item(repo_id, FederatedProposal) -> IngestedCommit`

Write operations vary along three different axes, and callers MUST NOT blur them:

* **Sequencing authority**
  * `append()` is for `LOCAL` items; Core assigns `item_seq`.
  * `ingest()` is for `FEDERATED` items; the caller supplies externally assigned
    `item_seq`.
* **Change shape**
  * **content-only existing-item**: the target item already exists and the proposal
    changes only item content/events.
  * **structural / bootstrap**: create, rename, move, trash/tombstone, or any
    proposal that changes item placement, lifecycle, or requires bootstrap
    materialization.
  * **federation transition**: marking an item federated, unfederating it, or
    changing federation binding metadata.
* **Coordination policy**
  * optimistic / CAS
  * server-materialized `base_seq`
  * platform lock or edit-session policy

The **change-shape** axis is an engine semantic distinction.
The **coordination policy** axis is not.
For example, a `FILE::SET_CONTENT` write under an exclusive platform lock is still
**content-only existing-item** traffic, not a structural write.

Supported read operations are:

* `get_item(repo_id, item_id) -> ItemRow | null`
* `get_commit(repo_id, commit_id) -> CommitRow | null`
* `list_events(repo_id, item_id, from_item_seq, limit) -> [EventRow]`
  * Ordered by `item_seq` ascending.
* `list_ref_events(repo_id, owner_item_id, from_ref_seq, limit) -> [RefEventRow]`
  * Ordered by `ref_seq` ascending.
* `list_repo_events(repo_id, after?, limit) -> [EventRow]`
  * Returns only **sequenced** events (events whose containing commit has a non-null `repo_seq`).
  * Ordered by `(repo_seq, commit_event_idx)` ascending.
  * Cursor shape: `after = { repo_seq, commit_event_idx }`.
  * Cursor semantics: `after` is exclusive (results start strictly after the cursor).
* `list_commits_by_repo_seq(repo_id, from_repo_seq, to_repo_seq?, limit) -> [CommitRow]`
  * Returns only **sequenced** commits (commits with a non-null `repo_seq`).
  * Ordered by `repo_seq` ascending.
* `list_items(repo_id, after?, lifecycle_state?, limit) -> [ItemRow]`
  * Ordered by `(created_at, item_id)` ascending.
  * Cursor shape: `after = { created_at, item_id }`.
  * Cursor semantics: `after` is exclusive (results start strictly after the cursor).

The stable error taxonomy is the minimum set listed in section 3.5.

#### Invariants guaranteed forever (normative)

These invariants define “Synck VCS” in a way that callers can depend on:

* **Append-only history**: accepted commits/events are immutable; there is no supported mutation or rewrite of existing history.
* **Canonical event bytes**: stored item-event and ref-event payload bytes are
  RFC 8785 canonical JSON for their proposal type with the sparse rule in
  section 5.4.1.
* **Content-addressed commits**:
  * `events_digest` commits to the ordered event payload digests (section 5.4.2).
  * `commit_id` is `sha256:` of the canonical commit bytes (section 5.4.2).
  * `format_version` matches the binary’s supported commit format.
* **Linear per-item order**:
  * `item_seq` is strictly increasing per item starting at 1 (no gaps for accepted history).
  * attached `ref_seq` is independent of `item_seq`, starts at 1, and is
    strictly contiguous;
  * `commit_event_idx` is contiguous `0..event_count-1` per commit.
* **Repo activity order (derived)**:
  * `repo_seq` is an optional monotonic activity sequence per repo, starting at 1 (gaps allowed).
  * Only sequenced commits/events (non-null `repo_seq`) participate in repo-wide ordering and cursor scans.
  * Once assigned, a commit’s `repo_seq` never changes.
* **Structural invariants**: folder parent existence/type, folder acyclicity, name uniqueness (`name_key`), tombstone finality, and sequencing-mode routing rules.
* **Event row integrity**:
  * `events.event_payload_digest` matches `sha256(events.payload)`,
  * `events.payload` parses as `EventProposal` and re-canonicalizes to identical bytes,
  * `events.item_id` and `events.event_type` match the parsed payload,
  * `ref_events.event_payload_digest` matches `sha256(ref_events.payload)`,
  * `ref_events.payload` parses as `RefEventProposal`, re-canonicalizes to
    identical bytes, and its owner and event type match the row,
  * `events.parent_folder_id` / `events.parent_folder_id_before` match the replayed structural state.
* **Derived state fidelity**: `items` is a derived state map that MUST be rebuildable from the event log plus persistence materialization rules in this spec, including:
  * `item_type`, `parent_folder_id`, `name`, `name_key`, `lifecycle_state`,
  * `sequencing_mode` / `federation_stream_id` consistency,
  * `head_seq` matches the last event `item_seq`.
  * `ref_head_seq` matches the last attached ref event `ref_seq`, or zero when
    none exists.
* **Historical state fidelity**: `vcs_state_nodes`, `vcs_state_roots`, and
  `vcs_state_projection_heads` are disposable post-sequencing projections.
  Every published commit root MUST reproduce the exact Core state after that
  commit under the [historical-state implementation](../crates/synck-vcs/src/state/mod.rs). `state_root` and
  `repo_seq` MUST NOT participate in accepted `commit_id` bytes.

#### What can change (and how) (normative)

* Constraint/index names, query plans, and internal surrogate keys (such as `commit_pk`) are storage details and MUST NOT be treated as part of the contract.
* Implementations MAY add new error variants, but MUST preserve the meanings of the stable categories in section 3.5.

#### What embeddings MUST NOT do (normative)

* MUST NOT write to `vcs_*` tables directly. If you bypass the engine, you own the corruption.
  The only permitted direct `vcs_*` mutations outside the engine are whole-repo
  administrative data movement paths that do not reinterpret item history:
  schema creation/drop on an empty or disposable database, bundle import with
  explicit overwrite, and explicitly documented maintenance rollback/repair
  that deletes a complete repo-scoped VCS dataset together with its owning
  platform repo state. These paths MUST NOT update individual VCS item/event
  semantics in place.
* MUST NOT treat `vcs_items` as authoritative history; it is derived state.
* MUST NOT depend on specific index/constraint names or Postgres error strings.
* MUST NOT assume retries are idempotent unless you supply a stable `operation_id` for `append()` (see section 3.6).

#### 1.10 Import atomicity and recovery (normative)

Import guarantees:

* Canonical `import` mutation MUST execute as one DB transaction and MUST
  rollback on parse, digest, sequence, or canonical invariant failures.
* Canonical-history and terminal-item verification MUST pass before canonical
  commit. Derived historical-state projection runs after that commit and MUST
  complete before the import reports full success. Confirmed-partial and
  unknown-commit outcomes follow
  the [historical-state implementation](../crates/synck-vcs/src/state/mod.rs).

---

## 2. Data Model

**Synck VCS** defines a deliberately minimal data model for **local version control of structured items**. The engine stores exactly what it needs to maintain item history, commits, and structural correctness. It does not store platform policy, user identity, or network state.

Storage operations scope item and commit identities by `repo_id`. The PostgreSQL
adapter supports multi-repository server operation; the standalone local SQLite
format permits exactly the repository named by its config. These deployment
profiles do not change the engine's backend-independent history model.

### Storage Backend

The engine accepts any implementation of its `StorageBackend` and `StorageTxn`
contracts. The included SQLite adapter and the separately maintained PostgreSQL
adapter satisfy those interfaces. Logical relations are repository-scoped; for
example, the logical item key is `(repo_id, item_id)`.

The Rust library provides `StorageBackend` and `StorageTxn` traits. PostgreSQL
and SQLite implementations must satisfy the same ordering, atomicity,
constraint, read, and error contracts. The standalone SQLite profile is defined
in [Local repositories](local-repository.md).

### Table Overview

| Table | Purpose |
| --- | --- |
| `items` | State map; one row per item. Tracks existence, structure, `head_seq`, and `ref_head_seq`. |
| `events` | Append-only ordinary item log addressed by `(repo_id, item_id, item_seq)`. |
| `ref_events` | Append-only attached-ref log addressed by `(repo_id, owner_item_id, ref_seq)`. |
| `commits` | Atomic batches of item and/or ref events with a content-addressed `commit_id` and optional `repo_seq`. |

---

### 2.1 Items Table

The **items table** is the state map. One row per item per repo.
It tracks existence, type, structural placement in the folder graph, sequencing mode, and item/ref heads.

```
items
─────
repo_id              UUID NOT NULL
item_id              UUID NOT NULL
item_type            TEXT NOT NULL (opaque to Synck Core)
parent_folder_id     UUID (NULL for root)
name                 TEXT NOT NULL
name_key             TEXT NOT NULL (canonical name used for uniqueness)
head_seq             BIGINT NOT NULL
ref_head_seq         BIGINT NOT NULL DEFAULT 0
lifecycle_state      TEXT NOT NULL DEFAULT 'LIVE'      -- 'LIVE' | 'TRASH' | 'TOMBSTONED'
created_at           TIMESTAMPTZ NOT NULL
created_by           TEXT NOT NULL (opaque author identifier)
sequencing_mode      TEXT NOT NULL DEFAULT 'LOCAL'      -- 'LOCAL' or 'FEDERATED'
federation_stream_id TEXT                               -- NULL unless sequencing_mode = 'FEDERATED'

PRIMARY KEY (repo_id, item_id)
```

**Key points:**

* **Primary key is `(repo_id, item_id)`**. Each item is uniquely identified within its repo.
* `item_id` is generated by Synck Core on item creation (UUID v7) unless a `create_item` event proposal provides an explicit `item_id` (in which case Core MUST reject if it already exists).
* `item_type` is opaque to Synck Core. Platforms define their own types (FILE, CHAT, TASK_LIST, etc.).
  Synck Core may reserve a minimal built-in set (for example `FOLDER`) only for structural checks.
* `head_seq` is the highest `item_seq` applied to this item.

  * The creation event is `item_seq = 1`.
  * Applying the creation event initializes `head_seq = 1`; later events for
    that item in the same commit advance it normally.
  * For any existing item, head_seq MUST be >= 1 and MUST equal the greatest stored (repo_id, item_id, item_seq) in events for that item.
* `ref_head_seq` is the highest attached `ref_seq` applied to this item.

  * Zero means the item has no attached-ref history.
  * Ref-only commits update `ref_head_seq` without changing `head_seq`.
  * Normal writes allocate `ref_seq = ref_head_seq + 1` under the same locked
    item row used for compare-and-swap.
* `lifecycle_state` is the single lifecycle flag:

  * `LIVE` – item participates in path resolution and name uniqueness.
  * `TRASH` – soft deletion. Item is hidden from normal views and name
    uniqueness, but can be restored.
  * `TOMBSTONED` – terminal deletion. No new events are accepted; items may
    later be removed from application projections. Their accepted history and
    tombstoned Core state remain available for verification and replay.
* `parent_folder_id` points to the containing folder. `NULL` means the item is at root scope.
* `sequencing_mode` controls who assigns `item_seq`:

  * `LOCAL`: Synck Core assigns `item_seq = head_seq + 1` via `append()`.
  * `FEDERATED`: `append()` is rejected; `item_seq` must be supplied by the caller via `ingest()`.

**Constraints:**

* `repo_id` is required and scopes all identities and invariants.
* `ref_head_seq` MUST be non-negative.
* `sequencing_mode` MUST be one of: `LOCAL`, `FEDERATED`.
* `parent_folder_id` must reference an existing item in the same repo with `item_type = 'FOLDER'` (or be NULL).
* Within a folder, no two items may share the same name when both have
  `lifecycle_state = 'LIVE'`.

**Recommended indexes (logical constraints):**

* Name uniqueness within a folder, excluding trashed and tombstoned items:

  * Unique over `(repo_id, normalized nullable parent, name_key)` with predicate
    `lifecycle_state = 'LIVE'`; root names participate in the same uniqueness
    rule (section 4).
* Federated identity uniqueness within a repo:

  * `federation_stream_id` MUST be unique when `sequencing_mode = 'FEDERATED'` and `federation_stream_id IS NOT NULL`.

**Federation support (platform-managed):**

* `sequencing_mode`

  * `LOCAL`: Synck Core assigns `item_seq` for new events via `append()`.
  * `FEDERATED`: `item_seq` is assigned by an external Federated Item Sequencer; Core accepts it via `ingest()`.
* `federation_stream_id`

  * Opaque identifier provided by the platform, used to correlate this item across repos in a federation.
  * Synck Core stores it and enforces basic uniqueness/matching rules during federated ingestion, but does not implement any federation protocol.

### 2.2 Attached Ref Events

An item may own one attached ref ledger. The ledger is for references to
immutable item history that need an independent lifecycle and compare-and-swap
domain. It is not a second item and it cannot change item structure.

`RefEventProposal` contains `owner_item_id`, `event_type`, optional
`set_metadata`, optional `set_content`, and `base_ref_seq`. It has no create,
name, parent, lifecycle,
sequencing-mode, or federation-binding fields.

Local `append_with_refs` accepts a `CommitProposal` plus a separate
`ref_events` array; refs are not a field of `CommitProposal`. FOLDER-domain
`FederatedDomainProposal` carries both `events` and `ref_events` arrays.
At least one combined event is required. The
canonical order is every item event in array order followed by every ref event
in array order; `commit_event_idx` spans that combined order without gaps.

For a normal proposal, all ref events for one owner carry the same pre-commit
`base_ref_seq`. Core locks touched owner rows in stable item-id order, compares
the base with `ref_head_seq`, and allocates consecutive ref sequences. An owner
created by earlier item events in the same commit uses base zero. A missing or
tombstoned owner is rejected. The item/ref events, both heads, commit, and
current item state are one transaction. Historical-state roots are projected
separately after repository sequencing.

Every ref ledger starts at one and advances contiguously. There is no explicit
initial sequence, migration baseline, or maintenance API for bypassing its CAS.

`ref_events` rows have primary key `(repo_id, owner_item_id, ref_seq)`, unique
commit position `(repo_id, commit_pk, commit_event_idx)`, canonical payload and
digest fields, and the containing-commit foreign key. Repository activity
merges item and ref rows through their containing commit and orders the tagged
records by `(repo_seq, commit_event_idx)`.

---

### 2.2 Events Table

The **events table** is the append-only event log.
Each event belongs to exactly one item and exactly one commit.
Events are addressed by `(repo_id, item_id, item_seq)`.


```
events
──────
repo_id              UUID NOT NULL
item_id              UUID NOT NULL
item_seq             BIGINT NOT NULL (starts at 1, per item)
event_type           TEXT NOT NULL (namespaced label; verb semantics are opaque to Synck Core)
commit_id            TEXT NOT NULL     -- logical "sha256:<hex64>" commit id
commit_event_idx     INTEGER NOT NULL (position in commit, 0-indexed)
payload              BYTEA NOT NULL     -- canonical JSON encoding of EventProposal
event_payload_digest BYTEA NOT NULL     -- SHA-256 digest of payload bytes (32 bytes)
created_at           TIMESTAMPTZ NOT NULL
parent_folder_id     UUID                              -- parent AFTER this event
parent_folder_id_before UUID                          -- parent BEFORE this event (only set for moves)

PRIMARY KEY (repo_id, item_id, item_seq)
```

**Key points:**

* `(repo_id, item_id, item_seq)` is the canonical identity of an event.
* `item_seq` is strictly monotonically increasing per item, starting at `1`.
* `event_type` is a platform-defined label. For compatibility with existing
  projections and diagnostics, accepted write paths require it to be namespaced
  as `ITEM_TYPE::VERB` for item-specific events, or `ITEM::VERB` for generic
  item-scoped events. The prefix is a routing/audit check only; the verb after
  `::` does not define Core semantics.
* `payload` is the canonical JSON representation of the `EventProposal` for this
  event (see section 5.2). Synck Core interprets **structural fields** inside
  that JSON but treats the rest of the payload as platform data.
* `event_payload_digest` is the SHA-256 digest of the stored canonical `payload` bytes
  (see section 5.4). It is redundant, but lets backends validate / rebuild
  `commits.events_digest` without re-hashing large payload bytes.
* `parent_folder_id` / `parent_folder_id_before` are **denormalized structural hints**:

  * For non-move events (no `set_parent`), Synck Core MUST set `parent_folder_id` to the item's parent at the end of the event and leave `parent_folder_id_before` as NULL.
  * For move events (`set_parent`), Synck Core MUST set:
    * `parent_folder_id_before` to the item's parent before applying the move (may be NULL for root), and
    * `parent_folder_id` to the item's parent after the move.
  * These columns never participate in canonical commit hashing; they exist solely to make folder‑/subtree‑scoped event queries cheap and deterministic.
* In PostgreSQL:
  * `payload` is stored as canonical RFC 8785 JSON bytes (`BYTEA`),
  * `event_payload_digest` is stored as raw 32-byte SHA-256 digest bytes (`BYTEA`).

**Structural vs. opaque fields:**

* Synck Core treats the `event_type` verb as opaque. It enforces only the
  compatibility namespace described above.
* **Structural modifiers** on `EventProposal` (encoded in `payload`) are interpreted
  by Synck Core and update the `items` table under structural invariants:

  * `create_item`
  * `set_parent`
  * `set_name`
  * `set_tombstoned`
  * `set_in_trash`
  * `set_is_federated`
  * `set_federation_stream_id`
* `commit_id` and `commit_event_idx` link each event to its containing commit.

Embedding applications map their commands onto these modifiers; Core defines no
HTTP routes. The FILE/FOLDER profile creates a file through `FILE::CREATE` and
`FILE::SET_CONTENT` events in the same commit, so accepted file creation includes
its initial content. See the [profile materializer](../crates/synck-file-folder/src/materialize.rs).

Synck VCS enforces the semantics of these fields and the sequencing rules around
`item_seq` / `base_seq`. Embedding applications are responsible for mapping their
commands and per-item policies onto these modifiers. For example,
`FILE::SET_CONTENT` with `set_name` is a structural rename and is not content-only;
`CHAT::MESSAGE_CREATE` with only `set_content` / `set_metadata` remains
content-only.

The generic engine accepts non-structural `set_metadata` and `set_content`
without interpreting their application meaning, including on creation events.
Application profiles define their event conventions. The FILE/FOLDER profile
uses separate `FILE::CREATE` and `FILE::SET_CONTENT` events in one commit.
Creation cannot combine with structural or federation modifiers, except for the
federation identity materialized by ingest as described in section 3.2.

Platform item events are usually named in the short `ITEM_TYPE::VERB` form. A platform MAY also define deeper item-scoped paths when the stream owner is still the item itself but the event represents a domain-specific auxiliary mutation rather than a direct item-state update. Current built-in example: `ITEM::DATA_LINK::PUT|DELETE`.

**Constraints:**

* `item_seq` is strictly monotonically increasing per `(repo_id, item_id)`, starting at `1`.
* Each event belongs to exactly one commit.
* `(repo_id, commit_id, commit_event_idx)` is unique.

**Recommended foreign keys (logical):**

* events (repo_id, item_id) must reference items (repo_id, item_id).
* events (repo_id, commit_id) must reference commits (repo_id, commit_id).

Cross-repo references MUST be rejected.

**What Synck Core validates on accept (append/ingest):**

* The target item exists in the same `repo_id`.
* The target item is not tombstoned.
* `item_seq` is exactly `head_seq + 1` for that item (no gaps, no duplicates).
* For `append()` and `ingest_domain()`, `base_seq` for each existing item is consistent within the commit and equals the current `items.head_seq` (compare-and-swap precondition).
* For `append_materializing_base_seq()`, `base_seq` for events targeting an existing item is omitted (`null` / absent) and Core materializes it from the current local `items.head_seq` under the commit lockset.
* Structural modifiers do not violate:

  * parent existence
  * folder acyclicity
  * name uniqueness rules

**What Synck Core does NOT validate:**

* `event_type` verb semantics (platform responsibility).
* The application-specific schema inside metadata or content. Core still
  validates the typed proposal shape, event namespace, canonical encoding,
  configured limits and blob identifier syntax.
* Lock ownership and lock lifecycle (platform responsibility).
* For `ingest()` (federated item ingest), caller-provided `base_seq` values:
  Core ignores them and materializes `base_seq` from local state.

---

### Event Ordering

* **Per-item order**:
  `(repo_id, item_id, item_seq)` defines a strict linear history for an item.

* **Repo activity order (derived)**:
  Events are totally ordered for repo-wide scans only after they are **sequenced**:
  `commits.repo_seq` + `events.commit_event_idx` (within the same `repo_id`).
  Unsequenced commits (`repo_seq = null`) are not part of repo-wide ordering until a sequencer assigns their `repo_seq`.
  Concretely, the ordering key for scans is:

  * `(repo_id, commits.repo_seq, events.commit_event_idx)` where `commits.repo_seq IS NOT NULL`

This preserves strict per-item linear history while allowing repo-wide scans/projections to use a deterministic cursor.

---

### 2.3 Commits Table

The **commits table** stores atomic batches of events.
Each commit is scoped to a repo and groups events atomically.
Repo-wide ordering for scans/projections is provided by an optional derived `repo_seq` (see section 3.7); it is not VCS ancestry truth.
The same commit_id may legitimately appear in multiple repos; therefore commits are keyed by (repo_id, commit_id).

```
commits
───────
repo_id              UUID NOT NULL
commit_id            TEXT NOT NULL        -- "sha256:<hex64>" of canonical commit bytes
repo_seq             BIGINT              -- NULL until sequenced; starts at 1 per repo once assigned
parent_commit_id     TEXT                -- deprecated: MUST be NULL (no repo-wide ancestry chain)
author_id            TEXT NOT NULL (opaque)
	committer_id         TEXT NOT NULL (opaque)
	message              TEXT
	occurred_at          TIMESTAMPTZ             -- optional origin/author time (may differ from created_at)
	format_version       INTEGER NOT NULL     -- 2/3 item-only; 4/5 include attached refs
	import_job_id        UUID                    -- required for formats 3/5; absent for 2/4
	created_at           TIMESTAMPTZ NOT NULL
	event_count          INTEGER NOT NULL
	events_digest        TEXT NOT NULL        -- "sha256:<hex64>" (see section 5.4)

PRIMARY KEY (repo_id, commit_id)
```

**Key points:**

* `(repo_id, commit_id)` uniquely identifies a commit within a repository.
* `commit_id` is the SHA-256 digest of the **canonical commit bytes**.
  It is deterministic and content-addressed.
* `repo_seq` is a derived activity cursor assigned after commit acceptance.
  * While unsequenced, `repo_seq` is `null` and the commit is not returned by repo-wide cursor APIs.
  * Once assigned, `repo_seq` is strictly increasing per repo starting at `1`; gaps are allowed.
* `author_id` is opaque to Synck Core. Platforms interpret it as needed.
* `committer_id` is opaque to Synck Core. It identifies who *recorded* the
  commit into this repo (often the same as `author_id`).
* `created_at` is assigned by the receiving repository's storage authority when
  the commit is accepted. Append and ingest APIs MUST NOT derive it from caller
  input or origin metadata.
* `occurred_at` is optional commit metadata representing when the commit
  occurred in the authoring/origin context ("author date" in Git terms).
  * Platforms SHOULD set it for federated ingestion using the authority's
    commit timestamp.
  * When absent, platforms MAY treat it as equal to `created_at`.
* `event_count` is the number of events in this commit and MUST be `>= 1`.
* `events_digest` commits to the ordered event payloads without embedding them
  in the commit (see section 5.4).
* `import_job_id` is canonical lowercase UUIDv4 provenance shared by every
  commit of one bulk import. It is absent from formats 2/4 and required by, and
  hashed into, formats 3/5. It is not an idempotency key.

**Constraints:**

* `commit_id` is unique within a repo via the composite primary key `(repo_id, commit_id)`.
* For sequenced commits (`repo_seq IS NOT NULL`), `repo_seq` is strictly monotonically increasing per `repo_id`; gaps are allowed.
* A commit contains one or more events:

  * `event_count >= 1`
  * The combined number of item `events` and attached `ref_events` rows with
    `(repo_id, commit_id)` MUST equal `event_count`.

**Recommended indexes (logical constraints):**

* Uniqueness of `repo_seq` per repo (sequenced commits only):
  * Unique over `(repo_id, repo_seq)` for non-null `repo_seq` values.
* Efficient history scans:
  * Index on `(repo_id, repo_seq)` (Postgres may choose a partial index to exclude unsequenced `NULL` rows).

**Backend storage types (informative):**

The tables above describe the **logical** schema. Implementations are free to
use more compact native types in physical storage as long as the logical
contract is preserved on the wire:

* PostgreSQL backend:
  * `repo_id`, `item_id`, `parent_folder_id` stored as `UUID`.
  * `commit_id` stored as `BYTEA` (raw 32‑byte SHA‑256 digest).
  * `events_digest` stored as `BYTEA` (raw 32‑byte SHA‑256 digest).
  * `payload` stored as `BYTEA` (canonical RFC 8785 JSON bytes).

At the Rust API and HTTP boundaries, `RepoId` / `ItemId` are always
hyphenated UUIDv7 strings and `CommitId` is always `"sha256:<hex64>"`,
regardless of how they are represented in the underlying database.

**Why content-addressed commit IDs?**

* Integrity: the commit ID changes if the canonical commit bytes change.
* Deduplication semantics: identical commits can share the same ID across repos without coordination.
* Reproducibility: rebuild the same canonical bytes, get the same ID.
* Compatibility: aligns with Git-style content-addressed thinking.

**Atomicity:**

Synck Core validates all events in a commit.

Structural invariants (parent existence/type, folder acyclicity, name uniqueness) are evaluated on commits that can affect structure (create/move/rename/lifecycle structural modifiers).
Content-only commits (for example: chat message append, file content updates) do not pay a folder-graph walk tax; they rely on the pre-commit graph already being valid.

If any required checks fail, the commit is rejected.
If all succeed:

* `item_seq` is allocated for each touched item.
* All affected `items`, `events`, `ref_events`, and `commits` rows are written
  in one transaction.
  An embedding platform may use `Repo::append_guarded_in_transaction` to include
  its policy writes in that same transaction. Core validation and conflict locks
  are unchanged. The returned receipt is provisional until the caller commits;
  any error requires rollback of the entire transaction. No projection or
  external publication may precede commit. The caller resolves accepted-command
  replay through operation-id lookup before attempting another append.

This guarantees **no partial commits** and preserves strict per-item ordering invariants.
Repo-wide activity sequencing (`repo_seq`) may be assigned later by a sequencer as a separate step.

---

### 2.4 Blobs

Blobs are large immutable payloads such as file bodies, previews, and attachments.
Synck Core does **not** store blob bytes. It only records blob
identifiers referenced by events.

* Blob identifiers are carried inside the event payload, via
  `EventContent.blob_id` when `set_content` uses the blob variant.
* Each event may reference at most one blob id.
* Blob storage, availability, and transfer are platform responsibilities.
* Synck Core does **not** validate blob existence at commit acceptance time.

**Blob ID format:**

* `blob_id = "sha256:<hex>"` (content-addressed)
* Blobs are immutable and may be physically deduplicated by hash.

---

### 2.6 Federation Domains (Core)

For federated folder replicas (multi-item, authority-sequenced streams), Synck Core maintains a small cursor table per repo:

```
federation_domains
──────────────────
repo_id         UUID NOT NULL
domain_id       TEXT NOT NULL
last_domain_seq BIGINT NOT NULL
created_at      TIMESTAMPTZ NOT NULL
updated_at      TIMESTAMPTZ NOT NULL

PRIMARY KEY (repo_id, domain_id)
```

Semantics:

* `last_domain_seq` is the last applied domain proposal sequence for that `(repo_id, domain_id)` stream.
* `last_domain_seq` starts at `0` for a new domain.
* The cursor MUST be updated atomically with the commit write for `ingest_domain()` so that replays and out-of-order delivery are handled deterministically.

---

### 2.7 Logical persistence relations

Synck Core persists commits and their effects into logical relations:

- `vcs_items`
- `vcs_commits`
- `vcs_events`
- `vcs_ref_events` (the attached-ref ledger defined in section 2.2)
- `vcs_federation_domains` (core cursor table for `ingest_domain`)

Core additionally maintains three derived historical-state relations:

- `vcs_state_nodes` — repository-scoped immutable canonical node bytes keyed by
  their SHA-256 identity;
- `vcs_state_roots` — exact `(repo_id, repo_seq, commit_id)` to state-root
  mappings for sequenced commits;
- `vcs_state_projection_heads` — the last completely published root per
  repository, including the sequence-zero projection origin.

These relations are not accepted history and are omitted from logical bundles.
Their exact schema, encoding, foreign keys, publication, repair, and verification
contract is the [historical-state implementation](../crates/synck-vcs/src/state/mod.rs).

The schema is intentionally simple and optimized for correct ordering and crash safety.

The standalone SQLite backend maps the same logical relations to SQLite and
uses `application_id` plus `user_version` for its physical schema. Its exact
types, timestamp encoding, migrations, and local projection tables are defined
by [Local repositories](local-repository.md).

**vcs_items**

* `PRIMARY KEY (repo_id, item_id)`
* `CHECK (head_seq >= 1)`
* `CHECK (sequencing_mode IN ('LOCAL','FEDERATED'))`
* `CHECK (lifecycle_state IN ('LIVE','TRASH','TOMBSTONED'))`
* Partial UNIQUE `(repo_id, parent_folder_id, name_key)` WHERE `lifecycle_state = 'LIVE'` (folder-scoped name uniqueness for live items).
* Partial UNIQUE `(repo_id, federation_stream_id)` WHERE `sequencing_mode = 'FEDERATED' AND federation_stream_id IS NOT NULL` (federated identity uniqueness).
* FK (logical) `parent_folder_id` references `(repo_id, item_id)` with `item_type = 'FOLDER'` or NULL (enforced by core logic, not by a SQL FK).

**vcs_events**

* `PRIMARY KEY (repo_id, item_id, item_seq)`
* `CHECK (item_seq >= 1)`
* `UNIQUE (repo_id, commit_pk, commit_event_idx)` (commit-local ordering)
* `CHECK (commit_event_idx >= 0)`
* `CHECK (octet_length(event_payload_digest) = 32)`
* FK `(repo_id, commit_pk)` references `vcs_commits (repo_id, commit_pk)`

`vcs_events` does not store `repo_seq`. Logical event rows obtain repository
activity order from their containing `vcs_commits` row through
`(repo_id, commit_pk)`.

**vcs_commits**

* `PRIMARY KEY (repo_id, commit_pk)`
* `UNIQUE (repo_id, repo_seq)` (repo activity order for sequenced commits; `repo_seq` may be NULL until sequenced)
* `UNIQUE (repo_id, commit_id)` (content-addressed id; may be NULL during internal staging)
* `CHECK (repo_seq IS NULL OR repo_seq >= 1)`
* `CHECK (format_version >= 1)`
* `CHECK (octet_length(commit_id) = 32 when present)`
* `CHECK (parent_commit_id IS NULL)`
* `CHECK (octet_length(events_digest) = 32 when present)`
* `CHECK (event_count is NULL or >= 1)`
* `CHECK (either commit_id/events_digest/event_count are all NULL, or all present)`
* Index `(repo_id, repo_seq)` for sequenced scans

### 2.8 Identifiers, Hashes, and Timestamps

* `repo_id`, `item_id`: RFC 9562 UUIDv7; stored in Postgres as `UUID`; encoded in APIs as lowercase, hyphenated strings.
  * SQLite stores the same canonical lowercase, hyphenated values as TEXT.
* `commit_id`, `blob_id`: `"sha256:" + <64 lowercase hex>` of SHA-256 digest in APIs.
  * In Postgres core tables, `commit_id` and related digests are stored as raw 32-byte SHA-256 digest bytes (`BYTEA`), not the prefixed string form.
* `created_at`: stored in Postgres as `TIMESTAMPTZ`; encoded in timestamp-bearing
  interfaces as RFC 3339 UTC with `Z` suffix. Timestamp precision is a backend
  detail; timestamps need not be unique. Item order uses `item_seq`, repository
  activity uses `repo_seq`, and item-list cursors break timestamp ties with
  `item_id`.
  * SQLite stores fixed-width RFC 3339 UTC TEXT with exactly nine fractional
    digits and a `Z` suffix so lexical and temporal ordering are identical.

---

## 3. Core Library Operations

The main write families below accept local proposals and ingest externally
sequenced proposals. Core also exposes repository activity sequencing; bundle
administration and historical-state projection have their own APIs. Read
operations query item state, item/ref events, and commits.

### 3.1 Append (local items)

```
append(repo_id, commit_proposal) -> AcceptedCommit | Error
append_materializing_base_seq(repo_id, commit_proposal) -> AcceptedCommit | Error
append_file_folder_import_checked(repo_id, commit_proposal, ref_events,
                                 guards, subtree_guards, import_job_id)
  -> AcceptedCommit | Error
```

The append family writes items with `sequencing_mode = 'LOCAL'`.

`append_file_folder_import_checked()` is the same checked local append
transaction with one additional required canonical UUIDv4 provenance value. It
produces commit format 3 for item-only commits or format 5 when refs are present. It does not
relax proposal validation, limits, locking, sequencing, or projection rules.
Ordinary append entry points produce format 2 for item-only commits or format 4
when refs are present and do not accept import provenance.

`CommitProposal.operation_id` is a required, non-empty idempotency anchor for
`append()`. It identifies one caller intent and MUST be unique per repo among
accepted local appends.

On success, Core:

1. Validates structural invariants for all structural modifiers in the proposal:

   * target item exists for non-creation events
   * target item is not tombstoned
   * parent folder exists and is a folder when setting `parent_folder_id`
   * folder graph remains acyclic
   * name is unique within the parent among items that are not tombstoned and not in trash

Create-item constraints (normative):

* `create_item` events MUST NOT combine with structural modifiers
  (`set_parent`, `set_name`, `set_tombstoned`, `set_in_trash`) or federation modifiers.
* If a commit contains multiple events targeting a newly created item, the
  `create_item` event MUST be the first event for that item in the commit.

CAS / idempotency constraints (normative):

* For `append()`, for each existing item touched by the commit, all events for that item MUST
  carry the same `base_seq`.
* For `append()`, for each existing item touched by the commit, `base_seq` MUST equal the
  current `items.head_seq` observed under the engine's write synchronization for the commit's conflict set.
* For `append()`, if `base_seq` does not match `head_seq`, append MUST fail with `BaseSeqMismatch`.
* If `operation_id` already exists in `vcs_commits` for the same repo, append
  MUST fail with `OperationAlreadyApplied` and MUST NOT create a second commit.

2. Assigns sequences atomically:

   * each event receives `item_seq = head_seq + 1` for its item
   * the commit is accepted with `repo_seq = null` (unsequenced); repo-wide sequencing is assigned later (section 3.7)

3. Generates `commit_id`:

   * `commit_id = "sha256:<hex64>"` of the canonical commit bytes

4. Persists atomically:

   * updates `items.head_seq`
   * inserts `events`
   * inserts `commits`

Append does not:

* interpret application event verbs or application-specific payload schemas
* enforce lock ownership policies
* validate blob existence
* accept events for `sequencing_mode = 'FEDERATED'` items

If any event targets a federated item, `append()` MUST fail with `ItemIsFederated`.

**Engine limits (normative):**

Implementations MUST reject commits that exceed configured engine limits
(for example: maximum events per commit, maximum canonical event payload
bytes, or maximum total canonical payload bytes per commit). Such rejections
MUST surface as `InvalidProposal`.

#### 3.1.1 Append (server-materialized `base_seq`, content-only)

`append_materializing_base_seq()` is an append variant intended for high-contention multi-writer
streams where callers do not want to implement CAS retry loops for content-only updates.

This is a **change-shape specialization**, not a lock or edit-session mode.
Platforms MAY still require locks or drafts before calling it, but those policies are outside
Synck Core.

Constraints (normative):

* For events targeting an existing item, callers MUST omit `base_seq` (`null` / absent). Core MUST
  materialize `base_seq` as the current local `items.head_seq` observed under the engine's write
  synchronization for the commit conflict set.
* The operation MUST be **content-only**. Core MUST reject any commit containing any of:
  `create_item`, `set_parent`, `set_name`, `set_tombstoned`, `set_in_trash`, `set_is_federated`,
  `set_federation_stream_id`. Such proposals MUST fail with `InvalidProposal`.
  This is decided only from the typed `EventProposal` fields, never from the
  `event_type` verb.
* `operation_id` remains a required, non-empty idempotency anchor (same as `append()`).

This operation does not change any other `append()` rules: it still rejects events for items that
are already `sequencing_mode = 'FEDERATED'`.

#### 3.1.2 Checked FILE/FOLDER append

The shared FILE/FOLDER adapter uses one checked append entry point over the
ordinary local append transaction. It accepts the already materialized
`CommitProposal` plus zero or more subtree closure guards:

```text
SubtreeClosureGuard {
  root_item_id,
  expected_non_tombstoned_descendants: [{ item_id, head_seq }, ...]
}
```

There is no size cap on a prepared lifecycle closure. The root is protected by
its ordinary proposal baseline. Guards are sorted by root ID; descendant pairs are sorted by item ID.
Roots and guarded closures must be disjoint. Before allocating the lock set,
Core rejects an overlapping prepared guard set.

Under the ordinary append transaction, Core:

1. locks every touched item, guarded item, and old and new namespace parent in
   deterministic ID order;
2. rechecks the existing exact head guards;
3. traverses each guarded root's non-tombstoned descendants, requesting at most
   that guard's expected descendant count plus one;
4. returns `SubtreeChanged` if a sentinel appears or exact sorted membership
   differs from the prepared guard;
5. evaluates the final in-commit FILE/FOLDER graph and requires every LIVE
   non-root parent chain to consist only of LIVE folders;
6. applies the ordinary name, cycle, lifecycle, baseline, commit, and storage
   rules and publishes only the complete commit.

Preparation overflow is `SubtreeTooLarge`. A locked traversal overflow is
`SubtreeChanged`, because the accepted preparation no longer describes current
authority state. Storage traversal must stop at the sentinel; it may not
materialize or globally sort an unbounded closure first.

The platform's resumable large-folder trash uses a separate root-only checked
append guard after earlier commits have trashed all live children. Under the
root lock, Core rejects the append if a direct child is still `LIVE`. A live
descendant necessarily has a live direct-child path under the FILE/FOLDER
parent-closure invariant. This guard carries no descendant list and does not
weaken exact closure guards for ordinary lifecycle intents. The guard takes the
root's namespace lock, and checked restores take their current parent's
namespace lock, so a child cannot become live between that check and the root
trash commit.

The checked entry point retains ordinary event-namespace and typed-proposal
validation. It does not interpret application event verbs, inspect blob storage,
authorize users, or discover implicit lifecycle events. Callers without
FILE/FOLDER lifecycle guards pass an empty guard list. Existing unrelated flat
guarded-append users retain their current head-guard behavior.

---

### 3.2 Ingest Federated Item Proposal

```
ingest(federated_proposal) -> IngestedCommit | Error
```

This is the general write operation for items with `sequencing_mode = 'FEDERATED'`.

For engine-contract purposes, federated ingest also has two write shapes:

* **existing-item content-only ingest**:
  * the local item already exists for the given `federation_stream_id`,
  * all events are ordinary content events for that same item,
  * the proposal does not create, rename, move, trash/tombstone, federate,
    unfederate, or change `federation_stream_id`.
* **generic federated ingest**:
  * any bootstrap creation, structural mutation, or federation transition.

These write shapes are semantic and are independent of platform coordination policy.
For example, a federated `FILE` content write may still require an authority-owned exclusive lock
at the platform layer while remaining **existing-item content-only ingest** at the engine layer.

The caller provides the externally assigned `item_seq`. The proposal is addressed by
`federation_stream_id`; the ingesting repo resolves that to its local `item_id` and
materializes that `item_id` into all stored event payloads.

All events MUST target the same item; mixing items is rejected with `MultipleItemsInFederatedProposal`.
Any parent references (`create_item.parent_folder_id` or `set_parent`) MUST be local
`item_id` values in this repo; Core does not resolve federated parent identifiers.

Create-item constraints (normative):

* If the proposal creates a new item, the first event MUST be `create_item`.
* `create_item` events MUST NOT combine with structural modifiers
  (`set_parent`, `set_name`, `set_tombstoned`, `set_in_trash`).
* For federated bootstrap creation, Core materializes federation identity in the
  stored payload bytes (`set_is_federated: true`, `set_federation_stream_id: "<proposal.federation_stream_id>"`).

Core:

1. Resolves item identity:

   * If an item exists for this `federation_stream_id`, it MUST have `sequencing_mode = 'FEDERATED'` and its stored `federation_stream_id` MUST match the proposal.
   * If no such item exists, the first event MUST be `create_item` and Core creates a new item in `sequencing_mode = 'FEDERATED'` with the proposal’s `federation_stream_id` and a new locally-generated `item_id`.
     * `create_item.parent_folder_id` MAY be set but MUST reference an existing local folder in the same repo.
     * Core does not resolve federated parent identifiers; callers MUST supply local `item_id` values.
     * Persistence rule (normative): when bootstrapping a new federated item, Core MUST materialize the federation identity into the stored canonical payload bytes for the `create_item` event by ensuring the persisted `EventProposal` includes:
       * `set_is_federated: true`
       * `set_federation_stream_id: "<proposal.federation_stream_id>"`

2. Validates structural invariants (same rules as `append`, including `set_parent` moves).

3. Materializes `base_seq` deterministically:

   * Core ignores any caller-provided `base_seq` values in federated ingest.
   * For existing items, Core sets `base_seq` to the local `head_seq` observed at ingest time.
   * For bootstrap-created items, Core sets `base_seq` to `null`.

4. Validates sequencing:

   * provided `item_seq` MUST equal `head_seq + 1` for the item; subsequent events for the same item take `item_seq + (commit_event_idx - first_event_idx_for_item)`

5. Accepts and builds a local commit:

   * the commit is persisted with `repo_seq = null` (unsequenced); repo-wide sequencing is assigned later (section 3.7)

6. Persists atomically (items, events, commits).

Ingest does not:

* communicate with any external service
* validate signatures or federation policy
* validate blob existence
* resolve non-local parent identifiers
* require a federation protocol or invite mechanism (platform responsibility)

The same **engine limits** described for `append()` apply to `ingest()` and
MUST surface as `InvalidProposal`.

#### 3.2.1 Ingest (existing-item, content-only)

```
ingest_content_only_existing_item(federated_proposal) -> IngestedCommit | Error
```

This is a semantically narrower federated ingest operation for hot existing-item
content traffic.

Constraints (normative):

* An item for the proposal `federation_stream_id` MUST already exist locally.
* All proposal events MUST target that one existing item.
* The operation MUST be **content-only**. Core MUST reject any proposal
  containing any of:
  `create_item`, `set_parent`, `set_name`, `set_tombstoned`, `set_in_trash`,
  `set_is_federated`, `set_federation_stream_id`.
  This is decided only from the typed `EventProposal` fields, never from the
  `event_type` verb. No special `SET_CONTENT` event name is required.
* Caller-supplied `base_seq` remains ignored; Core materializes base/current head
  from local state as in general `ingest()`.

This operation does not define new semantics.
It is a stricter contract for the common existing-item content-only federated
case, and implementations MAY use a narrower ingest path under that stricter
contract.

---

### 3.3 Ingest Federated Domain Proposal (multi-item)

```
ingest_domain(federated_domain_proposal) -> IngestedDomainCommit | Error
```

This operation is used for **federated folder replicas** where an authority allocates a **monotonic proposal sequence** (`domain_seq`) for a domain stream (`domain_id`) and each proposal may touch multiple items.

Additional constraints (normative):

* All events MUST carry explicit `item_id` values; `create_item` events MUST include `item_id`.
* Per-item federation transitions (`set_is_federated`, `set_federation_stream_id`) are rejected in domain ingests.

Core enforces **monotonic domain sequencing** per `(repo_id, domain_id)`:

* The first accepted proposal for a domain MUST have `domain_seq = 1`.
* For any later proposal:
  * if `domain_seq == last_domain_seq + 1`, Core applies it,
  * if `domain_seq == last_domain_seq`, Core MUST treat it as a replay and return `IngestedDomainCommit { applied: false, ... }`,
  * if `domain_seq < last_domain_seq`, Core MUST reject with `DomainSeqOverlap`,
  * if `domain_seq > last_domain_seq + 1`, Core MUST reject with `DomainSeqMismatch` (gap / out-of-order delivery).

Core applies the proposal as a single atomic commit using the same structural invariants as `append()`:

* parent existence/type,
* folder acyclicity,
* name uniqueness for live items,
* tombstone finality.

`ingest_domain()` does not implement any federation protocol, signatures, or authz; it is purely a deterministic ingest primitive.

`ingest_domain_in_transaction(txn, repo_id, proposal)` performs the same operation
inside a caller-owned storage transaction. Embedders use it to bind imported task
identities atomically with the accepted history and domain cursor. Acquire
platform authority locks before Core's conflict locks. A successful result is
provisional until the caller commits; any later companion-write failure requires
rollback of the entire transaction. Do not publish receipts or start projection
before commit. Head replay still does not prove payload equivalence: the protocol
owner must verify the original accepted receipt and payload separately.

---

### 3.4 Result Shapes

```
AcceptedCommit {
  commit_id: string
  repo_seq: integer|null  // null until sequenced (section 3.7)
  created_at: timestamp
  occurred_at: timestamp|null
  created_items: [(event_idx, item_id)]  // only for create_item events
  event_item_seqs: [integer]  // item-event input order
  ref_event_seqs: [integer]   // ref-event input order
}

IngestedCommit {
  commit_id: string
  repo_seq: integer|null  // null until sequenced (section 3.7)
  created_at: timestamp
  occurred_at: timestamp|null
  item_seq: integer
  federation_stream_id: string
}

IngestedDomainCommit {
  applied: bool
  commit_id: string|null
  repo_seq: integer|null
  created_at: timestamp|null
  occurred_at: timestamp|null
  domain_id: string
  domain_seq: integer
  domain_seq_end: integer
}
```

---

### 3.5 Error Categories (minimum stable set)

Implementations MUST expose these error classes, even if the internal shape differs.
This stable taxonomy applies to the core API and any HTTP surfaces that wrap it.

* `EmptyCommit`
* `ItemNotFound`
* `ItemTombstoned`
* `ItemAlreadyExists` (creation conflict)
* `ParentNotFound`
* `ParentNotFolder`
* `ParentNotLive`
* `NameConflict`
* `NameInvalid`
* `NameTooLong`
* `CycleDetected`
* `ItemIsFederated`
* `ItemNotFederated`
* `FederatedIdMismatch`
* `BaseSeqMismatch`
* `ItemSeqMismatch`
* `DomainSeqMismatch`
* `DomainSeqOverlap`
* `CommitIdMismatch`
* `MultipleItemsInFederatedProposal`
* `EventCountMismatch`
* `CommitEventIdxMismatch`
* `IllegalFederationTransition`
* `RepoSeqConflict` (unique constraint)
* `DuplicateCommit` (same canonical commit already stored)
* `OperationAlreadyApplied` (same `(repo_id, operation_id)` already stored)
* `SubtreeTooLarge`
* `SubtreeChanged`
* `StorageError`
* `InvalidProposal`
* `InvalidId`
* `InvalidTimestamp`

---

### 3.6 Duplicate commits and idempotency

Synck uses content‑addressed commits:

* `commit_id` is `sha256:` plus the hexadecimal SHA-256 digest of the canonical
  commit bytes described in section 5.4.2.
* For a given `(repo_id, commit_id)` there is **at most one** stored commit; this
  is enforced by a unique index in the storage layer.

As a result:

* `append()` idempotency is keyed by `CommitProposal.operation_id`, not by
  transport retries:
  * storage enforces uniqueness on `(repo_id, operation_id)` for non-null
    operation ids in `vcs_commits`,
  * the first accepted append for an operation id wins,
  * later replays of the same operation id MUST return
    `OperationAlreadyApplied { .. }` and MUST NOT create a second commit.
* Ordinary item `ingest()` has no caller-supplied operation ID. Domain ingest
  derives an internal operation ID from the domain ID and proposal sequence,
  while domain sequence rules decide replay/overlap behavior. The dedicated
  `ingest_federated_item_refs()` entry point requires a stable operation ID and
  validates replay payload equivalence. These mechanisms do not change commit
  identity.
* When a uniqueness violation occurs while sealing commit content, Core MUST map it to the most specific stable error:
  * `(repo_id, operation_id)` conflict -> `OperationAlreadyApplied { .. }`
  * `(repo_id, commit_id)` conflict -> `DuplicateCommit { .. }`
  * another commit uniqueness conflict -> `RepoSeqConflict { .. }`
  * other backend failures remain `StorageError { .. }`

This means:

* A caller that wants exactly-once local append intent MUST provide a stable
  `operation_id` across retries.
* A fresh `operation_id` means “new intent” and may produce a new commit.
* Callers that implement at-least-once delivery SHOULD treat
  `OperationAlreadyApplied` as “already committed” and MUST NOT retry
  with the same operation id as a new mutation.

### 3.7 Sequencing and Edge Cases

* `item_seq` is authoritative:
  * `item_seq` is allocated at commit acceptance and is strictly increasing per item starting at 1 (no gaps for accepted history).
* `repo_seq` is a derived repo-wide **activity cursor** used for scans/projections, not VCS ancestry truth:
  * commits are accepted with `repo_seq = null` ("unsequenced"); their events
    inherit that visibility and never store a second sequence value,
  * a repo activity sequencer assigns `repo_seq` later,
  * once assigned, a commit’s `repo_seq` never changes,
  * for sequenced commits, `repo_seq` is strictly increasing per repo starting at 1; gaps are allowed.
* Cursor safety requirement (normative):
  * `list_repo_events(after={repo_seq, commit_event_idx})` MUST NOT miss commits/events.
  * Therefore, implementations MUST NOT allocate `repo_seq` by simply calling `nextval()` (or equivalent) inside the commit write transaction.
    Transactions can allocate ids and commit out of order; cursor scans would skip late commits.
  * Reference Postgres approach:
    1. accept commits with `repo_seq = null` and events grouped by
       `(repo_id, commit_pk)`,
    2. in a separate sequencer transaction, lock the repository's activity-head
       row to serialize sequence allocation for that repository,
    3. select committed unsequenced commits with `FOR UPDATE SKIP LOCKED`,
    4. assign `repo_seq` in `ORDER BY commits.created_at ASC, commits.commit_pk ASC`,
    5. update the commit rows and activity head atomically.
       Repo-wide scans MUST join `commits.repo_seq` onto events through
       `(repo_id, commit_pk)`; events do not persist a denormalized copy.
* `parent_commit_id` is deprecated and MUST be `null` (no repo-wide parent chain).
* `commit_event_idx` MUST be contiguous 0..`event_count - 1` in the order provided; holes or duplicates are `CommitEventIdxMismatch`.
* `event_count` MUST equal the number of events written; mismatch yields `EventCountMismatch`.
* If a commit touches the same item multiple times, `item_seq` increments in event order starting from `head_seq + 1`; structural checks evaluate against the evolving in-commit state.
* Proposal validation, identifier/sequence materialization, structural checks and
  canonicalization complete before persistence. The commit hash is computed from
  the resulting canonical event bytes. Callers must handle the returned stable
  error category rather than depend on a universal ordering of validation failures
  across different append/ingest entry points.
* Federation transitions are expressed via the `set_is_federated` and `set_federation_stream_id` fields:

  * In persisted event payloads, `set_is_federated = true` with a non-empty `set_federation_stream_id` is:
    * required for federated bootstrap creation via `ingest()` (Core materializes it into the stored `create_item` event), and
    * allowed once more in that same bootstrap commit as a structurally pure, same-stream replay event when preserving the source item's event sequence, and
    * otherwise only allowed in commits accepted via `append()` for items whose current `sequencing_mode = 'LOCAL'`.
  * `set_is_federated = false` (with no `set_federation_stream_id`) is only allowed in commits accepted via `ingest()` for items whose current `sequencing_mode = 'FEDERATED'`.
  * At most one federation mode change is allowed per item per commit; violations are `IllegalFederationTransition`.

---

## 4. Structural Invariants (Core)

Synck Core enforces these invariants on every `append()` and `ingest()`:

1. **Folder acyclicity**
   The parent graph must be acyclic.

2. **Name uniqueness**
   Within a folder, no two items may share the same name when both have `lifecycle_state = 'LIVE'`.

   Implementations MUST enforce name semantics via a canonical `name_key` derived from the
   user-visible `name`:

   * `name` is a UTF‑8 string supplied by the caller and stored as-is for display.
   * `name_key` is computed by:
     * validating the display name:
       * non-empty, not just whitespace;
       * not `"."` or `".."` after trimming;
       * no ASCII control characters `U+0000–U+001F` or `U+007F`;
       * no zero-width / format characters `U+200B`, `U+200C`, `U+200D`, `U+2060`, `U+FEFF`;
       * no `/`, `\`, `<`, `>`, `:`, `"`, `|`, `?`, `*`;
       * name must not end with an ASCII space or dot;
       * name must not equal any reserved Windows device name (case-insensitive): `CON`, `PRN`,
         `AUX`, `NUL`, `COM1..COM9`, `LPT1..LPT9`.
     * normalizing the name to Unicode NFC;
     * enforcing a per-segment maximum of **255 UTF‑8 bytes** on the NFC form;
     * applying locale-independent Unicode lowercasing to obtain `name_key`.
   * Core MUST return:
     * `NameInvalid` if validation fails (bad characters, reserved names, etc.);
     * `NameTooLong` if the NFC form exceeds the byte limit;
     * `NameConflict` if two live items in the same folder share the same `(parent, name_key)`.
   * Storage backends MUST enforce uniqueness on `(repo_id, normalized parent key, name_key)`
     for items where `lifecycle_state = 'LIVE'`, mapping the root's null parent to
     one backend-appropriate key, and surface violations as `NameConflict`.
     Backend conflict probes MUST use the same normalized nullable-parent key
     expression as the uniqueness index; they MUST NOT rely on nullable-parent
     predicates that prevent the storage backend from using that index on large
     repositories.

3. **Parent existence and type**
   `parent_folder_id` must reference an existing folder in the same repo, or be NULL for root.
   The checked FILE/FOLDER append additionally enforces parent-closed LIVE state
   at the final commit boundary. Generic Core items retain this base invariant.

4. **Single parent**
   Items have at most one parent. No multi-homing.

5. **Tombstone finality**
   Once `lifecycle_state` becomes `TOMBSTONED`, no new events are accepted for that item.

6. **Sequencing mode rules**

   * `LOCAL` items accept only `append()`.
   * `FEDERATED` items accept only `ingest()`.

---

## 5. Commit and Event Proposals

### 5.1 CommitProposal

```
CommitProposal {
  author_id: string
  committer_id: string
  message: string?
  occurred_at: timestamp?      // optional origin/author time (RFC3339 UTC)
  operation_id: string         // required non-empty idempotency anchor for append()
  events: [EventProposal]   // may be empty when append_with_refs supplies ref events
}
```

### 5.2 EventProposal

```
EventProposal {
  item_id?: string?         // required unless create_item is set; optional/null for create_item input (Core generates if omitted)
  event_type: string

  // Non-structural metadata (tags, labels, small JSON blobs, etc.).
  // Core records this without interpreting its application-specific schema.
  set_metadata?: JsonValue

  // Structured content body for the event.
  //
  // This is optional at the event level; if absent, the event does not
  // carry any content changes.
  // Canonical form is a strict union: exactly one of text/json/blob_id/content_id.
  set_content?:
    { text: string } |
    { json: JsonValue } |
    { blob_id: string } | // "sha256:<hex64>" blob identifier
    { content_id: string } // strict lowercase "sha256:<hex64>" logical content ID

  create_item?: {
    item_type: string
    parent_folder_id?: string?   // absent/null => root
    name: string
  }

  // Set parent (tri-state):
  // - absent => no parent change
  // - null   => move to root
  // - "<UUID>" => move under that folder
  set_parent?: string?
  set_name?: string

  set_tombstoned?: bool
  set_in_trash?: bool

  set_is_federated?: bool
  set_federation_stream_id?: string

  // Snapshot of the item's head_seq the caller observed when
  // constructing this event.
  //
  // - For append()/ingest_domain() events targeting an existing item, MUST be an integer.
  // - For append_materializing_base_seq() events targeting an existing item, MUST be null/absent.
  // - For create_item events, MUST be null (no prior history).
  //
  // All events in a commit that target the same (repo_id, item_id)
  // MUST carry the same base_seq value (or all null for the synthetic
  // "new item" case).
  // For append()/ingest_domain(), this value is a CAS precondition and MUST
  // equal the current items.head_seq for that item.
  // For append_materializing_base_seq(), Core materializes base_seq under the
  // commit's conflict lockset; callers do not supply it.
  //
  // Federated ingest note:
  // - For `FederatedProposal` ingest, callers MAY omit `base_seq` (null) for
  //   events targeting an existing item; Core ignores any provided `base_seq`
  //   values and materializes `base_seq` from the local `head_seq` observed at
  //   ingest time (or null for bootstrap creation).
  base_seq?: integer | null // absent treated as null; required for existing-item append()
}

Persistence rule (normative):

* When persisting an accepted commit, Core MUST materialize `EventProposal.item_id`
  for *all* events (including `create_item`), generating a new id if the input
  omitted it. The stored canonical event payload bytes (`events.payload`) MUST
  contain that materialized `item_id` string (never `null`).
```

Core interprets only the structural modifiers and `base_seq` CAS constraints
above to update `items`. `event_type` must satisfy the namespace rule for
accepted writes, but its verb is platform-defined. Core does not interpret the
application values of `set_metadata` or `set_content`. Blob identifier syntax is
checked by Core; blob storage and availability remain outside it.

The **canonical JSON payload** stored in `events.payload` is exactly the JSON
encoding of this `EventProposal` structure, subject to the canonicalization
rules in section 5.4.

---

### 5.3 FederatedProposal (input)

`FederatedProposal` is a **proposal** for a federated update, not a pre-baked commit
object. Each repo ingests the proposal and constructs its own local commit
(`commit_id`, optional `repo_seq`) in its `commits` table.

```
FederatedProposal {
  federation_stream_id: string
  item_seq: integer
  author_id: string
  committer_id: string
  message: string?
  occurred_at: timestamp?      // optional authority commit time (RFC3339 UTC)
  events: [EventProposal]            // length >= 1
}
```

All events in a `FederatedProposal` MUST target the same `(item_id, federation_stream_id)`
once mapped into the local repo:

* The proposal itself is expressed in terms of `federation_stream_id` and `item_seq`.
* The ingesting repo resolves `federation_stream_id` to its local `item_id` and
  uses that id in all `EventProposal.item_id` fields before building the local commit.
  * Callers SHOULD omit `EventProposal.item_id` (or set it to `null`) in federated proposals.
  * If a caller supplies non-null `EventProposal.item_id`, it MUST match the local `item_id` resolved for `federation_stream_id`; otherwise the proposal is rejected.

`author_id` identifies the principal whose authority and accountability the
mutation uses. `committer_id` identifies the direct human, agent, service, or
federation worker that recorded the commit in this repository. Both are
required, non-empty opaque byte strings subject to the same configured length
bound. Equality is common for direct writes but is not an invariant. Core does
not infer, normalize, or substitute either identity.

---

### 5.4 Canonical JSON and commit hashing

All implementations MUST generate identical canonical bytes for the same logical
event and commit.

Rules (normative):

* Canonical form is RFC 8785 canonical JSON encoded as UTF-8 bytes.

#### 5.4.1 Canonical `EventProposal` JSON bytes (stored in `events.payload`)

The canonical bytes for an event are the RFC 8785 canonical JSON encoding of
the `EventProposal` object with a hard **sparse** rule:

* Only fields that are semantically present MUST appear in the JSON object output.
* Fields that are conceptually absent MUST be omitted (not emitted with JSON value `null`).
* `null` is allowed only where it has defined semantics (for example `set_parent: null`
  meaning “move to root”).
* Nested objects behave the same:
  * if the nested object is absent, the entire nested object field MUST be omitted,
  * if the nested object is present, its absent fields MUST be omitted.

This rule exists to prevent multiple encodings of the same logical event while keeping
format evolution tractable (new optional fields do not rewrite existing payload bytes).

Numeric note (normative):

* RFC 8785 canonicalization defines JSON number serialization in terms of IEEE 754 / ECMAScript semantics.
* Canonicalization MUST reject an integer if its RFC 8785 serialization would
  change its exact numeric value. The rejection is `InvalidProposal` and occurs
  before hashing or persistence. JSON deserialization MUST preserve an integer's
  exact decimal value until this check has completed, including values outside
  native `i64` and `u64` ranges. Integers in the inclusive range
  `-9007199254740991` through `9007199254740991` are always lossless; larger
  integers are accepted only when their canonical decimal spelling preserves
  the value exactly. Callers MUST encode rejected integers as strings. IEEE 754
  floating-point values remain subject to RFC 8785 serialization.

#### 5.4.2 Canonical `Commit` JSON bytes (for hashing only; not stored)

A commit is a small header object that commits to its ordered event payloads
via `events_digest` + `event_count`. The canonical commit JSON object has the
following versioned fields, and every field defined by the selected version is
always present:

* `object_type` – must be `"vcs_commit"` (domain separation)
* `format_version` – integer `2`, `3`, `4`, or `5`
* `author_id` – string
* `message` – string or `null` (null when absent)
* `parent_commit_id` – must be `null` (reserved; the engine has no repo-wide ancestry chain)
* `import_job_id` – formats 3 and 5 only; canonical lowercase UUIDv4 string
* `event_count` – integer `n` (must be `>= 1`)
* `events_digest` – string `"sha256:<hex64>"`

Version dispatch is strict:

* Format 2 has exactly the historical fields and bytes defined above except
  `import_job_id`; that key MUST be absent. Existing format-2 commit IDs do not
  change.
* Format 3 has the same fields plus required `import_job_id`. The dedicated
  checked import append creates it; bundle and replication paths preserve it
  when transferring accepted history.
* Format 4 has the format-2 fields and requires at least one attached ref event.
  Its `events_digest` hashes each payload digest after a one-byte event-family
  tag: `0` for an item event and `1` for a ref event. Every item event precedes
  every ref event.
* Format 5 has the format-3 fields and the same tagged event digest and ref-event
  requirement as format 4.
* Unknown versions are rejected. A reader never retries a failed hash under a
  different format or drops an unknown field to manufacture a match.

**Identity, placement, and authority (normative):**

These are separate facts and MUST remain separate:

* `commit_id` identifies immutable commit content. It answers which canonical
  header and ordered event payloads were accepted. It does not prove repository
  membership, repository order, or which authority accepted the commit.
* `(repo_id, repo_seq, commit_id)` identifies the commit's placement in one
  repository after sequencing. `repo_id` and `repo_seq` MUST NOT affect
  `commit_id`; the same content may therefore have the same `commit_id` in more
  than one repository, whose repository-local positions may differ.
* Authority or inclusion evidence is supplied by the protocol that exposes the
  placement, such as an authenticated receipt, checkpoint, replication
  response. Such evidence MUST bind the relevant repository and placement to
  `commit_id`, but it is not commit content and MUST NOT change `commit_id`. A
  bare commit ID or copied placement tuple is not proof of authority.

Federation authority ordering is likewise external to the receiving
repository's commit identity and placement. Its protocol evidence binds the
authority stream and sequence; local ingestion independently creates and
places the receiving repository's commit.

`operation_id` is likewise outside commit identity. It deduplicates one local
append intent; it neither identifies commit content nor proves inclusion.

**Repo ancestry note (normative):**

* The engine does not define a repo-wide parent chain.
* Implementations MUST set `parent_commit_id = null` for all commits.

**Repository state note (normative):**

* `repo_seq` and any repository state root MUST NOT participate in canonical
  commit bytes or `commit_id`.
* Commits are accepted with `repo_seq = null`; a state root for a repository
  position cannot exist until the separate repository activity sequencer has
  assigned that position.
* Requiring either value during commit hashing would serialize independent
  item writers behind one repository-wide head and is contrary to the Synck
  acceptance model.
* A historical state root, when present, is derived post-acceptance data mapped
  to an already immutable `(repo_id, repo_seq, commit_id)`. Failure or lag in
  that projection MUST NOT change, reject, or invalidate the accepted commit.

No other keys or platform fields participate in commit hashing beyond the
version-selected fields above and the event payload bytes. In particular,
`repo_id`, `repo_seq`, `operation_id`, `created_at`, `occurred_at`, and
`committer_id` never affect `commit_id`.

```
event_bytes_i = canonical EventProposal or RefEventProposal JSON bytes (RFC 8785, UTF-8)
event_hash_i = SHA256(event_bytes_i)                                  // 32 bytes
digest_input_i = event_hash_i                  // formats 2/3
digest_input_i = family_tag_i || event_hash_i  // formats 4/5: one byte, item=0/ref=1
events_digest_bytes = SHA256(digest_input_0 || ... || digest_input_{n-1}) // 32 bytes
commit_bytes = canonical Commit JSON bytes (RFC 8785, UTF-8)
commit_id = "sha256:" + hex(SHA256(commit_bytes))
```

Digest field string representation rule (normative):

* Canonical JSON always uses `"sha256:<hex64>"` for SHA-256 digests.
* The PostgreSQL and SQLite adapters store raw 32-byte digests for `commit_id`
  and `events_digest`; the engine's logical identities retain the prefixed form.

## Core commit metadata bounds

`CoreConfig` owns optional UTF-8 byte limits for `author_id` and
`committer_id`, commit `message`,
and append `operation_id`, alongside its event-count and canonical-payload
limits. Core validates them before any commit row or event is persisted through
append, batch append, federated ingest, domain ingest, or import-backed append.
Limit violations use the existing invalid-proposal error.

The compiled `CoreConfig` defaults are 256 bytes for author/committer and
operation IDs, 4,096 bytes for a message, 10,000 events, 1 MiB for one canonical
event, and 16 MiB aggregate canonical event bytes. Permanent `synck-local` repositories
explicitly leave the three metadata limits disabled and otherwise retain their
existing acceptance policy.

### Embedding-platform commit evidence

After a successful new local commit insert, append calls
`StorageTxn::record_authored_commit` inside the same transaction, before publishing
its events. The default is a no-op. An embedding backend may persist request
evidence in its own companion schema; failure aborts the append. This hook does
not change canonical bytes or identity. Duplicate operations and sealed-history
ingestion/replication do not invoke it. Standalone backends need no platform table.
