# Integrating an application

An authoring application (a text editor, a design tool, a co-authoring server)
records its saved revisions in a Synck repository by running the `synck`
command. Any language that can start a process and parse JSON can integrate;
no library, socket or service is involved.

The application keeps its own live editing state: undo history, unsaved edits
and co-authoring operations stay in the application. Synck receives only saved
revisions, as the exact bytes of the document in its native format. Save the
format that carries the most information, not a lossy export.

## Setup

Write to a repository created by `synck init` or one made writable by
`synck repository activate`. Its configured author is recorded as the
committer of every write; set it to the application's identity:

```sh
synck init /path/to/repository --author app:example-editor
```

The application stores the `item_id` that Synck returns for each document.
Item IDs are permanent, including across renames and moves.

## Calls

| Purpose | Command |
| --- | --- |
| Create a document | `synck --json repository create-file <name> <source> [--parent <folder-id>] [write options] --path <repo>` |
| Save a revision | `synck --json repository save-file <item-id> <source> [--base-content <blob-id>] [write options] --path <repo>` |
| Read the current state | `synck --json item show <item-id> --path <repo>` |
| Read the current bytes | `synck --json item export <item-id> <destination> --path <repo>` |

`<source>` is a file holding the exact bytes to record. Write options:

- `-m, --message <text>`: commit message, 1 to 16,384 UTF-8 bytes.
- `--author <id>`: who made this revision, nonblank and at most 256 bytes.
  The repository author remains the committer.
- `--occurred-at <time>`: when the revision was made, as RFC 3339. Use it
  when saves reach Synck later than they happened, for example from an
  offline queue. Synck still records its own acceptance time.
- `--operation-id <uuid>`: a canonical lowercase UUID naming this one write.
  A repeated call with the same ID returns the commit (and, for
  `create-file`, the item) accepted the first time and adds no history.
  Reusing an ID for different bytes, another FILE or another location is
  refused.

`--base-content <blob-id>` is the content object ID returned by Synck for the
revision the application started editing from. In an unencrypted repository it
is the plain SHA-256 of those bytes. The save is accepted only while that is
still the document's current content; renames and moves do not affect it.

## Saving a revision

1. Before editing, record the base: `item.projection.content_blob_id` from
   `item show`.
2. On save, generate an operation ID and persist it with the pending save
   before calling Synck.
3. Run `save-file` with `--base-content`, `--operation-id` and the
   attribution options.
4. After a crash or timeout, repeat the identical call. The operation ID makes
   the retry return the original commit even though its base is no longer
   current.
5. On `CONFLICT`, someone else saved first. Export the current bytes, merge
   them in the application, and save again with the new base and a new
   operation ID.

## Output and errors

Success prints one JSON object to standard output: `create-file` returns
`schema_version`, `item_id` and `commit_id`; `save-file` returns
`schema_version` and `commit_id`. Both exit 0.

With `--json`, a failure exits nonzero and prints one object to standard
error, `{"schema_version":5,"error":{"kind":...,"message":...}}`. Branch on
`kind`, never on `message`:

| Kind | Meaning |
| --- | --- |
| `CONFLICT` | The current content is not `--base-content`. `error.current` holds `item_id`, `head_seq`, `blob_id` and `size` of the current revision. Nothing was written. |
| `BUSY` | Another process holds the repository. Nothing was written; retry the identical call. |
| `INVALID` | Bad input: unknown or non-live item, invalid option, occupied name, reused operation ID, or a dirty file in a checkout. |
| `FILE_FOLDER` | The FILE/FOLDER profile refused the write; the message carries its code. Saving unchanged bytes to a bare repository is `NO_OP`. |
| `ACCEPTED_NEEDS_RECOVERY` | The commit was accepted but local finalization failed. Run `synck repository finalize <repo>`; do not repeat the write as a new one. |

## Checkouts

When the repository has a filesystem checkout, `create-file` and `save-file`
also write the document at its path in the checkout and commit through the
ordinary selected-commit path. They refuse a target that has uncommitted
changes or lies outside the checkout selection. Saving unchanged bytes adds
no history. See [Portable repositories](portable-repository.md#optional-editable-filesystem-checkout).

## Limits

- A revision has one author. Co-authored revisions record their main author.
- Source bytes are read from a file; standard input is not accepted.
- One process writes at a time. `BUSY` is returned immediately rather than
  waiting.
- Writes require a FILE with local sequencing; federated items are refused.

## Agents sharing a local checkout

Before loading or editing a path, use `synck --json lock acquire PATH --owner
SESSION`, including for new files. Pass the returned `result.token` through the
global `--lock-token TOKEN` flag on `create-file`, `save-file` and checkout
mutations; library callers use `set_agent_lock_tokens`. Keep the base-content and
operation-ID checks above. Renew before expiry and release after the work is
recorded. On `LOCKED` or `LOCK_LOST`, preserve pending content and stop the edit.
See [local agent reservations](local-repository.md#local-agents-sharing-a-checkout)
for path coverage, takeover and the boundary with direct filesystem writes.
