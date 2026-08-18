# syntax=docker/dockerfile:1.7
#
# A sandbox image with the workspace already in it.
#
# The expensive part of starting an agent is not installing a binary, it is
# the first read of a repository: a cold client asks the server for every file,
# tree and commit it needs before it can do anything, and then walks the
# operation log to index what it found. That cost does not have to be paid at
# boot. `tandem clone` materializes a workspace *and* fills the client cache
# with everything it fetched; `tandem workspace update-stale` after it walks the
# operation log and leaves the index. Running both at image build time turns the
# first read into a layer. A container started from the image attaches to the
# same workspace name and asks the server only for what has been published since
# the image was baked — a price that is a function of the delta and not of how
# long the server has been up.
#
# There is no `tandem cache warm`, on purpose: the cache has no CLI surface,
# and baking an image is running the two ordinary commands with
# `TANDEM_CACHE_DIR` pointed somewhere the image keeps.
#
# Build (from the repository root, so the builder stage can see the sources):
#
#     printf %s "$TANDEM_ADMIN_TOKEN" > /tmp/tandem-token
#     docker build -f docs/images/Dockerfile \
#       --network=host \
#       --secret id=tandem_token,src=/tmp/tandem-token \
#       --build-arg TANDEM_SERVER=127.0.0.1:13013 \
#       --build-arg TANDEM_WORKSPACE=agent-a \
#       --build-arg BAKE_STAMP="$(date -u +%Y%m%dT%H%M%SZ)" \
#       -t tandem-sandbox .
#
# `BAKE_STAMP` is not optional on any build that is meant to refresh the bake:
# without it the bake layer is reused and the image keeps the bake it already
# had. See the ARG below.
#
# `--network=host` is what lets the bake step below reach the
# server: build steps do not join the user-defined networks `docker run
# --network` can. A server that is already reachable by name needs no flag.
#
# Boot:
#
#     docker run --rm -e TANDEM_TOKEN="$TANDEM_ADMIN_TOKEN" tandem-sandbox
#
# See ./README.md for the whole recipe, including how to read the delta-only
# fetch out of the server's log.

ARG RUST_IMAGE=rust:1-trixie
ARG RUNTIME_IMAGE=debian:trixie-slim

# ─── The binary ───────────────────────────────────────────────────────────────
#
# Built in the image rather than copied from the host: a baked image is meant
# to be shippable, and a host binary carries the host's libc with it. To reuse
# a binary you have already built for this exact runtime, override the stage:
# `--build-context builder=<dir>`, where <dir> holds the binary at `out/tandem`
# — the layout this stage leaves, and the one the COPY below reads.

FROM ${RUST_IMAGE} AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin tandem \
    && mkdir -p /out && cp target/release/tandem /out/tandem

# ─── The sandbox ──────────────────────────────────────────────────────────────

FROM ${RUNTIME_IMAGE} AS sandbox

# The workspace's server address and name are build arguments because they are
# what the bake is *of*. The token is not: see the secret mount below.
ARG TANDEM_SERVER
ARG TANDEM_WORKSPACE

COPY --from=builder /out/tandem /usr/local/bin/tandem

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Where the baked objects live. Named explicitly rather than left to
# `$XDG_CACHE_HOME` / `$HOME`, so that a container running as a different user
# than the build did cannot quietly resolve a different directory and boot
# cold with a full cache sitting unused one path over.
ENV TANDEM_CACHE_DIR=/var/cache/tandem

# The server and the workspace name are baked. A clone writes both next to the
# store as well, but the environment wins, which is what lets one image be
# pointed at a different server without being rebuilt.
ENV TANDEM_SERVER=${TANDEM_SERVER}
ENV TANDEM_WORKSPACE=${TANDEM_WORKSPACE}

# jj wants an identity before it will write an operation, and the daemon
# writes operations without being asked. Set it before the clone, not at boot,
# so a container that is handed nothing still works.
RUN mkdir -p /root/.config/jj \
    && printf 'user.name = "tandem sandbox"\nuser.email = "sandbox@invalid"\n[fsmonitor]\nbackend = "none"\n' \
       > /root/.config/jj/config.toml

WORKDIR /work

# What makes a rebuild a rebuild.
#
# BuildKit keys a `RUN` on the expanded text of the command and on the layers
# under it — not on anything the command goes and reads. The secret is
# deliberately outside that key, and so is the state of the server. With an
# unchanged binary and an unchanged command line, `docker build` therefore
# reuses the bake layer wholesale and reports a successful build: an image
# that is "rebuilt" every night and carries a month-old bake, with nothing in
# any log to say so. Every container from it then pays the whole month as a
# delta at boot, which is the one failure this file exists to prevent.
#
# So the stamp is part of the command, and a build that means to refresh the
# bake passes a new one: `--build-arg BAKE_STAMP="$(date -u +%Y%m%dT%H%M%SZ)"`.
# It is left at a constant default rather than made mandatory, because a build
# that deliberately wants the cached bake — iterating on the entrypoint, say —
# should be able to have it by saying nothing.
#
# The stamp is written into the image as well as into the cache key, so that
# `docker run … cat /etc/tandem-bake` answers "when was this baked, and against
# what" without anyone having to reason about layers.
ARG BAKE_STAMP=unstamped

