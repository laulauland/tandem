---
name: distributed-smoke
description: Run a disposable cross-machine Tandem acceptance and recovery drill with reproducible evidence and bounded cleanup. Use when provisioning remote smoke environments, qualifying a deployment, testing cold recovery, or capturing release evidence; do not use for local cargo tests.
---

# Distributed Tandem smoke test

This skill executes a procedure; architecture belongs in the repository docs.
Read `docs/operations.md` and use `tandem --help` for the exact installed CLI.

## Safety envelope

- Use a unique run ID and newly provisioned or explicitly disposable resources:
  one server, two clients, and one isolated bucket prefix. Record provider IDs
  as they are created.
- Never target production, a shared workspace name, or a bucket root. Do not
  delete resources selected by a glob, label query, or unresolved variable.
- Keep admin and workspace tokens out of command arguments, captured output,
  images, shell tracing, and evidence. Transfer them through provider secrets,
  protected files, or stdin, and remove those copies during cleanup.
- Provisioning and deletion are external mutations. Confirm they are within the
  user's request; otherwise stop before the first mutation.

## Provision and qualify

1. Choose an opaque run ID safe for filenames and labels. Provision three
   independently addressable machines and a bucket prefix containing that ID.
   Record exact IDs in the evidence directory.
2. Build with `cargo build --release -p jj-tandem --bin tandem`. Locate the
   executable from Cargo's `--message-format=json` compiler-artifact output
   (rather than assuming a package-local target directory). Record its version
   and checksum, and install the
   same bytes on every machine. Record OS and architecture. Reject incompatible
   runtimes instead of rebuilding different untracked binaries.
3. Run `scripts/preflight.sh <server-ssh> <client-a-ssh> <client-b-ssh>` from
   this skill after provisioning. It proves distinct reachable machines and
   required local tooling.
4. Start the server with the isolated bucket and a protected admin token. Put
   TLS, a provider HTTPS endpoint, a private network, or a tunnel in front of
   it. Capture status and startup replay counters without secrets.
5. Mint distinct short-lived credentials for distinct workspace names. Clone
   on both clients and start one daemon per workspace.

Wrap evidence-producing, secret-free commands with
`scripts/capture.sh <evidence-dir> <label> -- <command...>`. Do not wrap a
command whose arguments or output contain credentials.

## Verify collaboration

Use unique paths and random, recorded payloads. For every check, record the
payload digest, revision ID, observing machine, and returned digest.

1. Client A writes and publishes; client B observes the revision and reads
   byte-identical content without copying files out of A.
2. Both clients publish different files concurrently. Verify both revisions
   survive, heads settle, and a fresh read from each client returns both exact
   payloads.
3. Exercise a normal jj description, diff, operation-log, and namespaced
   bookmark workflow through `tandem`. Confirm the server can inspect the same
   revisions from its materialized repo.
4. Capture structured server status/log evidence for the publishes and any CAS
   retry. Absence of a retry is not failure; lost content or divergent change
   IDs is.

## Verify recovery

1. After an acknowledged publish, kill the server uncleanly. Restart it over
   the same repo and bucket, then verify the acknowledged bytes from the other
   machines.
2. Stop it again, move the disposable materialization aside without touching
   the bucket, and start against a new empty directory with the same bucket.
3. Verify the head set, operation-log traversal, workspace pointers, and every
   recorded payload by digest. Publish one new payload after cold recovery,
   restart once more, and read it from the opposite client.
4. If a throwaway upstream remote is in scope, use jj on the server to push a
   uniquely named bookmark and verify a clean clone. Never use a real project
   branch for smoke evidence.

Any byte mismatch, missing operation, unreplayable head, divergent change ID,
or acknowledged publish lost after restart fails the run. Preserve resources
and evidence on failure unless the user explicitly prefers cleanup.

## Evidence and cleanup

Create a manifest containing the run ID, UTC times, revision and binary
checksum, provider resource IDs, bucket prefix, commands' exit codes, payload
digests, head/revision IDs, recovery counters, and final verdict. Redact tokens
and signed URLs. Note skipped optional checks explicitly.

On success, stop daemons and server, remove secret files, and delete only the
exact machines and throwaway upstream refs recorded in the manifest. Delete
the exact bucket prefix only if it was declared ephemeral and the evidence is
already retained elsewhere; otherwise report it for manual disposition. Run
provider list/read commands afterward to prove each intended resource is gone.

Never turn a cleanup failure into a successful verdict. Report remaining IDs,
cost exposure, and the exact safe follow-up action.
