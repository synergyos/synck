# Converting format 7 history to format 8

The rules `synck-convert` applies to one repository's published format 7
history. It is storage-free: a driver reads commits in `repo_seq` order, with
each commit's events in commit-event-index order and the stored bytes of the
metadata objects they name, and writes what the converter returns. The CLI has
no conversion command.

## What stays

Every `repo_seq`, `item_seq` and `ref_seq`, every acceptance and author time,
operation ID, author, committer, message, event type and row field such as
`parent_folder_id`. No event is added, removed or moved, so clients and other
systems that hold sequences keep them. The history generation stays; the
genesis changes because it hashes the format.

Encrypted repositories are not converted. A converter cannot rewrite metadata
it cannot read, so it refuses the first encrypted object.

## What changes

- Each envelope is the format 8 encoding of the same proposal.
- Each commit ID and checkpoint is recomputed. The converter first checks that
  the source commit hashes to its format 7 ID, so it converts only what was
  published. The driver keeps the old-to-new commit ID map until no stored
  reference to an old ID remains.
- Item metadata becomes complete maps. Format 7 folded patches: `CREATE` and
  `SET_METADATA` fold, and an erased one clears every key; a record's
  `SET_TITLE` sets `title`; a FILE event with new bytes sets `content`; an
  erased one of those two removes only its key. A two-segment event's new map
  is the item's folded metadata after the event plus the event's own keys, so a
  key that did not fold, such as a revision's origin, stays on its event. Null
  values, which meant unset, are dropped.
- An EMAIL's `mailcal` stays only on `EMAIL::CREATE`. A later payload's
  `mailcal`, as a repair job wrote, is not carried over; the converter counts
  these in `dropped_mailcal`.
- `FILE_VERSION::*` label metadata drops `file_item_id`, `operation_id`,
  `source_commit_id` and `source_commit_event_idx` (the owner, the commit and
  the revision already hold them).

A payload already in its final form keeps its object. A rewritten object keeps
its source object's salt, so a rerun gives the same object and commit IDs and an
interrupted conversion can restart. An erased payload keeps its ID: in format 8
an erased latest payload makes every key unavailable. Subitem payloads, which
are whole replacements already, keep their objects.

`crates/synck-convert/tests/repository_vector.rs` converts the format 7
repository vector into exactly the format 8 one.