# The bake.
#
# The token arrives as a BuildKit secret and not as an ARG or an ENV: a build
# argument is readable in `docker history` for as long as the image exists,
# and a bearer that speaks for a workspace is not something to ship inside the
# thing it lets you into. The mount exists for this one command and leaves no
# layer.
#
# `tandem clone` writes the token it was given next to the store, so that an
# ordinary `jj` command in the workspace works without one. That file would be
# a layer, so it is removed in the same step: the image carries a workspace and
# its objects, and no authority at all. A container is given a fresh token at
# boot with `-e TANDEM_TOKEN=…`, which is where a short-lived bearer belongs.
# The second command is not decoration, and leaving it out is the difference
# between an image that boots on the delta and one that boots on the server's
# whole history.
#
# A clone reads one operation and one view — its own. The operation log is
# therefore a cache miss from end to end after a clone, and worse: attaching to
# a workspace deliberately builds its operation as a *sibling* of everything the
# server has done, for the token-scope reason in `workspace_init.rs`. The first
# command that loads the repository at head merges the two sides, and jj builds
# its commit index from the newest ancestor operation that already has one — so
# it walks every operation on the side the clone never saw, reading each
# operation, each view, and the commits their heads name, one round trip at a
# time. On a server with a day of history behind it that is hundreds of serial
# requests: measured at 254 reads against 33 for a cold clone with no image at
# all, which is the cold start this file exists to remove, inverted.
#
# `tandem workspace update-stale` here is the boot's own first step, run at
# build time. It pays that walk once, into the layer: it leaves an index that
# covers the server's history and a cache holding the operations and views the
# index was built from. The boot then merges one delta into an index that is
# already there. Measured at 20 reads whether the server has 4 commands of
# history behind it or 80 — see
# `tests/integration/baked_image.rs::what_a_boot_costs_is_the_delta_and_not_the_server_s_history`.
#
# It is a separate `&&` and not a separate `RUN` because it needs the token,
# and the token is mounted for this command only.
RUN --mount=type=secret,id=tandem_token \
    printf 'bake_stamp=%s\nserver=%s\nworkspace=%s\n' \
      "${BAKE_STAMP}" "${TANDEM_SERVER}" "${TANDEM_WORKSPACE}" > /etc/tandem-bake \
    && export TANDEM_TOKEN="$(cat /run/secrets/tandem_token)" \
    && tandem clone "${TANDEM_SERVER}" . --workspace "${TANDEM_WORKSPACE}" \
    && tandem workspace update-stale \
    && find /work/.jj -type f -name token -delete

# The boot.
#
# Two steps, and the first one is the interesting half. The image holds the
# repository as it stood when the image was built, and the workspace name may
# have been published to since — from the machine that baked it, or from
# whatever ran before this container. `update-stale` is what brings /work to
# the last published snapshot, and with the baked cache behind it, everything
# it needs except that delta is already on disk.
#
# The daemon will not do this for you, deliberately: moving files under
# whoever is editing them is a decision, not a reflex. A container that has
# just booted is the one moment when nobody is editing them yet, which is why
# the entrypoint may do it and the daemon may not. It is `|| true` because a
# workspace that is already current says so and exits non-zero on some paths,
# and "already current" is not a reason to refuse to start.
#
# Then the daemon, for good: it subscribes, and publishes every burst of file
# changes in /work as one operation. Whatever edits /work — an agent, a build,
# a person — never learns that tandem is there.
#
# It says what it booted from first. A stale bake is invisible from inside the
# container — a delta of two files and a delta of two thousand look the same
# from here, they just take different amounts of time — so the one fact that
# makes the difference legible, the stamp of the build that baked it, goes in
# the log before anything else happens.
RUN printf '%s\n' \
    '#!/bin/sh' \
    'set -e' \
    'echo "tandem sandbox booting; $(tr "\\n" " " < /etc/tandem-bake)"' \
    'cd /work' \
    'tandem workspace update-stale || true' \
    'exec tandem daemon /work "$@"' \
    > /usr/local/bin/tandem-sandbox-boot \
    && chmod +x /usr/local/bin/tandem-sandbox-boot

ENTRYPOINT ["/usr/local/bin/tandem-sandbox-boot"]
