# Self-host Tandem

Run the native Rust host on your own machine with an independently operated
S3-compatible bucket. The same process serves named repositories, the website,
and the installer. Neither exe.dev nor Cloudflare is required.

Agents keep working files, builds, and tests on their own machines. The host's
jj caches are disposable; namespace ownership, the repository catalog, and
published history live in the bucket. Signing secrets and deployment
configuration need a separate protected backup. See the
[architecture diagram](../ARCHITECTURE.md#deployment-shape) and the
[durability contract](reliability.md).

## Use your own S3 service

Create a dedicated bucket before starting Tandem. Give the host credentials
that can read, write, and list its chosen prefix. Do not enable expiration of
Tandem objects. The endpoint must provide read-after-write consistency and
atomic conditional creation and replacement using ETags. A provider accepting
ordinary S3 uploads alone is insufficient: ignoring write preconditions can
lose concurrent history. AWS describes those preconditions in its
[conditional-write documentation](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html).

Tandem uses path-style S3 requests by default. Configure an endpoint and region
in the service environment; credentials never belong in the bucket URL:

```dotenv
TANDEM_BUCKET=s3://your-bucket/tandem
AWS_ENDPOINT=https://s3.example.net
AWS_REGION=us-east-1
AWS_ACCESS_KEY_ID=YOUR_ACCESS_KEY
AWS_SECRET_ACCESS_KEY=YOUR_SECRET_KEY
```

Use the endpoint and region supplied by your provider. For AWS S3, omit
`AWS_ENDPOINT` to use its regional endpoint. For R2, use your account's S3
endpoint and region `auto`, as described in the
[R2 S3 API reference](https://developers.cloudflare.com/r2/api/s3/api/).
If a provider requires virtual-hosted requests,
use `TANDEM_BUCKET=s3://your-bucket/tandem?virtual_hosted=true`. Local HTTP
endpoints are accepted; use HTTPS for storage reached over untrusted networks.

Before using a new provider, run the real storage contract against a disposable
prefix. This writes test objects; it is not a read-only check:

```bash
# AWS_* credentials and endpoint are already in the protected environment.
export TANDEM_TEST_S3_BUCKET='s3://your-test-bucket/tandem-contract'
cargo test -p jj-tandem-storage --features s3 s3_backend_honours_the_bucket_contract
```

The test exercises create-only writes, replacement with the current version,
rejection of stale versions, and reads. It does not establish a provider's
physical durability or disaster-recovery guarantees. Qualify a publish and
empty-cache recovery against that provider before admitting real work.

## Build and supervise the host

These examples use a Linux machine with Rust, Cargo, a native C toolchain,
`pkg-config`, OpenSSL development headers, and systemd. Start from a source
checkout; the public package release may predate the hosted-server code.

```bash
cargo build --locked --release -p jj-tandem --bin tandem
sudo install -m 0755 target/release/tandem /usr/local/bin/tandem
sudo useradd --system --user-group --home-dir /var/cache/tandem \
  --shell /usr/sbin/nologin tandem
sudo install -d -m 0755 /etc/tandem /opt/tandem/dist
sudo install -m 0600 deploy/self-hosted/host.env.example /etc/tandem/host.env
sudoedit /etc/tandem/host.env
```

In the protected file, replace every placeholder. Generate a long random admin
secret with your password manager and preserve it: it also signs owner and
workspace credentials. Set `TANDEM_PUBLIC_URL` to the public HTTPS origin,
without a path, and supply your bucket configuration above. The host renders
this origin into the website and installer.

On **GNU/Linux x86_64**, also install the matching client artifact so the host's
installer can distribute it:

```bash
sudo install -m 0755 target/release/tandem \
  /opt/tandem/dist/td-x86_64-unknown-linux-gnu
```

The installer also supports **Apple Silicon macOS**. To serve those clients,
build the same source revision on an Apple Silicon Mac with the Cargo command
above. Transfer that Mac binary to your Linux host and install it as
`/opt/tandem/dist/td-aarch64-apple-darwin`. The installer selects the matching
artifact automatically. A Linux executable cannot substitute for a Mac one.

Clients can also build the source themselves and use the owner bootstrap below
when their platform artifact is not available on the host.

Install the [service definition](../deploy/self-hosted/tandem.service):

```bash
sudo install -m 0644 deploy/self-hosted/tandem.service \
  /etc/systemd/system/tandem.service
sudo systemctl daemon-reload
sudo systemctl enable --now tandem
curl --fail http://127.0.0.1:13013/healthz
sudo journalctl -u tandem --since today
```

The service binds to loopback. Put your TLS reverse proxy on the same host and
forward the chosen domain to `127.0.0.1:13013`. Point DNS at that proxy. It must
pass authorization and conditional request headers, allow the bounded publish
body (up to 64 MiB), and support long-lived server-sent event responses without
buffering them. Tandem does not terminate TLS itself. A health response proves
the process is listening, not that repository recovery succeeded.

Hosted mode includes **public owner registration** through the installer.
Anyone able to reach it can obtain an owner credential and claim an available
namespace. For a private team host, restrict access to the whole origin through
your VPN or an access proxy that also works for CLI requests. There is no
built-in invitation-only registration switch.

The host service user writes only its cache and runtime directories. Keep
`/etc/tandem/host.env`, release artifacts, the service definition, and proxy
configuration backed up outside that machine. Never run two hosts against the
same catalog/prefix; initial failover is a fenced restart or replacement.

## Connect an owner and an agent

On GNU/Linux x86_64 or Apple Silicon macOS, use your own host's installer:

```bash
curl -fsSL https://tandem.example/install -o /tmp/tandem-install.sh
sh /tmp/tandem-install.sh
export PATH="$HOME/.local/bin:$PATH"
td clone https://tandem.example/your-name/project ./project --workspace agent-a
cd project
td daemon
```

Choose your own available namespace in place of `your-name`. The installer
stores a host-specific owner credential; the first clone claims the namespace
and creates the repository. The daemon stays in the foreground. Edit files
from another terminal; use your ordinary jj operations through `td`.

For a source-built client, obtain an owner credential without downloading a
binary. This command keeps the response and token out of terminal output and
process arguments. Preserve the resulting credential in protected storage;
requesting another token creates a different owner:

```bash
# Disable shell tracing before handling credentials.
set +x
export TANDEM_ORIGIN=https://tandem.example
export TANDEM_TOKEN="$(curl -fsS -X POST "$TANDEM_ORIGIN/install/token" |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')"
test -n "$TANDEM_TOKEN"
tandem clone "$TANDEM_ORIGIN/your-name/project" ./project --workspace agent-a
```

A second machine running the installer independently gets a different owner,
not access to your namespace. Instead, the existing owner mints a distinct
workspace credential for each agent. With that owner's token in
`TANDEM_TOKEN`, save a scoped credential to a protected file:

```bash
set +x
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$TANDEM_TOKEN" |
  curl --config - --fail --silent --show-error \
    "$TANDEM_ORIGIN/your-name/project/api/tokens" \
    -H 'content-type: application/json' \
    --data '{"workspaceId":"agent-b","ttlSeconds":3600}' |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])' \
    > agent-b.token
```

Deliver that file through your secret-management channel. On the second
machine, load it into `TANDEM_TOKEN` without printing it, then clone the
existing repository with `--workspace agent-b`. Scope and workspace name must
match. Workspace tokens expire; see
[workspace access](operations.md#workspace-access) for rotation and access
limitations. One active writer uses each workspace identity. The daemon never
updates another agent's working files automatically.

## Try a local S3 instance in Docker

This local-only example uses SeaweedFS and an ordinary source-built Tandem
process. It proves the S3 integration without needing a cloud account. It has
no storage authentication, binds only to loopback, and keeps its bucket inside
the container. **Removing the container deletes that history.** A container or
volume on the host's disk is not durable storage independent of that host.
For a durable SeaweedFS deployment, follow the provider's
[deployment documentation](https://github.com/seaweedfs/seaweedfs), then use
its protected S3 endpoint in the service configuration above.

```bash
docker run -d --name tandem-local-s3 \
  -p 127.0.0.1:18333:8333 chrislusf/seaweedfs:4.42 server -s3
# Once the S3 listener is ready, create the bucket:
curl --fail --silent --show-error -X PUT http://127.0.0.1:18333/tandem-local
```

In a terminal at the source root:

```bash
set +x
export TANDEM_ADMIN_TOKEN="$(openssl rand -hex 32)"
export TANDEM_BUCKET='s3://tandem-local/demo?endpoint=http://127.0.0.1:18333&anonymous=true'
export TANDEM_PUBLIC_URL=http://127.0.0.1:13013
target/release/tandem serve --hosted --listen 127.0.0.1:13013 \
  --repo /tmp/tandem-local-cache
```

Keep that shell's secret unchanged for the whole demo, including restarts.
Use a fresh cache path if `/tmp/tandem-local-cache` already belongs to another
server. In another terminal, follow the source-built owner bootstrap with
`TANDEM_ORIGIN=http://127.0.0.1:13013`, then clone and run the daemon. This demo
does not configure the binary download directory; use your built client.

To exercise recovery, stop the daemon after an acknowledged snapshot, record
its exact file bytes, stop the host, and restart it with the **same** bucket
and signing secret but a new empty cache directory. Reattach from a fresh
client directory with the original scoped credential and workspace identity.
Compare exact bytes and publish another change. Do not wipe the agent's local
working directory or the bucket to test host-cache recovery.

When finished, stop the host and remove only this demo container:

```bash
docker rm -f tandem-local-s3
```

The SeaweedFS 4.42 storage contract, hosted owner/scoped onboarding, normal
daemon publish, and fresh-client exact-byte reads after an empty host-cache
restart were exercised for this recipe. The systemd unit is an example for
your host; qualify your own TLS proxy and storage placement before use. Routine
backups, replacement, and incident procedures belong in
[operations.md](operations.md).
