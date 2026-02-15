# Slice 5 — WatchHeads notifications

- **Date completed:** 2026-02-15
- **Test file(s):** `tests/slice5_watch_heads.rs`

## What was implemented

Real-time head change notifications via Cap'n Proto streaming:

1. **WatchHeads RPC capability**
   - Server implements `HeadWatcher` capability in schema
   - Clients subscribe via `watchHeads()` RPC call
   - Server notifies watchers on every successful `updateOpHeads`

2. **`tandem watch` command**
   - New command: `tandem watch --server <addr>`
   - Streams head notifications to stdout (JSON format)
   - Includes version, head IDs, timestamp

3. **Notification delivery**
   - Server tracks active watchers in memory
   - On head update, server calls `notify()` on all registered watchers
   - Watchers can reconnect after server restart (no persistent subscription state)

## Acceptance coverage

Integration test `watch_heads_real_time_notifications` validates:

- Agent A subscribes to watchHeads
- Agent B writes file and commits
- Agent A receives notification with new head
- Agent A can immediately `tandem file show` the new file — exact bytes match
- Multiple watchers all receive the same notification

## Architecture notes

WatchHeads enables real-time collaboration:
- Agents can poll-free monitor for new work
- Orchestrator can watch for agent progress
- Foundation for future live UI/dashboard
