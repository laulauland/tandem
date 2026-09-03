# Tandem

Tandem gives agents on different machines real local working copies backed by
one shared [Jujutsu](https://jj-vcs.github.io/jj/latest/) repository. The
`tandem` binary embeds jj: ordinary commands remain ordinary jj commands, while
the object, operation, and head stores live behind an authenticated HTTP
server.

Use Tandem when collaborators need to see one another's in-progress commits
without sharing a filesystem. If every collaborator is on one machine, jj or
Git worktrees are simpler.

## Install

```bash
cargo install jj-tandem
```

A source build is just `cargo build --release -p jj-tandem --bin tandem`; no schema compiler or code
generation is required.

## Quick start

On the server, start from a colocated jj/Git repository. A durable remote
bucket is strongly recommended for anything that must survive the machine.

```bash
tandem up --repo /srv/project --listen 0.0.0.0:13013 \
  --bucket 's3://tandem-project?region=us-east-1'
```

`tandem up` prints the admin token when one was not supplied. Give each agent a
distinct workspace name and a workspace-scoped token; see
[operations](docs/operations.md#workspace-access) for token minting and network
safety.

```bash
tandem clone server:13013 ~/work/project \
  --workspace agent-a --token "$TANDEM_TOKEN"
cd ~/work/project
tandem daemon .
```

The daemon publishes file-change bursts after its debounce window. In another
terminal, use jj through the same binary:

```bash
tandem status
tandem describe -m 'feat: add authentication'
tandem log
tandem bookmark create agent-a/auth -r @
```

Run `tandem --help` and `tandem <command> --help` for the current command and
environment-variable reference. Tandem-owned help works without a server.

## Mental model

- Each agent edits files on local disk and owns one workspace writer lease.
- The server coordinates publishes and hosts a normal colocated jj/Git repo.
- A bucket-backed write-ahead log is the durable source of truth; the server
  repo can be rebuilt from it.
- Concurrent operation heads are preserved and reconciled through jj rather
  than overwritten.
- Git remotes and credentials stay on the server. Agents use jj; the integrator
  decides what reaches GitHub.

The exact boundaries are in [ARCHITECTURE.md](ARCHITECTURE.md), and the
durability contract is in [docs/reliability.md](docs/reliability.md).

## Operations and safety

Tandem authenticates every repository request, but does not terminate TLS.
Use a private network, tunnel, or TLS reverse proxy whenever the path is not
trusted. A local filesystem bucket is convenient for development but does not
survive loss of the server disk.

Deployment, backup, restore, observability, and incident procedures are in
[docs/operations.md](docs/operations.md). Image-baking instructions live in
[docs/images/README.md](docs/images/README.md).

## Development

```bash
python3 scripts/check_docs.py
cargo test --workspace
```

Read [AGENTS.md](AGENTS.md) before changing the repository. It routes to the
architecture, reliability, testing, operations, and decision records without
duplicating them. Benchmark methods and retained measurements are in
[docs/benchmarks/README.md](docs/benchmarks/README.md).

License: MIT.
