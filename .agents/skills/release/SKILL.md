---
name: release
description: Prepare, publish, and verify a Tandem version release through Jujutsu, GitHub Releases, Homebrew, and optionally crates.io. Use for release requests, version bumps, release preflights, or release failure recovery; do not use for ordinary feature pushes.
---

# Release Tandem

Use `jj` for every repository operation. Never substitute Git commands. The
tag-triggered automation in `.github/workflows/release.yml` owns binaries, the
GitHub release, changelog generation, and the Homebrew tap update.

## Establish the release

1. Read `.github/workflows/release.yml`, `Cargo.toml`, and the latest relevant
   entries in `docs/decision-ledger.md`. Inspect `jj status`, `jj log`,
   bookmarks, tags, and conflicts.
2. Require an explicit semantic version and clarify whether crates.io
   publication is included. Do not infer either from the latest tag.
3. Fetch the target remote with jj and confirm the candidate revision is the
   intended descendant of the remote default branch. Stop on bookmark, tag, or
   content conflicts.
4. Create a child revision described `chore(release): vX.Y.Z`. Change only the
   package version files that exist in this repository. Do not mix product
   changes into the release revision.
5. Run `scripts/preflight.sh X.Y.Z` from this skill. Resolve every failure; do
   not weaken or bypass the checks.

## Review before publication

Show the candidate revision ID, version diff, test result, intended remote,
whether crates.io is included, and the release workflow side effects. Create
the local tag with `jj tag set vX.Y.Z -r @`, then use dry runs for the main
bookmark and tag pushes.

Immediately before the first external mutation, obtain explicit user
authorization for the reviewed release. A request to prepare or validate a
release does not authorize pushing, publishing a crate, or changing the tap.

The installed jj must support pushing tags (`jj git push --help` includes
`--tag`). If it does not, stop and ask for a jj upgrade; never fall back to Git.

## Publish and verify

After authorization:

1. Point `main` at the release revision and push that bookmark with jj.
2. Push only `vX.Y.Z` with jj. The tag starts the release workflow.
3. If crates.io publication was explicitly included, run `cargo publish` once.
4. Observe the workflow to completion. Verify the release assets for every
   configured target, the generated changelog, and the Homebrew formula's
   version and checksums. If crates.io was included, verify its published
   version too.
5. Record exact URLs and outcomes. Leave the working copy in a fresh child
   revision if more work is expected.

On partial failure, report which irreversible steps landed and retry only the
failed idempotent step. Never move or delete a published tag, overwrite release
assets, republish a crate version, or edit the tap by hand without a separate
explicit recovery decision.
