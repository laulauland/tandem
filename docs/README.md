# Tandem documentation

Start with [the README](../README.md) to install Tandem and connect a workspace.

- [Self-hosting](self-hosting.md): run a native host with your own S3 storage,
  configure HTTPS, and give agents access.
- [Architecture](../ARCHITECTURE.md): components, state ownership, and how
  work moves between local workspaces and durable storage.
- [Reliability](reliability.md): publish ordering, concurrent writers, and
  recovery guarantees.
- [Operations](operations.md): monitor, back up, upgrade, and replace a host.
- [Agent images](images/README.md): build a container with a prepared workspace.
- [Testing](testing.md): run correctness tests and measure performance.

Use `tandem --help` for current commands and options. Contributors should also
read [AGENTS.md](../AGENTS.md).
