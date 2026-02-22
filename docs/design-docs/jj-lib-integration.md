# jj-lib Integration (Completed)

> **Status:** Implementation complete as of 2026-02-15
> **Implementation:** `src/backend.rs`, `src/op_store.rs`, `src/op_heads_store.rs`
> **Research date:** 2026-02-15 (kept for reference)

---

## Implementation Summary

Tandem implements three jj-lib traits to provide transparent remote storage:

| Trait | Implementation | File |
|-------|---------------|------|
| `Backend` | `TandemBackend` | `src/backend.rs` |
| `OpStore` | `TandemOpStore` | `src/op_store.rs` |
| `OpHeadsStore` | `TandemOpHeadsStore` | `src/op_heads_store.rs` |

All trait methods route to Cap'n Proto RPC calls defined in `schema/tandem.capnp`.
The server uses jj's Git backend internally, so objects are real git-compatible blobs.

Stock jj commands (`log`, `new`, `diff`, `file show`, `bookmark create`, etc.) all work
transparently — the agent never knows the store is remote.

See `tests/slice1_single_agent_round_trip.rs` for integration test coverage.

## Head authority model (Option C)

Server head authority is jj-lib op-heads state.

- `src/server.rs` updates/reads op heads via jj-lib `OpHeadsStore` APIs.
- `.jj/repo/tandem/heads.json` is metadata sidecar only (`version`, `workspace_heads`).
- No manual filesystem sync path for `.jj/repo/op_heads/heads/*`.

See `tests/slice15_head_authority_jj_lib.rs` for integration coverage of
jj-lib vs RPC head consistency.

## Integration workspace mode (flagged)

Optional runtime mode (`--enable-integration-workspace`, or
`TANDEM_ENABLE_INTEGRATION_WORKSPACE=1`) adds a server-side recompute worker:

- Trigger: successful `updateOpHeads` RPC
- Input: latest workspace head map (`workspace_heads`) resolved to workspace commit IDs
- Action: compute merged tree, create integration commit, move bookmark `integration`
- Output metadata: `.jj/repo/tandem/integration.json`
  (`enabled`, `last_input_fingerprint`, `last_integration_commit`, `last_status`,
  `last_error`, `updated_at`)

Conflict merges are intentionally visible: if parents conflict, `integration`
points to a conflicted commit.

---

## Original Research Notes

The following sections document the trait signatures and registration mechanisms
that informed the implementation. Kept for reference.

---

## Table of Contents

