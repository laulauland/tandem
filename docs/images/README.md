# Baking a sandbox image

A cold client knows nothing. Its first command asks the server for every file,
tree, commit, operation and view it needs before it can answer — and an agent's
first command is usually "show me the repository". On a large repository, over
a real network, that is the slowest thing the agent will do all day, and it
does it again in every fresh container.

None of it has to happen at boot. The bake is two ordinary commands, run at
image build time so that what they leave behind becomes layers:

1. **`tandem clone`** materializes a workspace on disk and fills the client
   cache (`TANDEM_CACHE_DIR`) with every object it fetched on the way.
2. **`tandem workspace update-stale`** — the boot's own first step, run early.
   It walks the operation log and leaves jj's commit index next to the
   workspace, along with the operations and views it read. Without it the image
   is warm in files and stone cold in history, and a boot pays for the server's
   whole operation log instead of for the delta. [Why the second command is not
   optional](#why-the-bake-has-two-commands) has the measurements.

A container started from the image attaches to the same workspace name and asks
the server only for what has been published since the image was baked — a price
that is a function of the delta, and not of how long the server has been up.

There is deliberately no `tandem cache warm`. The cache has no CLI surface at
all: baking is the ordinary commands, run somewhere that keeps their output.

[`Dockerfile`](./Dockerfile) is the template.

## Build

BuildKit is required, and `--secret` is why: the legacy builder has no secret
mount, so on it the token could only arrive as a build argument, which is a
layer. If `docker build` says "BuildKit is enabled but the buildx component is
missing or broken", install the plugin:

```bash
mkdir -p ~/.docker/cli-plugins
id=$(docker create --entrypoint /bin/true docker/buildx-bin:latest)
docker cp "$id:/buildx" ~/.docker/cli-plugins/docker-buildx
docker rm "$id" && chmod +x ~/.docker/cli-plugins/docker-buildx
```

(`--entrypoint /bin/true` because that image has no default command, and
`docker create` refuses a container without one.)

```bash
# The token goes in a file, not on a command line and not in a build argument.
printf %s "$TANDEM_ADMIN_TOKEN" > /tmp/tandem-token

docker build -f docs/images/Dockerfile \
  --network=host \
  --secret id=tandem_token,src=/tmp/tandem-token \
  --build-arg TANDEM_SERVER=127.0.0.1:13013 \
  --build-arg TANDEM_WORKSPACE=agent-a \
  --build-arg BAKE_STAMP="$(date -u +%Y%m%dT%H%M%SZ)" \
  -t tandem-sandbox .
```

Four flags earn their place:

- **`--network=host`** — the bake has to reach the server, and build steps do
  not join the user-defined networks that `docker run
  --network` can. With host networking a server on `127.0.0.1:13013` is
  reachable from inside the build. A server that already answers to a name the
  build daemon can resolve needs no flag at all.
- **`--secret`** — a `--build-arg` is readable in `docker history` for as long
  as the image exists. A bearer that speaks for a workspace has no business
  being shipped inside the thing it opens. The secret is mounted for one
  command and leaves no layer, and the token file `tandem clone` writes next to
  the store is deleted in the same step, so the finished image carries a
  workspace and its objects and no authority whatsoever.
- **`--build-arg BAKE_STAMP=…`** — without it, a rebuild is not a rebuild. See
  [Refreshing the bake](#refreshing-the-bake) below; skipping this flag is the
  single easiest way to ship a stale image and believe it is fresh.
- **`-t`** — the image is the artifact. Bake one per repository state you want
  agents to start from.

The build compiles the binary in a `rust` stage. To reuse one you have already
built *for this runtime* — a host binary carries the host's libc and will not
generally run in `debian:trixie-slim` — override the stage:

```bash
# The directory replaces the builder stage, so it has to be laid out the way
# that stage leaves things: the binary at out/tandem inside it, because the
# sandbox stage copies /out/tandem out of it. A directory with `tandem` at its
# root fails the build with "/out/tandem: not found".
mkdir -p /tmp/prebuilt/out && cp path/to/tandem /tmp/prebuilt/out/tandem

docker build -f docs/images/Dockerfile \
  --build-context builder=/tmp/prebuilt \
  ... # as above
```

## Refreshing the bake

Running `docker build` again does **not**, on its own, re-run the clone. It is
worth being blunt about this, because the build reports success either way.

BuildKit keys each `RUN` on the expanded text of the command and on the layers
beneath it. It does not key on anything the command goes and reads: not the
secret, which is excluded from the cache key on purpose, and not the state of
the server. So a second build with the same binary and the same arguments
finds every layer cached, prints `CACHED` next to the bake, and hands back an
image carrying the bake it already had — from whenever that was, and possibly
against a server that no longer exists.

`BAKE_STAMP` is what makes a rebuild a rebuild. It is part of the bake
command, so a new value is a new cache key:

```bash
docker build … --build-arg BAKE_STAMP="$(date -u +%Y%m%dT%H%M%SZ)" -t tandem-sandbox .
```

Two ways to tell what you actually got:

```bash
# What the build did: a real bake runs the RUN, it does not print "CACHED".
docker build … 2>&1 | grep -A1 'tandem clone'

# What the image holds: the stamp, the server, and the workspace it was baked
# against. The entrypoint also prints this line as the container starts.
docker run --rm --entrypoint /bin/cat tandem-sandbox /etc/tandem-bake
```

Omitting `BAKE_STAMP` is legitimate when the cached bake is what you want —
iterating on the entrypoint, for instance — which is why the default is a
constant rather than an error. It is never legitimate on a scheduled rebuild.

## Boot

```bash
docker run --rm --network=host -e TANDEM_TOKEN="$TANDEM_WORKSPACE_TOKEN" tandem-sandbox
```

`--network=host` for the same reason the build needed it, and for as long as
the server is only reachable on the host's loopback. A server with an address
of its own needs no flag.

The entrypoint does two things.

First it runs `tandem workspace update-stale` — the same command the bake ran,
which is why it is cheap here. The image holds the repository as it stood when
the image was built, and the workspace name may have been published to since.
This is the step that brings `/work` to the last published snapshot, and with
the baked cache and the baked index behind it, everything it needs except that
delta is already on disk. The daemon deliberately will not do this for you:
moving files under whoever is editing them is a decision, not a reflex. A
container that has just booted is the one moment when nobody is editing them
yet, which is what makes it the entrypoint's job and not the daemon's.

Then it runs `tandem daemon /work`, for good. The daemon claims the writer
role, subscribes for head changes, and publishes every burst of file changes in
`/work` as one jj operation. Whatever edits `/work` — an agent, a compiler, a
person — never learns that tandem is there, and there is no checkpoint command
for it to forget to run.

The token is the one thing the image does not have and cannot be given at build
time without becoming a liability. Mint a short-lived workspace-scoped one per
container:

```bash
curl -s http://your-server:13013/api/tokens \
  -H "Authorization: Bearer $TANDEM_ADMIN_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"workspaceId":"agent-a","ttlSeconds":3600}'
```

Re-cloning at boot would be redundant: it asks the server questions the image
already knows the answers to. Boot into the daemon.

## Why the bake has two commands

A clone reads exactly one operation and one view: its own. Everything the
server has ever done is therefore still a cache miss when the clone finishes,
and it is worse than a miss. Attaching to a workspace name deliberately builds
its operation as a *sibling* of the server's history rather than on top of it —
the reason is a token-scope one and it is spelled out in `workspace_init.rs`.
The first command that loads the repository at head has to merge the two sides,
and jj builds its commit index from the newest ancestor operation that already
has an index. For a clone, no operation on the server's side has one. So it
walks all of them: every operation, every view, and the commits their heads
name, one serial round trip each.

That cost is a function of how long the server has been up, and it lands at
boot — which is exactly the cold start the image exists to remove. Measured
against a server carrying forty commands of unrelated history, booting two
images built minutes apart against the same server and the same two-file delta:

| bake | objects | operations | views | total reads |
|---|---|---|---|---|
| `clone` only | 59 | 188 | 67 | **314** |
| `clone` + `workspace update-stale` | 6 | 10 | 4 | **20** |

For scale, the same arrival with no image at all — clone and catch up in a cold
container — cost 297. A clone-only bake is *worse than no bake*, and gets worse
every day the server stays up, while looking fine in any measurement that counts
only object reads.

`tandem workspace update-stale` at build time pays that walk once, into the
layer. It leaves an index that covers the server's history, and a cache holding
the operations and views the index was built from, so the boot merges one delta
into an index that is already there.

## Reading the delta-only fetch out of the server's log

The claim the recipe rests on is a number, so check it as one. The server logs
every read by name at `--log-level debug`, and counting them there rather than
in the client is the point: counting inside the client would be counting the
client's opinion of itself.

Count all three reads, not objects alone. An operation and a view each cost a
round trip like anything else, and the failure above is invisible in an object
count:

```bash
reads() { grep -a 'rpc request' server.log \
          | grep -c -e getObject -e getOperation -e getView; }

before=$(reads); docker run -d --name boot --network=host \
  -e TANDEM_TOKEN="$TANDEM_WORKSPACE_TOKEN" tandem-sandbox
sleep 6; docker rm -f boot; echo "boot cost $(( $(reads) - before )) reads"
```

One number on its own says nothing, so take it against two controls. The first
is the same image with the baked cache switched off (`-e
TANDEM_DISABLE_CACHE=1`) — that isolates the cache from the working copy. The
second is a container with no bake at all, which has to fetch the repository
before it can do anything:

```bash
docker run --rm --network=host -e TANDEM_TOKEN=… --entrypoint /bin/sh tandem-sandbox -c '
  rm -rf /work/.jj /work/* /var/cache/tandem/*
  cd /work && tandem clone "$TANDEM_SERVER" . --workspace "$TANDEM_WORKSPACE" --token "$TANDEM_TOKEN"
  tandem workspace update-stale'
```

Measured on a 24-file repository whose workspace had published before the bake,
with forty unrelated commands in another workspace behind it and two files
published after the bake (2026-08-22, all three boots reaching the same files):

| boot | objects | operations | views | total reads |
|---|---|---|---|---|
| from the bake | 6 | 10 | 4 | **20** |
| from the bake, cache switched off | 13 | 505 | 13 | **531** |
| no bake at all — clone and catch up, cold | 81 | 158 | 58 | **297** |

The middle row is the one to read twice. The image still has its working copy
and its index, and it still costs five hundred round trips without the cache
behind it, because the operation log is what a boot walks and the cache is what
answers for it. The last row grows with the repository; the first does not grow
with either the repository or the history.

The same measurements are made without Docker, on every `cargo test`, in
[`tests/integration/baked_image.rs`](../../tests/integration/baked_image.rs) —
what a container adds over a directory is a filesystem namespace, and the
image's warmth is a directory either way. The test that pins this claim runs the
whole recipe twice, against a server with four commands of history and against
one with forty, and requires the two boots to cost the *same*: a price that
tracks the history is the failure above, whatever its absolute size. That file
also measures the other boot shape, a warm `tandem clone` into a fresh
directory, where the object saving is starker still — the objects it reads are
exactly the delta: the two files published since the bake, the tree that names
them, and the commit that points at that tree.

## Things that will bite

- **Pin `TANDEM_CACHE_DIR`.** The default resolves through `$XDG_CACHE_HOME`
  and then `$HOME`. A container running as a different user than the build did
  would resolve a different directory, boot stone cold, and leave a full cache
  sitting unused one path over — with nothing in any log to say so. The
  template sets the variable explicitly for exactly this reason.
- **The workspace name is an identity, not a label.** Booting two containers
  from one image points two daemons at one workspace name; the second is
  refused the writer role, says so, and keeps running without publishing.
  Parallel agents get parallel workspaces — bake one image and pass
  `-e TANDEM_WORKSPACE=agent-b`, or bake one image per agent.
- **The bake goes stale, and a rebuild does not refresh it by itself.** Nothing
  invalidates a bake; a boot from a month-old image simply has a month-long
  delta to fetch. Rebuild on the same cadence you would rebuild any other
  dependency layer — and pass a new `BAKE_STAMP` when you do, or the layer
  cache will hand the whole month back to you as a successful build. See
  [Refreshing the bake](#refreshing-the-bake).
- **Build-time network access is required.** A build that cannot reach the
  server cannot bake anything, and there is no offline mode: the objects have
  to come from somewhere.
- **Bake with both commands or neither.** A bake that stops after `tandem
  clone` is warm in files and cold in history, and a boot from it costs more
  than a container with no image at all — 314 reads against 297 in the
  measurement above, and the gap widens with every operation the server
  publishes. See [Why the bake has two commands](#why-the-bake-has-two-commands).
  If you write your own recipe rather than using this template, this is the part
  to copy.
- **The first snapshot after a boot re-uploads files that have not changed.**
  A container's first publish wrote 30 objects in the measurement above, most of
  them file objects whose content was already on the server. Nothing is
  corrupted by it — the ids come out identical, so no new operation is created —
  but it is upload bandwidth and serial round trips on the boot path. The cause
  is that jj decides a file is unchanged from its recorded stat data, and an
  image layer unpacked into a container does not reproduce the stat data the
  bake recorded; jj then re-reads each file, and tandem's backend learns an
  object's id by writing it to the server. A client that hashed files locally
  before uploading would avoid it, at the cost of carrying a copy of git's
  hashing rules on this side of the wire — which is a decision `src/cache.rs`
  documents having deliberately declined. Left as it stands for now.
