# The sync server

`synck serve` serves repositories over protocol 1 ([spec/protocol.md](spec/protocol.md))
to one owner's devices and agents and to the people they give a token. It is to
a hosted service what a plain Git server is to GitHub: no users, folder
permissions, encryption, mail or web UI. The code is `synck_local::server`.

## Use

```bash
synck serve init ~/synck-state
synck serve token add ~/synck-state --name laptop --access write --repository ~/Documents/personal
synck serve ~/synck-state --listen 127.0.0.1:8740 ~/Documents/personal ~/Documents/work
```

- `init` creates a private state root (mode 0700): `server.lock` and
  `state.sqlite3`.
- `token add` prints the token once; only the SHA-256 of its secret is kept. A
  token is `read` or `write` and names the repositories it may use. `token
  list` and `token revoke <name>` work while the server runs and apply at the
  next request.
- `synck serve STATE` takes repository directories that order their own
  history (not clones or followers) and routes each by its repository ID. It
  reads them without their lock, so their working trees keep working. One
  server per state root.
- It listens on loopback only. Protocol 1 requires TLS on any network, so a
  proxy on the same host (Caddy, nginx) terminates it.
- `synck serve` has no JSON output; `--json` is refused.

## Write path

- Upload pages are kept in `state.sqlite3` until their commit is accepted or
  the upload expires, 24 hours after its first page; expiry is never extended.
  Expired uploads are deleted when another page arrives. The streaming summary
  of each upload is in memory and rebuilt from its pages after a restart.
- An object upload is written to `<state>/tmp`, checked against its ID, then
  stored in the repository under the repository lock. Its form is checked
  when a commit names it, since only the commit gives it a role.
- A submission is verified again from its pages, then its request digest is
  recorded before acceptance, so a receipt always has its digest. Acceptance
  is `portable::ServedRepository::accept`: the repository lock (waited for, as
  a clone waits for its origin's), Core's checked FILE/FOLDER append with refs
  and head guards, then sequencing. One submission is accepted at a time
  across all repositories, and the whole proposal is in memory while it is
  accepted.
- The working tree of a served repository takes in accepted commits at its
  next `synck pull`, as it does a clone's. `synck gc` there may remove an
  object uploaded for a commit not yet submitted; the submission then answers
  `OBJECTS_MISSING` and the client uploads it again.
- A proposal needs every metadata object it names, uploaded through `PUT
  /objects` like content; acceptance installs it in the repository's history,
  and `GET /objects` serves it from there.
- An object download holds one of the 16 blocking-job slots while the client
  reads it.

## Clients

`synck clone https://HOST/v1/repositories/REPO_ID` makes a working tree that
pulls and pushes over these routes ([HTTPS clones](local-repository.md)). The
server's own repository directory keeps working as before; its working tree
takes in remote commits at its next `synck pull`.

## History and leases

- `POST /history` packs published history pages straight from the
  repository, without its lock. A cursor that is not on this history is
  `CHECKPOINT`.
- Leases follow `synck lock`: they live in the repository's own
  `.synck/agent-locks.sqlite3`, so a remote lease fences the repository's
  local working trees and a local reservation fences remote writers. `synck
  lock list` in the repository shows both. A submission presents its lease
  tokens in `synck-lock-token` headers; writing a reserved path without one is
  `LOCKED`, and a dead or uncovering token is `LOCK_LOST`. Lease changes and
  that check wait for the repository lock.
- The server cannot see a client's files: the client checks that the paths
  it leases are clean before it asks.

## Responses

Refusals are JSON `{"status": "..."}`, as the protocol's "HTTP refusals"
lists. Besides those, `SERVER_OVERLOADED` (503) means no blocking-job slot
within 5 s, or shutdown, and `INTERNAL` (500) is logged to stderr.

## Routes

All under `/v1/repositories/{repo_id}/sync`:

| Route | Token |
| --- | --- |
| `GET /capabilities` | read |
| `PUT /uploads/{operation_id}/pages/{offset}` | write |
| `POST /proposals` | write |
| `GET /receipts/{operation_id}` | read |
| `PUT /objects/{object_id}` | write |
| `GET /objects/{object_id}` | read |
| `POST /history` | read |
| `GET /leases` | read |
| `POST /leases`, `/leases/renew`, `/leases/release`, `/leases/break` | write |
