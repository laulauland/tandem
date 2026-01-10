# Tandem

A native forge for [Jujutsu](https://github.com/martinvonz/jj) (jj) with real-time multiplayer collaboration. Tandem syncs jj changes, bookmarks, and user presence across multiple clients using CRDTs.

## Overview

Tandem provides a server-client architecture where:

- The **server** maintains the authoritative Y.Doc (Yrs CRDT document) for each repository
- **Clients** run a daemon that syncs local changes to the server over WebSocket
- All data merges automatically without conflicts using Yrs (the Rust port of Yjs)

This enables multiple developers to work on the same repository simultaneously with real-time visibility into who is editing what.

## Architecture

Tandem is organized into three crates:

### tandem-core

The shared library containing:

- **types.rs** - Core data types: `ChangeId`, `TreeHash`, `Change`, `ChangeRecord`, `Bookmark`, `PresenceInfo`
- **sync.rs** - `ForgeDoc` wrapping a Y.Doc with maps for changes, bookmarks, presence, and subdocuments for lazy content loading

### tandem-server

An Axum-based HTTP/WebSocket server:

- **sync.rs** - WebSocket endpoint at `/sync/:repo_id` handling bidirectional CRDT sync
- **docs.rs** - `DocManager` loads and persists Y.Doc state to `.yrs` files
- REST API for repositories, changes, bookmarks, presence, and content

### tandem-cli

The `jjf` command-line tool and background daemon:

- **main.rs** - CLI commands: `init`, `link`, `clone`, `daemon start`
- **daemon.rs** - Background sync daemon connecting to the forge
- **presence.rs** - Tracks who is editing which change
- **offline.rs** - Queues operations when disconnected for later replay
- **content.rs** - Lazy fetching of tree/blob content from the server

## Data Model

### Core Types

**ChangeId** - A stable 32-byte identifier for a change that persists across rebases. This is jj's native change ID.

**Change** - The unit of work containing:
- `id: ChangeId` - Stable identifier
- `tree: TreeHash` - Content-addressed 20-byte hash of the tree
- `parents: Vec<ChangeId>` - Parent changes
- `description: String` - Commit message
- `author: Identity` - Name and email
- `timestamp: DateTime<Utc>`

**ChangeRecord** - The CRDT-friendly wrapper stored in Y.Doc:
- `record_id: Uuid` - Unique key for the Y.Map entry
- `change_id: ChangeId` - The actual change ID
- `visible: bool` - False when abandoned/hidden
- All fields from `Change`

The distinction matters: a single `ChangeId` can have multiple `ChangeRecord` entries when a change diverges across clients before syncing. The CRDT merges them all, and the application layer decides how to present divergence.

**Bookmark** - A named pointer to a change (like a git branch):
- `name: String` - Bookmark name
- `target: ChangeId` - The change it points to
- `protected: bool` - Whether rules apply
- `rules: BookmarkRules` - CI/review requirements

**PresenceInfo** - Real-time editing status:
- `user_id: String` - Username
- `change_id: ChangeId` - Currently edited change
- `device: String` - Device name
- `timestamp: DateTime<Utc>` - Last update time (stale after 5 minutes)

### Y.Doc Structure

Each repository has **one main Y.Doc** for metadata, plus **separate subdocuments** (each its own Y.Doc) for content:

```
ForgeDoc
├── Main Doc (one per repo)
│   ├── Y.Map("changes")   → {record_id: JSON(ChangeRecord)}
│   ├── Y.Map("bookmarks") → {name: ChangeId_hex}
│   └── Y.Map("presence")  → {user_id: JSON(PresenceInfo)}
│
└── Subdocuments (HashMap<hash, Doc>)
    ├── "abc123" → Y.Doc { Y.Map("data") → {content: base64} }
    ├── "def456" → Y.Doc { Y.Map("data") → {content: base64} }
    └── ...
```

This design enables:
1. **Fast metadata sync** - The main doc is small and syncs quickly
2. **Lazy content loading** - Each content blob is a separate Y.Doc fetched on-demand
3. **Independent sync** - Subdocuments can sync independently, so you only fetch content you need

## Sync Protocol

### State Vector Exchange

Yrs uses state vectors to track what each peer has seen. The sync flow:

1. **Client connects** via WebSocket to `/sync/:repo_id`
2. **Client sends state vector** - A compact encoding of which updates it has
3. **Server computes diff** - Encodes only the updates the client lacks
4. **Server sends update** - Binary Yrs update packet
5. **Client applies update** - Merges into its local Y.Doc
6. **Bidirectional sync continues** - Either side can send updates

```
Client                          Server
   |                               |
   |-- Binary(state_vector) ------>|
   |                               | (compute diff)
   |<----- Binary(update) ---------|
   |                               |
   |-- Binary(local_update) ------>| (from local edit)
   |                               | (apply, save, broadcast)
   |                               |
```

### Server-Side Sync (tandem-server/src/sync.rs)

The server handles each WebSocket message:

1. **Try to apply as update** - If valid Yrs update, apply it
2. **On success** - Save to disk, broadcast to other clients (excluding sender)
3. **On failure** - Assume it's a state vector, compute and send diff

The `SyncManager` maintains broadcast channels per repository so updates propagate to all connected clients.

### Client-Side Sync (tandem-cli/src/daemon.rs)

The daemon:

1. Connects to the forge WebSocket
2. Sends its state vector to request initial sync
3. Receives and applies updates from the server
4. When local changes happen, sends updates to the server
5. On disconnect, enters offline mode and queues operations

## Conflict Resolution

### Why CRDTs Avoid Traditional Conflicts

Yrs implements a CRDT (Conflict-free Replicated Data Type). Every operation is designed to commute - the same set of operations applied in any order produces the same result.

For Tandem:

- **Changes** are keyed by unique `record_id` UUIDs. Two clients creating a record for the same change get two records, not a conflict.
- **Bookmarks** use last-writer-wins semantics on the Y.Map. Concurrent moves to different targets result in one winning.
- **Presence** uses last-writer-wins per user. Stale entries are filtered out by timestamp.

### Divergence Handling

When the same `ChangeId` gets edited on two disconnected clients:

1. Each client creates a new `ChangeRecord` with a unique `record_id`
2. On reconnect, both records sync to all clients
3. `get_change_records(change_id)` returns multiple records
4. The application must decide which is canonical (by timestamp, by user, or by user choice)

Hidden/abandoned changes are marked with `visible: false` rather than deleted, preserving history.

### Edge Cases

- **Bookmark races** - If Alice moves `main` to change A while Bob moves it to B, one wins. The CRDT resolves this deterministically but the "loser" may need to re-move the bookmark.
- **Offline queuing** - Operations made while disconnected are stored in `.jj/forge-queue.json` and replayed on reconnect.
- **Stale presence** - Presence entries older than 5 minutes are filtered out.

## Getting Started

### Starting the Server

```bash
cd crates/tandem-server
DATABASE_URL=sqlite:tandem.db DATA_DIR=./data cargo run
```

The server starts on `http://localhost:3000` with:
- REST API at `/api/*`
- WebSocket sync at `/sync/:repo_id`
- Health check at `/health`

### Linking a Repository

In an existing jj repository:

```bash
jjf link https://forge.example.com/org/myrepo --token <auth_token>
```

This:
1. Tests the connection to the forge
2. Creates `.jj/forge.toml` with the forge URL
3. Prints instructions to start the daemon

### Cloning from Forge

```bash
jjf clone https://forge.example.com/org/myrepo
```

This:
1. Creates the target directory
2. Runs `jj init`
3. Links to the forge
4. Pulls initial state via WebSocket sync
5. Saves the Y.Doc state to `.jj/forge-doc.bin`

### Running the Daemon

```bash
jjf daemon start
```

The daemon:
1. Loads forge config from `.jj/forge.toml`
2. Connects to the forge WebSocket
3. Syncs bidirectionally in real-time
4. Tracks presence (which change you're editing)
5. Warns when someone else is editing the same change
6. Queues operations when offline, replays on reconnect

### Presence Tracking

The daemon uses `whoami` to identify the current user and device. When you edit a change, it broadcasts your presence. Other users see warnings like:

```
Warning: This change is currently being edited by alice@laptop
```

Presence entries expire after 5 minutes of inactivity.

## Configuration

### .jj/forge.toml

Created by `jjf link`, stores the forge URL:

```toml
[forge]
url = "https://forge.example.com/org/myrepo"
```

### .jj/forge-queue.json

Stores queued operations when offline:

```json
{
  "operations": [
    {
      "type": "change_updated",
      "record": { ... },
      "timestamp": "2024-01-01T00:00:00Z"
    }
  ]
}
```

### .jj/forge-offline

Marker file indicating offline mode. Removed when connection is restored.

## API Reference

### REST Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/auth/login` | Authenticate and get token |
| GET | `/api/auth/me` | Get current user |
| GET | `/api/repos` | List repositories |
| POST | `/api/repos` | Create repository |
| GET | `/api/repos/:id` | Get repository details |
| GET | `/api/repos/:id/changes` | List changes |
| GET | `/api/repos/:id/changes/:cid` | Get specific change |
| GET | `/api/repos/:id/bookmarks` | List bookmarks |
| POST | `/api/repos/:id/bookmarks` | Move bookmark |
| GET | `/api/repos/:id/presence` | Get active presence |
| GET | `/api/repos/:id/content/:hash` | Fetch content by hash |

### WebSocket Endpoints

| Path | Description |
|------|-------------|
| `/sync/:repo_id` | Bidirectional Yrs sync |
| `/events/:repo_id` | Server-sent events for notifications |

## License

MIT OR Apache-2.0
