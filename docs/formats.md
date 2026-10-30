# Format and compatibility

Synck 0.2.0 uses repository format 7, Core SQLite schema 7, commit format 6,
object frame V1 and history bundle format 5. These numbers name different
contracts; they need not match. The [standalone specification](spec/format.md),
its SQL schemas and language-neutral vectors define the persisted format.
Where implementation and specification disagree, the implementation is wrong.

Before 1.0, each release reads only its own repository format. Other versions
are refused without migration or modification. Format 6 belongs to Synck 0.1.x;
format 7 belongs to 0.2.0. Repositories written by untagged development builds
are disposable. Rust APIs are also pre-1.0 and consumers must pin a revision.
No compatibility reader, automatic converter or silent reinterpretation exists.

For a manual 0.1-to-0.2 transfer, use the 0.1 executable to run
`export-tree HEAD` into a new directory. Then use 0.2 to initialize and commit
that directory. FILE bytes transfer; old history, typed chat/email records,
identities and labels do not. Keep the original repository and its reader if
those records matter. There is no history converter in this release.

Normal writes never rewrite accepted history. `revert` adds a compensating
commit. Future administrative replacement must use a new history generation,
validate dependencies and explicitly reconcile stale clients. It cannot change
an already retained archive. These semantics do not mean a mutable local disk
is WORM storage or satisfies financial-record retention rules.

Complete copies preserve IDs, generation and exact stored objects. Thin repositories
can omit referenced payloads and remain structurally valid; complete archival
exports cannot. A long-term archive must retain a compatible reader or verified
migration independently of this project's pre-1.0 support policy.

Private cache schemas are not portable contracts. Deleting a disposable cache
causes replay, not a history migration. Canonical database structure is validated
by tables, types and keys, not SQL formatting or performance-index names.

After 1.0 the core format is frozen, and item families are versioned separately;
see [Core and item families](spec/format.md#core-and-item-families). The 0.2 typed
scope is FILE/FOLDER, CHAT and EMAIL. Hosted-only readers and old
FILE sidecars/resource forks were removed. Generic envelopes still provide
structural integrity for uninterpreted families. No cross-machine sync protocol
is included in this release.

`ItemReader::export_cache` / `ItemReader::from_cache` persist a disposable
versioned incremental reader snapshot, including independent ref positions and
historical FILE revisions. This is plaintext derived state, not a repository
format, canonical archive or authorization proof. A checksum detects damage;
callers must bind trusted storage to repository/item identity and a separately
verified history checkpoint, bound input size, and discard/replay unsupported or
invalid cache versions. Export and restore cost O(item state size); use periodic
snapshots with a verified replay tail rather than exporting the whole chat for
every message. No Core, event, hash or wire format changes.