1. [Backend Trait](#1-backend-trait)
2. [OpStore Trait](#2-opstore-trait)
3. [OpHeadsStore Trait](#3-opheadsstore-trait)
4. [Custom Backend Registration](#4-custom-backend-registration)
5. [Object Serialization Formats](#5-object-serialization-formats)
6. [Workspace Model](#6-workspace-model)
7. [On-Disk Layout (`.jj/store/`)](#7-on-disk-layout)
8. [Alternative: Background Sync Process](#8-alternative-background-sync-process)
9. [jj-cli Structure & Custom Binary](#9-jj-cli-structure--custom-binary)
10. [Recommended Approach for Tandem](#10-recommended-approach-for-tandem)
11. [Implementation Sketch](#11-implementation-sketch)

---

## 1. Backend Trait

**File:** `lib/src/backend.rs`

The `Backend` trait is the core content-addressable store for commits, trees, files, and symlinks.
All methods are **required** (no defaults).

```rust
#[async_trait]
pub trait Backend: Any + Send + Sync + Debug {
    /// Unique name written to `.jj/repo/store/type` on repo creation.
    fn name(&self) -> &str;

    /// Length of commit IDs in bytes (e.g. 64 for BLAKE2b-512).
    fn commit_id_length(&self) -> usize;

    /// Length of change IDs in bytes (e.g. 16).
    fn change_id_length(&self) -> usize;

    fn root_commit_id(&self) -> &CommitId;
    fn root_change_id(&self) -> &ChangeId;
    fn empty_tree_id(&self) -> &TreeId;

    /// Concurrency hint. Local backend: 1. Cloud backend: 100.
    fn concurrency(&self) -> usize;

    // --- File operations ---
    async fn read_file(
        &self, path: &RepoPath, id: &FileId,
    ) -> BackendResult<Pin<Box<dyn AsyncRead + Send>>>;

    async fn write_file(
        &self, path: &RepoPath, contents: &mut (dyn AsyncRead + Send + Unpin),
    ) -> BackendResult<FileId>;

    // --- Symlink operations ---
    async fn read_symlink(&self, path: &RepoPath, id: &SymlinkId) -> BackendResult<String>;
    async fn write_symlink(&self, path: &RepoPath, target: &str) -> BackendResult<SymlinkId>;

    // --- Copy tracking (can return Unsupported) ---
    async fn read_copy(&self, id: &CopyId) -> BackendResult<CopyHistory>;
    async fn write_copy(&self, copy: &CopyHistory) -> BackendResult<CopyId>;
    async fn get_related_copies(&self, copy_id: &CopyId) -> BackendResult<Vec<CopyHistory>>;

    // --- Tree operations ---
    async fn read_tree(&self, path: &RepoPath, id: &TreeId) -> BackendResult<Tree>;
    async fn write_tree(&self, path: &RepoPath, contents: &Tree) -> BackendResult<TreeId>;

    // --- Commit operations ---
    async fn read_commit(&self, id: &CommitId) -> BackendResult<Commit>;

    /// Write commit. May modify contents (e.g. authenticated committer).
    /// Returns (id, possibly-modified commit).
    async fn write_commit(
        &self,
        contents: Commit,
        sign_with: Option<&mut SigningFn>,
    ) -> BackendResult<(CommitId, Commit)>;

    // --- Copy records (streaming) ---
    fn get_copy_records(
        &self,
        paths: Option<&[RepoPathBuf]>,
        root: &CommitId,
        head: &CommitId,
    ) -> BackendResult<BoxStream<'_, BackendResult<CopyRecord>>>;

    // --- Garbage collection ---
    fn gc(&self, index: &dyn Index, keep_newer: SystemTime) -> BackendResult<()>;
}
```

### Key Data Types

```rust
pub struct Commit {
    pub parents: Vec<CommitId>,
    pub predecessors: Vec<CommitId>,      // deprecated, being removed
    pub root_tree: Merge<TreeId>,          // conflict-aware merged tree
    pub conflict_labels: Merge<String>,    // labels for conflict terms
    pub change_id: ChangeId,
    pub description: String,
    pub author: Signature,
    pub committer: Signature,
    pub secure_sig: Option<SecureSig>,
}

pub struct Signature {
    pub name: String,
    pub email: String,
    pub timestamp: Timestamp,
}

pub struct Timestamp {
    pub timestamp: MillisSinceEpoch(i64),
    pub tz_offset: i32,  // minutes
}

// Tree: sorted Vec of (name, TreeValue)
pub struct Tree {
    entries: Vec<(RepoPathComponentBuf, TreeValue)>,
}

pub enum TreeValue {
    File { id: FileId, executable: bool, copy_id: CopyId },
    Symlink(SymlinkId),
    Tree(TreeId),
    GitSubmodule(CommitId),
}
```

### ID Types (all `Vec<u8>` wrappers)

| Type | Typical Length | Hash |
|------|---------------|------|
| `CommitId` | 64 bytes | BLAKE2b-512 (Simple) or SHA-1 (Git) |
| `ChangeId` | 16 bytes | Random |
| `TreeId` | 64 bytes | BLAKE2b-512 / SHA-1 |
| `FileId` | 64 bytes | BLAKE2b-512 / SHA-1 |
| `SymlinkId` | 64 bytes | BLAKE2b-512 / SHA-1 |
| `CopyId` | varies | BLAKE2b-512 |

---

## 2. OpStore Trait

**File:** `lib/src/op_store.rs`

The `OpStore` manages operations (transactions) and views (repository state snapshots).
All methods are **required**.

```rust
#[async_trait]
pub trait OpStore: Any + Send + Sync + Debug {
    fn name(&self) -> &str;

    fn root_operation_id(&self) -> &OperationId;

    async fn read_view(&self, id: &ViewId) -> OpStoreResult<View>;
    async fn write_view(&self, contents: &View) -> OpStoreResult<ViewId>;

    async fn read_operation(&self, id: &OperationId) -> OpStoreResult<Operation>;
    async fn write_operation(&self, contents: &Operation) -> OpStoreResult<OperationId>;

    /// Resolve operation ID by hex prefix.
    async fn resolve_operation_id_prefix(
        &self,
        prefix: &HexPrefix,
    ) -> OpStoreResult<PrefixResolution<OperationId>>;

    /// Garbage collect unreachable operations/views.
    fn gc(&self, head_ids: &[OperationId], keep_newer: SystemTime) -> OpStoreResult<()>;
}
```

### Key Data Types

```rust
pub struct Operation {
    pub view_id: ViewId,
    pub parents: Vec<OperationId>,
    pub metadata: OperationMetadata,
    pub commit_predecessors: Option<BTreeMap<CommitId, Vec<CommitId>>>,
}

pub struct OperationMetadata {
    pub time: TimestampRange,
    pub description: String,
    pub hostname: String,
    pub username: String,
    pub is_snapshot: bool,
    pub tags: HashMap<String, String>,
}

pub struct View {
    pub head_ids: HashSet<CommitId>,
    pub local_bookmarks: BTreeMap<RefNameBuf, RefTarget>,
    pub local_tags: BTreeMap<RefNameBuf, RefTarget>,
    pub remote_views: BTreeMap<RemoteNameBuf, RemoteView>,
    pub git_refs: BTreeMap<GitRefNameBuf, RefTarget>,
    pub git_head: RefTarget,
    pub wc_commit_ids: BTreeMap<WorkspaceNameBuf, CommitId>,
}
```

---

## 3. OpHeadsStore Trait

**File:** `lib/src/op_heads_store.rs`

Manages the set of current operation heads (typically one, multiple during concurrent ops).
All methods are **required**.

```rust
#[async_trait]
pub trait OpHeadsStore: Any + Send + Sync + Debug {
    fn name(&self) -> &str;

    /// Replace old_ids with new_id atomically.
    /// old_ids must not contain new_id.
    async fn update_op_heads(
        &self,
        old_ids: &[OperationId],
        new_id: &OperationId,
    ) -> Result<(), OpHeadsStoreError>;

    async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError>;

    /// Optional advisory lock to prevent concurrent divergent-op resolution.
    async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError>;
}

pub trait OpHeadsStoreLock {}  // marker trait, holds lock on drop
```

---

## 4. Custom Backend Registration

### 4.1 The Factory Pattern

**File:** `lib/src/repo.rs`

jj uses a `StoreFactories` registry that maps type name strings to factory closures:

```rust
pub struct StoreFactories {
    backend_factories: HashMap<String, BackendFactory>,
    op_store_factories: HashMap<String, OpStoreFactory>,
    op_heads_store_factories: HashMap<String, OpHeadsStoreFactory>,
    index_store_factories: HashMap<String, IndexStoreFactory>,
    submodule_store_factories: HashMap<String, SubmoduleStoreFactory>,
}

// Factory type aliases:
type BackendFactory =
    Box<dyn Fn(&UserSettings, &Path) -> Result<Box<dyn Backend>, BackendLoadError>>;
type OpStoreFactory = Box<
    dyn Fn(&UserSettings, &Path, RootOperationData) -> Result<Box<dyn OpStore>, BackendLoadError>,
>;
type OpHeadsStoreFactory =
    Box<dyn Fn(&UserSettings, &Path) -> Result<Box<dyn OpHeadsStore>, BackendLoadError>>;
```

### 4.2 How Type Dispatch Works

When jj loads a repo, it reads the **type file** in each store directory:

| File | Example Content | Purpose |
|------|-----------------|---------|
| `.jj/repo/store/type` | `git` or `Simple` | Backend type |
| `.jj/repo/op_store/type` | `simple_op_store` | OpStore type |
| `.jj/repo/op_heads/type` | `simple_op_heads_store` | OpHeadsStore type |
| `.jj/repo/index/type` | `default` | IndexStore type |

`StoreFactories::load_backend()` reads `.jj/repo/store/type`, looks up the factory by name,
and calls it with `(settings, store_path)`.

### 4.3 Registration via CliRunner

**File:** `cli/src/cli_util.rs`

The `CliRunner` has an `add_store_factories()` method:

```rust
impl<'a> CliRunner<'a> {
    pub fn add_store_factories(mut self, store_factories: StoreFactories) -> Self {
        self.store_factories.merge(store_factories);
        self
    }
    // ...
}
```

### 4.4 Default Factories

`StoreFactories::default()` registers:
- **Backends:** `Simple`, `git` (if `git` feature), `secret` (if `testing` feature)
- **OpStores:** `simple_op_store`
- **OpHeadsStores:** `simple_op_heads_store`
- **IndexStores:** `default`
- **SubmoduleStores:** `default`

### 4.5 Can You Register Without Forking?

**No.** The stock `jj` binary has a hardcoded set of factories. To add a custom backend,
you must build a **custom binary** that calls `CliRunner::init().add_store_factories(...)`.

This is **by design** — jj's extension model is "build your own binary with jj-cli as a library."

The jj `main.rs` is literally:
```rust
fn main() -> std::process::ExitCode {
    CliRunner::init().version(env!("JJ_VERSION")).run().into()
}
```

### 4.6 Initializer vs Factory

There are two function signature types:
- **Initializer** (`BackendInitializer`): Creates a *new* store on `jj init`
  ```rust
  type BackendInitializer<'a> =
      dyn Fn(&UserSettings, &Path) -> Result<Box<dyn Backend>, BackendInitError> + 'a;
  ```
- **Factory** (`BackendFactory`): Loads an *existing* store when opening a repo
  ```rust
  type BackendFactory =
      Box<dyn Fn(&UserSettings, &Path) -> Result<Box<dyn Backend>, BackendLoadError>>;
  ```

Both are needed: the initializer for `jj init`, the factory for `jj log/diff/etc`.

---

## 5. Object Serialization Formats

### 5.1 Protobuf (prost)

jj uses **Protocol Buffers** (via `prost`) for serializing commits, trees, operations, and views.

#### `simple_store.proto` — Backend objects

```protobuf
syntax = "proto3";
package simple_store;

message TreeValue {
  message File {
    bytes id = 1;
    bool executable = 2;
    bytes copy_id = 3;
  }
  oneof value {
    File file = 2;
    bytes symlink_id = 3;
    bytes tree_id = 4;
  }
}

message Tree {
  message Entry {
    string name = 1;
    TreeValue value = 2;
  }
  repeated Entry entries = 1;
}

message Commit {
  repeated bytes parents = 1;
  repeated bytes predecessors = 2;
  repeated bytes root_tree = 3;       // Merge terms (alternating +/-)
  repeated string conflict_labels = 10;
  bytes change_id = 4;
  string description = 5;

  message Timestamp {
    int64 millis_since_epoch = 1;
    int32 tz_offset = 2;
  }
  message Signature {
    string name = 1;
    string email = 2;
    Timestamp timestamp = 3;
  }
  Signature author = 6;
  Signature committer = 7;
  optional bytes secure_sig = 9;
}
```

#### Op-store objects

Operations and views have a similar proto schema in `simple_op_store.proto`.
The key structures are `Operation` (view_id, parents, metadata, commit_predecessors)
and `View` (head_ids, bookmarks, tags, remote_views, git_refs, git_head, wc_commit_ids).

### 5.2 Files

Files are stored as **raw bytes** — no wrapper, no protobuf. The `FileId` is the
BLAKE2b-512 hash of the raw content.

### 5.3 Symlinks

Symlinks are stored as **UTF-8 strings** (the target path). The `SymlinkId` is the
BLAKE2b-512 hash of the target string bytes.

### 5.4 Git Backend

The Git backend uses Git's native object format (SHA-1 hashes, git blob/tree/commit objects).
It doesn't use the protobuf schema above — it has its own `git_backend.rs` that maps to/from
libgit2 objects. This means:
- Git backend: 20-byte SHA-1 IDs
- Simple backend: 64-byte BLAKE2b-512 IDs

**For tandem:** We proxy the server's backend, so we match whatever ID length the server uses.

---

## 6. Workspace Model

### 6.1 How Workspaces Work

A **workspace** is a working copy + pointer to a shared repo:

```
workspace_root/
├── .jj/
│   ├── repo/           → actual repo (or symlink to shared repo)
│   │   ├── store/      → Backend (commits, trees, files)
│   │   ├── op_store/   → OpStore (operations, views)
│   │   ├── op_heads/   → OpHeadsStore (current op heads)
│   │   └── index/      → IndexStore (commit graph index)
│   └── working_copy/   → WorkingCopy state
└── <working copy files>
```

For additional workspaces (`jj workspace add`), `.jj/repo` is a **file** containing
a relative path to the primary workspace's repo directory.

### 6.2 Backend Workspace Awareness

The backend **does not** need workspace awareness. Workspaces are managed at the
`View` level — each workspace has an entry in `view.wc_commit_ids`:

```rust
pub struct View {
    pub wc_commit_ids: BTreeMap<WorkspaceNameBuf, CommitId>,
    // ...
}
```

The working copy is managed locally by `LocalWorkingCopy` and is independent of
the backend.

### 6.3 For Tandem

Each agent machine has:
- A local working copy (managed by stock `jj`)
- `.jj/repo` pointing to a local directory with `store/type = "tandem"`
- The tandem backend proxies all reads/writes to the remote server
- The `View.wc_commit_ids` map tracks which workspace is on which commit

---

## 7. On-Disk Layout

### `.jj/repo/store/` (Backend)

For the **Git backend** (most common):
```
store/
├── type           → "git"
├── git_target     → relative path to .git directory
└── extra/         → jj-specific data (change IDs, etc.)
    └── <hex_id>   → extra metadata per commit
```

For the **Simple backend**:
```
store/
├── type       → "Simple"
├── commits/   → protobuf-encoded Commit objects, keyed by hex ID
├── trees/     → protobuf-encoded Tree objects
├── files/     → raw file content
├── symlinks/  → raw symlink targets
└── conflicts/ → (deprecated)
```

### `.jj/repo/op_store/` (OpStore)
```
op_store/
├── type         → "simple_op_store"
├── operations/  → protobuf-encoded Operation objects, keyed by hex ID
└── views/       → protobuf-encoded View objects, keyed by hex ID
```

### `.jj/repo/op_heads/` (OpHeadsStore)
```
op_heads/
├── type   → "simple_op_heads_store"
└── heads/ → empty files named by hex operation ID
```

---

## 8. Alternative: Background Sync Process

### 8.1 The Idea

Instead of implementing `Backend`/`OpStore`/`OpHeadsStore`, tandem could be a
background process that watches `.jj/store/` (or `.git/`) and replicates objects
to a remote server via rsync/rclone/custom protocol.

### 8.2 What Would Be Synced

| Directory | What | Size |
|-----------|------|------|
| `store/` (git) | Git packfiles and loose objects | All project content |
| `op_store/operations/` | Operation blobs | Small per-op |
| `op_store/views/` | View blobs | Medium (grows with bookmarks) |
| `op_heads/heads/` | Head pointer files | Tiny |
| `index/` | Commit graph index | Large, machine-specific |

### 8.3 Comparison

| Criterion | Custom Backend | Background Sync |
|-----------|---------------|-----------------|
| **Latency** | Sub-ms for cached, network RTT for miss | Eventual (seconds to minutes) |
| **Consistency** | Strong (read-after-write) | Eventual (race conditions) |
| **Concurrent writes** | Handled by OpHeadsStore CAS | **Dangerous** — can corrupt |
| **Complexity** | High (implement 3 traits) | Low (file watching + rsync) |
| **Stock jj compat** | Needs custom binary | Works with stock jj |
| **Offline support** | Needs explicit handling | Natural (local-first) |
| **Index** | Server-side or skip | Must rebuild per-machine |
| **Git interop** | Server handles git ops | Both sides need git |

### 8.4 Risks of Background Sync

1. **Concurrent writes cause corruption.** Two agents writing to `op_heads/heads/`
   simultaneously (even via NFS/rsync) can create dangling operation heads that
   reference objects not yet synced.

2. **Partial sync is invisible.** If agent A writes a commit + tree + file,
   but only the commit syncs before agent B reads, agent B gets `ObjectNotFound`.

3. **Op-head race.** If agent A advances op-heads and agent B syncs before the
   new operation's view is synced, agent B sees an empty/corrupt view.

4. **Index rebuild storms.** The commit graph index is machine-specific and must
   be rebuilt after every sync, which is expensive for large repos.

5. **No real-time notifications.** Agents can't know when new work is available
   without polling.

### 8.5 Verdict

Background sync is **unsuitable for tandem's design goals** (real-time multi-agent
collaboration with strong consistency). It could work as a simpler "eventual sync"
tool but not for the "shared filesystem" experience tandem targets.

---

## 9. jj-cli Structure & Custom Binary

### 9.1 How the jj Binary Is Built

The `jj` binary is in `cli/src/main.rs`:

```rust
use jj_cli::cli_util::CliRunner;

fn main() -> std::process::ExitCode {
    CliRunner::init().version(env!("JJ_VERSION")).run().into()
}
```

`CliRunner::init()` sets up:
- `StoreFactories::default()` — registers built-in backends
- `default_working_copy_factories()` — registers `LocalWorkingCopy`
- `DefaultWorkspaceLoaderFactory` — reads `.jj/repo/` from filesystem
- `crate::commands::default_app()` — clap command definitions
- `crate::commands::run_command` — command dispatch

### 9.2 Dependencies

```toml
[dependencies]
jj-lib = { workspace = true }   # core library
# ... many CLI deps (clap, crossterm, ratatui, etc.)
```

The `jj-lib` crate is the key dependency. It provides all traits, the `StoreFactories`
registry, and the `SimpleBackend`/`GitBackend` implementations.

### 9.3 Where Backend/OpStore/OpHeadsStore Are Created

1. **On `jj init`:** `ReadonlyRepo::init()` calls the `BackendInitializer`,
   `OpStoreInitializer`, and `OpHeadsStoreInitializer` closures. It writes the
   type name to `store/type`, `op_store/type`, `op_heads/type`.

2. **On every other command:** `RepoLoader::init_from_file_system()` reads the
   type files, looks up factories in `StoreFactories`, and calls them to load
   the stores.

### 9.4 Building a `jj-tandem` Binary

**This is the recommended approach.** Create a custom binary that extends jj:

```rust
// jj-tandem/src/main.rs
use jj_cli::cli_util::CliRunner;
use jj_lib::repo::StoreFactories;

fn main() -> std::process::ExitCode {
    let mut factories = StoreFactories::empty();

    factories.add_backend(
        "tandem",
        Box::new(|settings, store_path| {
            Ok(Box::new(tandem::TandemBackend::load(settings, store_path)?))
        }),
    );
    factories.add_op_store(
        "tandem_op_store",
        Box::new(|settings, store_path, root_data| {
            Ok(Box::new(tandem::TandemOpStore::load(settings, store_path, root_data)?))
        }),
    );
    factories.add_op_heads_store(
        "tandem_op_heads_store",
        Box::new(|settings, store_path| {
            Ok(Box::new(tandem::TandemOpHeadsStore::load(settings, store_path)?))
        }),
    );

    CliRunner::init()
        .version(env!("CARGO_PKG_VERSION"))
        .add_store_factories(factories)
        .run()
        .into()
}
```

Then `jj-tandem init` would create a repo with `store/type = "tandem"`, and all
subsequent commands (`jj-tandem log`, `jj-tandem diff`, etc.) would use the
tandem backend transparently.

---

## 10. Recommended Approach for Tandem

### Architecture

```
┌─────────────────────┐     Cap'n Proto      ┌──────────────────────┐
│  Agent Machine A    │◄────────────────────►│   Tandem Server      │
│                     │                       │                      │
│  jj-tandem binary   │                       │  tandem serve        │
│  ├─ TandemBackend   │     getObject()       │  ├─ jj repo (git)   │
│  ├─ TandemOpStore   │     putObject()       │  ├─ git interop      │
│  └─ TandemOpHeads   │     getHeads()        │  └─ watchHeads()     │
│                     │     updateOpHeads()    │                      │
│  Local working copy │                       └──────────────────────┘
│  (.jj/working_copy/)│
└─────────────────────┘
```

### What Each Trait Implementation Does

| Trait | Tandem Implementation | RPC Calls |
|-------|----------------------|-----------|
| `Backend` | `TandemBackend` | `getObject(kind, id)` → `data`, `putObject(kind, data)` → `id` |
| `OpStore` | `TandemOpStore` | `getOperation(id)`, `putOperation(data)`, `getView(id)`, `putView(data)`, `resolveOperationIdPrefix(prefix)` |
| `OpHeadsStore` | `TandemOpHeadsStore` | `getHeads()`, `updateOpHeads(old_ids, new_id)` |

### What's Stored Locally vs Remote

| Component | Location | Notes |
|-----------|----------|-------|
| Working copy files | Local | Managed by `LocalWorkingCopy` |
| Working copy state | Local `.jj/working_copy/` | checkout info |
| Backend objects | **Remote** (server) | via RPC |
| Operations/views | **Remote** (server) | via RPC |
| Op heads | **Remote** (server) | via RPC with CAS |
| Index | Local `.jj/repo/index/` | Rebuilt locally |
| Store type files | Local `.jj/repo/store/type` = `"tandem"` | Points to factory |
| Server address | Local `.jj/repo/store/server_address` | Connection config |

### Initialization Flow

1. User runs: `jj-tandem init --server=host:13013 /path/to/workspace`
2. `jj-tandem` calls `Workspace::init_with_factories()` with `TandemBackend::init`
3. `TandemBackend::init` connects to server, gets `RepoInfo`, writes
   `store/type = "tandem"` and `store/server_address = "host:13013"`
4. Creates root commit/operation matching server state
5. Local working copy is initialized

### Subsequent Operations

1. User runs: `jj-tandem new -m "feat: add auth"`
2. jj reads `store/type` → `"tandem"` → looks up `TandemBackend` factory
3. `TandemBackend::load()` reads `store/server_address`, connects to server
4. All `read_file`/`write_file`/`read_tree`/`write_tree`/`read_commit`/`write_commit`
   calls go over Cap'n Proto RPC
5. Working copy checkout happens locally

---

## 11. Implementation Sketch

### 11.1 TandemBackend

```rust
use std::any::Any;
use std::fmt::Debug;
use std::path::Path;
use std::pin::Pin;
use std::time::SystemTime;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::io::AsyncRead;

use jj_lib::backend::*;
use jj_lib::index::Index;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};

#[derive(Debug)]
pub struct TandemBackend {
    /// Cap'n Proto RPC client to the tandem server
    client: TandemClient,
    /// Cached from server's RepoInfo
    commit_id_len: usize,
    change_id_len: usize,
    root_commit_id: CommitId,
    root_change_id: ChangeId,
    empty_tree_id: TreeId,
}

impl TandemBackend {
    pub fn name() -> &'static str { "tandem" }

    pub fn init(settings: &UserSettings, store_path: &Path) -> Result<Self, BackendInitError> {
        // Read server address from settings or store_path config
        let server_addr = read_server_address(settings, store_path)?;
        let client = TandemClient::connect(&server_addr)
            .map_err(|e| BackendInitError(e.into()))?;
        let info = client.get_repo_info()
            .map_err(|e| BackendInitError(e.into()))?;

        // Write server address for future loads
        std::fs::write(
            store_path.join("server_address"),
            server_addr.as_bytes(),
        ).map_err(|e| BackendInitError(e.into()))?;

        Ok(Self {
            client,
            commit_id_len: info.commit_id_length,
            change_id_len: info.change_id_length,
            root_commit_id: info.root_commit_id,
            root_change_id: info.root_change_id,
            empty_tree_id: info.empty_tree_id,
        })
    }

    pub fn load(settings: &UserSettings, store_path: &Path) -> Result<Self, BackendLoadError> {
        let server_addr = std::fs::read_to_string(store_path.join("server_address"))
            .map_err(|e| BackendLoadError(e.into()))?;
        let client = TandemClient::connect(&server_addr)
            .map_err(|e| BackendLoadError(e.into()))?;
        let info = client.get_repo_info()
            .map_err(|e| BackendLoadError(e.into()))?;

        Ok(Self {
            client,
            commit_id_len: info.commit_id_length,
            change_id_len: info.change_id_length,
            root_commit_id: info.root_commit_id,
            root_change_id: info.root_change_id,
            empty_tree_id: info.empty_tree_id,
        })
    }
}

#[async_trait]
impl Backend for TandemBackend {
    fn name(&self) -> &str { Self::name() }
    fn commit_id_length(&self) -> usize { self.commit_id_len }
    fn change_id_length(&self) -> usize { self.change_id_len }
    fn root_commit_id(&self) -> &CommitId { &self.root_commit_id }
    fn root_change_id(&self) -> &ChangeId { &self.root_change_id }
    fn empty_tree_id(&self) -> &TreeId { &self.empty_tree_id }
    fn concurrency(&self) -> usize { 64 }  // network backend

    async fn read_file(
        &self, _path: &RepoPath, id: &FileId,
    ) -> BackendResult<Pin<Box<dyn AsyncRead + Send>>> {
        let data = self.client.get_object(ObjectKind::File, id.as_bytes()).await?;
        Ok(Box::pin(std::io::Cursor::new(data)))
    }

    async fn write_file(
        &self, _path: &RepoPath, contents: &mut (dyn AsyncRead + Send + Unpin),
    ) -> BackendResult<FileId> {
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(contents, &mut buf).await
            .map_err(|e| BackendError::Other(e.into()))?;
        let id = self.client.put_object(ObjectKind::File, &buf).await?;
        Ok(FileId::new(id))
    }

    async fn read_symlink(&self, _path: &RepoPath, id: &SymlinkId) -> BackendResult<String> {
        let data = self.client.get_object(ObjectKind::Symlink, id.as_bytes()).await?;
        String::from_utf8(data).map_err(|e| BackendError::Other(e.into()))
    }

    async fn write_symlink(&self, _path: &RepoPath, target: &str) -> BackendResult<SymlinkId> {
        let id = self.client.put_object(ObjectKind::Symlink, target.as_bytes()).await?;
        Ok(SymlinkId::new(id))
    }

    async fn read_copy(&self, _id: &CopyId) -> BackendResult<CopyHistory> {
        Err(BackendError::Unsupported("Copy tracking not yet supported".into()))
    }
    async fn write_copy(&self, _copy: &CopyHistory) -> BackendResult<CopyId> {
        Err(BackendError::Unsupported("Copy tracking not yet supported".into()))
    }
    async fn get_related_copies(&self, _copy_id: &CopyId) -> BackendResult<Vec<CopyHistory>> {
        Err(BackendError::Unsupported("Copy tracking not yet supported".into()))
    }

    async fn read_tree(&self, _path: &RepoPath, id: &TreeId) -> BackendResult<Tree> {
        let data = self.client.get_object(ObjectKind::Tree, id.as_bytes()).await?;
        // Decode protobuf (same format as SimpleBackend)
        decode_tree_proto(&data)
    }

    async fn write_tree(&self, _path: &RepoPath, contents: &Tree) -> BackendResult<TreeId> {
        let data = encode_tree_proto(contents);
        let id = self.client.put_object(ObjectKind::Tree, &data).await?;
        Ok(TreeId::new(id))
    }

    async fn read_commit(&self, id: &CommitId) -> BackendResult<Commit> {
        if *id == self.root_commit_id {
            return Ok(make_root_commit(
                self.root_change_id.clone(),
                self.empty_tree_id.clone(),
            ));
        }
        let data = self.client.get_object(ObjectKind::Commit, id.as_bytes()).await?;
        decode_commit_proto(&data)
    }

    async fn write_commit(
        &self, contents: Commit, sign_with: Option<&mut SigningFn>,
    ) -> BackendResult<(CommitId, Commit)> {
        // Encode, optionally sign, send to server
        let data = encode_commit_proto(&contents, sign_with)?;
        let (id, normalized) = self.client.put_object_with_normalized(
            ObjectKind::Commit, &data
        ).await?;
        let commit = decode_commit_proto(&normalized)?;
        Ok((CommitId::new(id), commit))
    }

    fn get_copy_records(
        &self, _paths: Option<&[RepoPathBuf]>, _root: &CommitId, _head: &CommitId,
    ) -> BackendResult<BoxStream<'_, BackendResult<CopyRecord>>> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn gc(&self, _index: &dyn Index, _keep_newer: SystemTime) -> BackendResult<()> {
        // GC is server-side only
        Ok(())
    }
}
```

### 11.2 TandemOpStore

```rust
#[derive(Debug)]
pub struct TandemOpStore {
    client: TandemClient,
    root_operation_id: OperationId,
    root_data: RootOperationData,
}

#[async_trait]
impl OpStore for TandemOpStore {
    fn name(&self) -> &str { "tandem_op_store" }
    fn root_operation_id(&self) -> &OperationId { &self.root_operation_id }

    async fn read_view(&self, id: &ViewId) -> OpStoreResult<View> {
        let data = self.client.get_view(id.as_bytes()).await?;
        decode_view_proto(&data)
    }

    async fn write_view(&self, contents: &View) -> OpStoreResult<ViewId> {
        let data = encode_view_proto(contents);
        let id = self.client.put_view(&data).await?;
        Ok(ViewId::new(id))
    }

    async fn read_operation(&self, id: &OperationId) -> OpStoreResult<Operation> {
        if *id == self.root_operation_id {
            return Ok(Operation::make_root(/* root view id */));
        }
        let data = self.client.get_operation(id.as_bytes()).await?;
        decode_operation_proto(&data)
    }

    async fn write_operation(&self, contents: &Operation) -> OpStoreResult<OperationId> {
        let data = encode_operation_proto(contents);
        let id = self.client.put_operation(&data).await?;
        Ok(OperationId::new(id))
    }

    async fn resolve_operation_id_prefix(
        &self, prefix: &HexPrefix,
    ) -> OpStoreResult<PrefixResolution<OperationId>> {
        self.client.resolve_operation_id_prefix(prefix).await
    }

    fn gc(&self, _head_ids: &[OperationId], _keep_newer: SystemTime) -> OpStoreResult<()> {
        // GC is server-side only
        Ok(())
    }
}
```

### 11.3 TandemOpHeadsStore

```rust
#[derive(Debug)]
pub struct TandemOpHeadsStore {
    client: TandemClient,
}

#[async_trait]
impl OpHeadsStore for TandemOpHeadsStore {
    fn name(&self) -> &str { "tandem_op_heads_store" }

    async fn update_op_heads(
        &self, old_ids: &[OperationId], new_id: &OperationId,
    ) -> Result<(), OpHeadsStoreError> {
        self.client.update_op_heads(old_ids, new_id).await
            .map_err(|e| OpHeadsStoreError::Write {
                new_op_id: new_id.clone(),
                source: e.into(),
            })
    }

    async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError> {
        self.client.get_heads().await
            .map_err(|e| OpHeadsStoreError::Read(e.into()))
    }

    async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
        // Server-side CAS provides coordination; no client-side lock needed
        Ok(Box::new(NoopLock))
    }
}

struct NoopLock;
impl OpHeadsStoreLock for NoopLock {}
```

### 11.4 `Cargo.toml` for `jj-tandem`

```toml
[package]
name = "jj-tandem"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "jj-tandem"
path = "src/main.rs"

[dependencies]
jj-lib = { git = "https://github.com/jj-vcs/jj", features = ["git"] }
jj-cli = { git = "https://github.com/jj-vcs/jj", features = ["git"] }
tandem = { path = "../tandem-lib" }  # our backend implementations
capnp = "0.20"
capnp-rpc = "0.20"
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
futures = "0.3"
```

---

## Summary of Key Findings

1. **Three traits to implement:** `Backend` (18 methods), `OpStore` (7 methods), `OpHeadsStore` (4 methods). All methods required.

2. **Registration is via `StoreFactories`** with string-keyed factory closures. **Requires a custom binary** — stock `jj` cannot load plugins. The `CliRunner::add_store_factories()` API is the official extension point.

3. **Type dispatch** reads `.jj/repo/store/type` file. Write `"tandem"` on init; jj will call our factory on every subsequent load.

4. **Protobuf serialization** for commits/trees (via `prost`). Files are raw bytes. We can reuse the same proto encoding on the wire — the server stores objects in jj-native format and just proxies the bytes.

5. **Working copies are local.** The backend has no workspace awareness — that's managed by `View.wc_commit_ids`. Each agent has its own local working copy.

6. **Background sync is inadequate** for tandem's strong-consistency, real-time goals. Concurrent write races, partial syncs, and lack of real-time notifications make it fragile.

7. **The `jj-tandem` binary approach** is clean and aligns with jj's extension model. It's literally `CliRunner::init().add_store_factories(tandem_factories).run()` — all stock jj commands work transparently.
