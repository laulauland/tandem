# Prepare an agent container

Use the [Dockerfile](Dockerfile) to build a Linux agent image with a local jj
workspace and its read cache. This is a client container, not a Tandem server.
For the server, use the [self-hosting guide](../self-hosting.md).

The image runs `tandem clone` and `tandem workspace update-stale` during the
build. The first command fetches working files; the second prepares the
operation history and index. At startup, the container explicitly updates the
workspace and starts its daemon. The daemon itself never moves working files
in response to another agent's edits.

## Build

Use Docker with BuildKit. Run these commands from the repository root. Set
`TANDEM_TOKEN` to a scoped credential for an existing repository and workspace,
as described in [workspace access](../self-hosting.md#connect-an-owner-and-an-agent).
Replace the server address and workspace below with that credential's scope.

```bash
umask 077
tandem_build_secret=$(mktemp)
trap 'rm -f "$tandem_build_secret"' EXIT
printf %s "$TANDEM_TOKEN" > "$tandem_build_secret"

docker build -f docs/images/Dockerfile \
  --secret id=tandem_token,src="$tandem_build_secret" \
  --build-arg TANDEM_SERVER=https://tandem.example/you/project \
  --build-arg TANDEM_WORKSPACE=agent-a \
  --build-arg BAKE_STAMP="$(date -u +%Y%m%dT%H%M%SZ)" \
  -t tandem-agent-a .
```

The build needs network access to the repository. If the server is on Linux
host loopback, use `--network=host` and its loopback address. A public HTTPS
server needs no host networking.

The build compiles Tandem for the image's Linux runtime. A macOS executable
cannot be copied into that image. The credential is a BuildKit secret, and the
clone's saved token is deleted in the same build step. Never pass a token as a
build argument. The image still contains repository files and history: keep it
private to people who may read that repository.

Pass a new `BAKE_STAMP` when refreshing repository contents. BuildKit does not
invalidate a cached layer when a remote repository or a secret changes. Inspect
the recorded stamp with:

```bash
docker run --rm --entrypoint /bin/cat tandem-agent-a /etc/tandem-bake
```

## Run

Provide a current scoped credential through the environment. Using `-e` with
only the variable name keeps its value out of the Docker command arguments:

```bash
export TANDEM_TOKEN
docker run --rm --name tandem-agent-a -e TANDEM_TOKEN tandem-agent-a
```

The daemon runs in the foreground and watches `/work`. Start the agent or
editor in that container after workspace setup has completed. Do not run two
containers from this image against the same workspace at the same time. Build
a separate image with a distinct workspace and scoped credential for each
concurrent agent.

Keep `TANDEM_CACHE_DIR` consistent between build and startup; the template sets
it to `/var/cache/tandem`. Rebuild periodically to reduce the history a new
container must fetch. Container replacement discards edits that were never
acknowledged, so stop editing and confirm publication before disposal.
