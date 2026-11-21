# Converting format 7 history to format 8

The rules `synck-convert` applies to one repository's published format 7
history. It is storage-free: a driver reads commits in `repo_seq` order, with
each commit's events in commit-event-index order, the stored bytes of the
metadata objects they name and the stored size of each content object their
converted content names, and writes what the converter returns. The CLI has no
conversion command.

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
- Format 7 event rows repeat their commit's acceptance time in `created_at`
  and an item event's previous parent in `parent_folder_id_before`. Format 8
  stores neither: the time is the commit's and the previous parent is that of
  the item's previous event.
- Every content entry gains its object's stored size, which format 7 did not
  record: the driver supplies it for each object of an event's `set_content`,
  and for the originals an `EMAIL::CREATE` or a `POST` names in metadata (see
  below). It reads a present object's length from its store; for an erased
  one only what format 7 metadata recorded can give it, such as a FILE's
  `content.size`. An event naming an object without a size is refused.
- A FILE's size moves from metadata to its bytes entry. A FILE content event's
  `content` must be exactly `{"size": n}` with `n` the supplied size, or the
  event is refused; erased metadata is not checked. `content` leaves every
  FILE map, and a FILE event whose map is then empty, while the item's previous
  one was too, carries no metadata.
- Each commit ID and checkpoint is recomputed. The converter first checks that
  the source commit hashes to its format 7 ID, so it converts only what was
  published. The driver keeps the old-to-new commit ID map until no stored
  reference to an old ID remains.
- Item metadata becomes complete maps. Format 7 folded patches: `CREATE` and
  `SET_METADATA` fold, and an erased one clears every key; a record's
  `SET_TITLE` sets `title`, and an erased one removes it. A FILE's `content`
  is gone before this fold. A two-segment event's new map is the item's
  folded metadata after the event plus the event's own keys, so a
  key that did not fold, such as a revision's origin, stays on its event. Null
  values, which meant unset, are dropped.
- An EMAIL keeps its originals and loses its renderings. The Hub referenced
  both from `mailcal`: the original in `source.blob_id`, or for a provider
  message the raw MIME in `raw_rfc822_blob_id` (unless its provenance is
  `generated`), and the provider's JSON in `provider_raw_json_blob_id`. These
  become the `source` and `provider` roles of `EMAIL::CREATE`. `body`,
  `body_html_blob_id`, a generated RFC 822, `snippet`, `inline_images` and
  `text_repair` are dropped: a reader rebuilds them from `source` with the
  current parser, so the Hub's repaired text comes from the repaired parser. An
  email with no original is refused. The Hub's `archive_command_id` and
  `origin.upload_intent_id` are dropped, and `archived_at` becomes
  `imported_at`. `mailcal` stays only on `EMAIL::CREATE`; a
  later payload's `mailcal`, as a repair job wrote, is not carried over, and the
  converter counts these in `dropped_mailcal`. The repair events themselves stay
  where they are.
- A Hub POST's body is an uploaded Markdown blob. The Hub named it with the
  `body` role, which elsewhere holds salted text; format 8 gives each role one
  form, so it becomes `bytes`. Early POSTs kept it in metadata as
  `body_markdown_blob_id`; the event's own key becomes its `bytes` role and
  leaves every map. Comments, which are salted text, keep `body`.
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
