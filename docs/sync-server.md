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
- It listens on loopback only. Protocol 1 requires TLS on any network;
  Caddy on the same host terminates it ([TLS](#tls)).
- `synck serve` has no JSON output; `--json` is refused.

## Try it on one machine

Without Caddy, a clone on the same machine talks to the server over plain HTTP.
`synck serve` prints each repository's ID as it starts (`serving <repo_id>
from …`):

```bash
SYNCK_TOKEN=<token> synck clone http://127.0.0.1:8740/v1/repositories/<repo_id> /tmp/laptop --author laptop
```

`http://` is accepted only for `localhost`, `127.0.0.1` and `[::1]`, and the
server listens only on loopback. Another machine, even on the same home
network, connects through HTTPS ([TLS](#tls)).

## Compared with Git's servers

Git has two built-in ways to serve repositories:

- `git daemon` speaks Git's own protocol with no login and no encryption. It
  is meant for public read-only access; pushing through it is off by default.
- Git's smart HTTP is `git http-backend` behind a web server such as Apache or
  nginx. The web server does the login and the TLS; Git does the transfer, and
  both fetching and pushing work.

`synck serve` sits closest to smart HTTP, as one program:

- **Login is built in.** Each device or agent gets a named token, read or
  write, for chosen repositories; revoking it takes effect at the next
  request. Git leaves this to the web server.
- **TLS comes from Caddy**, as smart HTTP's comes from its web server, but
  Caddy gets and renews the certificate itself.
- **Every commit is checked as it arrives**, as in the repository itself. A
  commit that touches a file someone else changed is refused with `CONFLICT`,
  and the client merges with `pull`, as `git push` is refused when the branch
  moved. There are no branches and no force push: history only grows.
- **Retries are safe.** A commit carries its ID and gets a receipt; a client cut
  off before the answer asks again and finds it accepted, never sending it
  twice.
- **Leases** let an agent reserve a file that cannot be merged, such as a Word
  document. Git servers have no locks (Git LFS adds them).
- **Offline commits are proposals.** A client keeps a complete, verified copy
  and keeps committing while the server is away; `push` sends those commits
  later, each with its ID. The server decides: a commit that touches a file
  changed on the server meanwhile is not taken, and `pull` puts its changes
  back in the client's files, merged, to be committed again. Git commits
  offline too, but a refused push leaves the merge or rebase to the user.
- **Transfer is one request per object**, where Git packs everything into one
  stream. A first clone or a commit of many new files is slower than Git.

## TLS

[`deploy/Caddyfile`](../deploy/Caddyfile) puts Caddy in front of the server.
Caddy obtains and renews a public certificate by itself, and clients check it
against the system's trusted roots. On a Linux host with a DNS name pointing at
it and ports 80 and 443 open:

1. Install Synck ([cli-install.md](cli-install.md)) and Caddy (its package
   runs it as the `caddy` service), then run `synck serve init` and `synck
   serve token add` as the owner.
2. Copy [`deploy/synck-serve.service`](../deploy/synck-serve.service) to
   `/etc/systemd/system/`, set the owner, the paths and the repositories, and
   run `systemctl enable --now synck-serve`.
3. Put the host's name in `deploy/Caddyfile`, copy it to `/etc/caddy/Caddyfile`
   and run `systemctl reload caddy`.
4. On each device: `SYNCK_TOKEN=<token> synck clone
   https://sync.example.com/v1/repositories/<repo_id> <directory> --author
   <name>`. `synck info` in a served repository prints its ID.

Leave the server on `127.0.0.1`: Caddy is the only listener on the network.

## Write path

- Upload pages are kept in `state.sqlite3` until their commit is accepted or
  the upload expires, 24 hours after its first page; expiry is never extended.
  Expired uploads are deleted when another page arrives. The streaming summary
  of each upload is in memory and rebuilt from its pages after a restart.
- An object upload is written to `<state>/tmp`, checked against its ID, then
  stored in the repository under the repository lock. Its form is checked
  when a commit names it, since only the commit gives it a role. It is
  durable once a commit is accepted: acceptance checkpoints the object store
  first, so an upload lost to a crash answers `OBJECTS_MISSING` and is sent
  again.
- The server keeps an idle connection to each repository's history and
  object databases. They keep their WAL when a connection closes, and a
  database with no connection open rebuilds its WAL index from the whole WAL
  at the next open.
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
  /objects` like content; acceptance installs them in the repository's
  history in one transaction, and `GET /objects` serves them from there.
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
