# syntax=docker/dockerfile:1.7
# Linux client image with a prepared workspace. See README.md for build and run.
# BuildKit supplies a scoped token; the image contains repository data but no token.

ARG RUST_IMAGE=rust:1-trixie
ARG RUNTIME_IMAGE=debian:trixie-slim

FROM ${RUST_IMAGE} AS builder
WORKDIR /src
COPY Cargo.toml README.md ./
COPY crates ./crates
COPY testing ./testing
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p jj-tandem --bin tandem \
    && mkdir -p /out && cp target/release/tandem /out/tandem

FROM ${RUNTIME_IMAGE} AS sandbox

ARG TANDEM_SERVER
ARG TANDEM_WORKSPACE

COPY --from=builder /out/tandem /usr/local/bin/tandem

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ENV TANDEM_CACHE_DIR=/var/cache/tandem

ENV TANDEM_SERVER=${TANDEM_SERVER}
ENV TANDEM_WORKSPACE=${TANDEM_WORKSPACE}

RUN mkdir -p /root/.config/jj \
    && printf 'user.name = "tandem sandbox"\nuser.email = "sandbox@invalid"\n[fsmonitor]\nbackend = "none"\n' \
       > /root/.config/jj/config.toml

WORKDIR /work

ARG BAKE_STAMP=unstamped

RUN --mount=type=secret,id=tandem_token \
    printf 'bake_stamp=%s\nserver=%s\nworkspace=%s\n' \
      "${BAKE_STAMP}" "${TANDEM_SERVER}" "${TANDEM_WORKSPACE}" > /etc/tandem-bake \
    && export TANDEM_TOKEN="$(cat /run/secrets/tandem_token)" \
    && tandem clone "${TANDEM_SERVER}" . --workspace "${TANDEM_WORKSPACE}" \
    && tandem workspace update-stale \
    && find /work/.jj -type f -name token -delete

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
