# V0 Prototype Slices (completed, superseded)

Slices 1-7 were implemented as a **description-only prototype** using a custom
CLI (`tandem new/log/describe/diff`) instead of jj-lib Backend trait
integration. The Cap'n Proto transport, CAS head coordination, watchHeads
callbacks, and git round-trip plumbing all work correctly.

**What was proven:**
- Cap'n Proto RPC with twoparty VatNetwork works for store-shaped protocol
- CAS-based op-head coordination converges under 5-10 concurrent agents
- WatchHeads callback capabilities deliver sub-second notifications
- Server-side jj repo can push/fetch to bare git remotes

**What was deferred (now addressed in v1 slices):**
- jj-lib Backend/OpStore/OpHeadsStore trait integration (client is stock jj)
- Real commit/tree/file/symlink object storage (not description-only JSON)
- Bookmark management through tandem RPC
- CLI help text and error suggestions

See `docs/exec-plans/active/slice-roadmap.md` for the v1 rewrite plan.
