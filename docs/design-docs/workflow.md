# Workflow: how work moves through tandem

Tandem is jj with the store on the other side of a network. The server holds
a jj+git colocated repo and a bucket behind it; a client is the same `tandem`
binary anywhere, running stock jj commands whose store traits speak HTTP. A
per-workspace daemon watches the files and publishes what changes. Nothing an
agent does has to know any of that.

This file is the workflow. The shape it rests on is
[the target architecture](./target-architecture.md), whose invariants are
binding; this is what those invariants look like from the outside.

## Roles

**The server** — one per repository, on a VM or a container.

- `tandem up --repo /srv/project --listen 0.0.0.0:13013 --bucket s3://…`
- Holds the jj+git colocated repo, and treats it as disposable: the durable
  truth is the bucket's write-ahead log, and the repo is a materialization of
  it that any `tandem up --bucket <same url>` can rebuild.
- Orders publishes (op-head compare-and-swap), fans out wake-ups, mints
  workspace tokens, tracks who holds each workspace's writer role.
- Is the only place git runs: `jj git push`, `jj git fetch`, `gh pr create`.

**An agent** — one workspace each, on whatever machine or container it likes.

- `tandem clone <server> <dir> --workspace <name>` once, then `tandem daemon
  <dir>` for as long as the agent lives.
- Runs ordinary jj commands through the same binary: `tandem log`,
  `tandem diff`, `tandem bookmark create`.
- Never touches git, and never checkpoints anything.

## 1. Stand the server up

```bash
# On the server
mkdir /srv/project && cd /srv/project
jj git init --colocate
jj git remote add origin git@github.com:org/project.git
jj git fetch

tandem up --repo /srv/project --listen 0.0.0.0:13013 \
  --bucket 's3://tandem-project?region=us-east-1'
```

`--bucket` is what makes the server replaceable. Without it the WAL goes in a
directory inside the repo, which is fine for a laptop and not fine for
anything that is supposed to survive the machine.

`tandem up` prints an admin token unless `--admin-token` gave it one. That
token mints every other token and does not belong on an agent's machine.

## 2. Give an agent a workspace

The admin token mints a short-lived bearer scoped to one workspace:

```bash
curl -s http://server:13013/api/tokens \
  -H "Authorization: Bearer $TANDEM_ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"workspaceId":"agent-a","ttlSeconds":3600}'
# → {"token":"…","workspaceId":"agent-a","ttlSeconds":3600}
```

That token is what the agent gets, and all it can do is add commits, move
`agent-a`'s own workspace pointer, and move bookmarks under `agent-a/`. The
server checks it as a diff of the published view, so the limit holds however
the client was told to behave.

```bash
# On the agent's machine or in its container
tandem clone server:13013 ~/work/project --workspace agent-a --token "$TOKEN"
cd ~/work/project
tandem daemon . &
```

`clone` writes the server address, the workspace name and the token next to
the store, so ordinary commands in that directory need no flags.
`TANDEM_SERVER`, `TANDEM_WORKSPACE` and `TANDEM_TOKEN` override the files,
which is how one baked image serves many agents — see
[baking a sandbox image](../images/README.md).

A workspace name is an identity. Two daemons on one name is not parallelism:
the second is refused the writer role, says so, and keeps running without
publishing. Parallel agents get parallel workspaces.

## 3. The agent works

```bash
cd ~/work/project
ls src/                                  # real files, on real disk
echo 'pub fn auth() {}' > src/auth.rs
```

That is the whole of it. The daemon sees the write, waits out the debounce
window, and publishes the burst as one jj operation that the server
acknowledges only once the WAL entry and the index are durable in the bucket.
There is no checkpoint command, and no `jj` command has to be run for work to
be safe. The durability window is the debounce interval and nothing else:
`--debounce-ms`, or `TANDEM_DEBOUNCE_MS`.

The agent still gets every jj verb when it wants one:

```bash
tandem log
tandem describe -m 'feat: add auth'
tandem bookmark create agent-a/task-42 -r @
```

Bookmarks are namespaced by workspace, and the token enforces it. `main` is
not something an agent can move.

## 4. Agents see each other

```bash
# Agent B, a different machine, a different workspace
tandem clone server:13013 ~/work/project --workspace agent-b --token "$TOKEN_B"
cd ~/work/project
tandem log                                 # agent A's commits are there
tandem file show -r agent-a/task-42 src/auth.rs
```

A publish anywhere wakes every subscribed daemon over `GET /api/events`. What
a woken daemon does is mark its workspace stale — and stop. It does not run
`jj workspace update-stale` for you, ever: that moves files under whoever is
editing them, which is a decision and not a reflex. A container that has just
booted is the one moment nobody is editing, which is why the image entrypoint
may do it and the daemon may not.

Concurrent publishes do not race to a winner. Op heads are kept and merged;
a lost compare-and-swap comes back as a 412 and jj's own transaction retry
converges.

## 5. The integrator ships

```bash
# On the server
cd /srv/project
jj log
jj diff -r agent-a/task-42
jj rebase -b agent-a/task-42 -d main       # linear, no merge commit
jj git push --bookmark agent-a/task-42
gh pr create --base main --head agent-a/task-42
```

Or, when the review happened here and GitHub is only the mirror, move `main`
itself — which is now a fast-forward, because the rebase made it one:

```bash
jj bookmark set main -r agent-a/task-42
jj git push --bookmark main
```

`main` advances one way: an integrator rebases a ready stack onto it. Agents
merge in the repo, never on disk.

## 6. Upstream comes back

```bash
# On the server, after the PR lands
jj git fetch
```

Agents see the new commits on their next command, or as a wake-up if they are
subscribed. Nothing has to be pulled.

## Where the truth lives

The bucket. Not the server's disk, and not GitHub.

- One publish is one immutable WAL entry; the op-heads set is one
  compare-and-swapped index object. `POST /api/heads` acknowledges only after
  both are durable.
- The server's repo is a cache of that log. Lose the machine, run
  `tandem up --bucket <same url>` on another, and it replays.
- GitHub is a mirror for CI, review and people outside. It is where work goes
  to be seen, not where it goes to be safe.

## What is parked

The always-on integration workspace (`--enable-integration-workspace`) is
still in the binary and is not part of this workflow. Continuous recompute
does not survive continuous snapshotting: it would recompute a merge for every
file save, over states that are mid-edit by construction. The direction when it
comes back is an on-demand conflict query over ready bookmarks, writable
nowhere, runnable by anyone.
