# Tandem QA Test Plan

## Prerequisites

1. Rust toolchain installed
2. `jj` (Jujutsu) installed
3. `sqlite3` CLI available
4. `curl` and `jq` for API testing
5. Two terminal windows minimum

## Build

```bash
cd /home/lau/code/laulauland/tandem
cargo build --release
```

**Expected**: Build completes with warnings only, no errors. Binaries at:
- `target/release/tandem-server`
- `target/release/jjf`

---

## Test 1: Server Startup

### Steps
```bash
# Terminal 1
DATABASE_URL=sqlite:tandem.db DATA_DIR=./data ./target/release/tandem-server
```

### Expected Output
```
Server running on http://localhost:3000
```

### Verify
```bash
curl http://localhost:3000/health
```

### Expected Response
```json
{"status":"ok"}
```

---

## Test 2: User Creation and Authentication

### Steps
```bash
# Create database tables (auto-created on first run, but verify)
sqlite3 tandem.db ".tables"
```

### Expected Output
```
auth_tokens  repo_access  repos        users
```

### Create Test User
```bash
sqlite3 tandem.db "INSERT INTO users (id, email, name, password_hash)
  VALUES ('user-alice', 'alice@example.com', 'Alice', 'password123');"
```

### Login
```bash
curl -s -X POST http://localhost:3000/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email":"alice@example.com","password":"password123"}'
```

### Expected Response
```json
{
  "token": "<64-char-hex-string>",
  "expires_at": "<RFC3339-timestamp>"
}
```

### Save Token
```bash
export TOKEN="<paste-token-here>"
```

### Verify Token
```bash
curl -s http://localhost:3000/api/auth/me \
  -H "Authorization: Bearer $TOKEN"
```

### Expected Response
```json
{
  "id": "user-alice",
  "email": "alice@example.com",
  "name": "Alice"
}
```

---

## Test 3: Repository Creation

### Steps
```bash
curl -s -X POST http://localhost:3000/api/repos \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name":"my-project","org":"acme"}'
```

### Expected Response
```json
{
  "id": "<uuid>",
  "name": "my-project",
  "org": "acme",
  "created_at": "<RFC3339-timestamp>"
}
```

### Save Repo ID
```bash
export REPO_ID="<paste-repo-id-here>"
```

### Verify Repo Created
```bash
curl -s http://localhost:3000/api/repos \
  -H "Authorization: Bearer $TOKEN"
```

### Expected Response
Array containing the created repo (user has admin access as creator).

---

## Test 4: Access Control

### Create Second User Without Access
```bash
sqlite3 tandem.db "INSERT INTO users (id, email, name, password_hash)
  VALUES ('user-bob', 'bob@example.com', 'Bob', 'password456');"

# Login as Bob
BOB_TOKEN=$(curl -s -X POST http://localhost:3000/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email":"bob@example.com","password":"password456"}' | jq -r .token)
```

### Try to Access Repo as Bob
```bash
curl -s http://localhost:3000/api/repos/$REPO_ID \
  -H "Authorization: Bearer $BOB_TOKEN"
```

### Expected Response
HTTP 403 Forbidden (Bob has no access to Alice's repo)

### Grant Bob Read Access
```bash
sqlite3 tandem.db "INSERT INTO repo_access (repo_id, user_id, role)
  VALUES ('$REPO_ID', 'user-bob', 'read');"
```

### Retry Access
```bash
curl -s http://localhost:3000/api/repos/$REPO_ID \
  -H "Authorization: Bearer $BOB_TOKEN"
```

### Expected Response
```json
{
  "id": "<repo-id>",
  "name": "my-project",
  "org": "acme",
  "created_at": "<timestamp>"
}
```

---

## Test 5: jjf CLI - Link Repository

### Setup Local jj Repo
```bash
# Terminal 2
mkdir /tmp/test-project && cd /tmp/test-project
jj init
echo "Hello World" > README.md
jj new -m "Initial commit"
```

### Link to Forge
```bash
/home/lau/code/laulauland/tandem/target/release/jjf link \
  http://localhost:3000/acme/my-project \
  --token $TOKEN
```

### Expected Output
```
✓ Linked to forge: http://localhost:3000/acme/my-project
  Run 'jjf daemon start' to begin syncing
```

### Verify Config Created
```bash
cat .jj/forge.toml
```

### Expected Content
```toml
[forge]
url = "http://localhost:3000/acme/my-project"
```

---

## Test 6: jjf CLI - Status

### Steps
```bash
/home/lau/code/laulauland/tandem/target/release/jjf status
```

### Expected Output
```
Repository: /tmp/test-project
Forge: http://localhost:3000/acme/my-project
Status: Not syncing (daemon not running)
```

---

## Test 7: jjf Daemon - WebSocket Sync

### Start Daemon
```bash
# Terminal 2 (in /tmp/test-project)
/home/lau/code/laulauland/tandem/target/release/jjf daemon start
```

### Expected Output
```
Starting daemon for /tmp/test-project
Connecting to forge: ws://localhost:3000/sync/my-project
```

### Verify in Server Logs (Terminal 1)
```
Client connected to sync for repo my-project
```

### Stop Daemon
Press `Ctrl+C`

### Expected Output
```
Daemon shutting down
```

---

## Test 8: jjf Clone

### Steps
```bash
# Terminal 2
cd /tmp
/home/lau/code/laulauland/tandem/target/release/jjf clone \
  http://localhost:3000/acme/my-project \
  --token $TOKEN
```

### Expected Output
```
Cloning into '/tmp/my-project'...
  Syncing initial state...
  ✓ Received X changes, Y bookmarks
✓ Linked to forge: http://localhost:3000/acme/my-project
  Run 'jjf daemon start' to begin syncing
✓ Cloned repository to /tmp/my-project
```

### Verify Clone
```bash
cd /tmp/my-project
ls -la .jj/
cat .jj/forge.toml
```

---

## Test 9: WebSocket Broadcast (Multi-Client Sync)

### Setup
Start two daemon instances connected to the same repo.

### Terminal 2
```bash
cd /tmp/test-project
/home/lau/code/laulauland/tandem/target/release/jjf daemon start
```

### Terminal 3
```bash
cd /tmp/my-project  # The cloned repo
/home/lau/code/laulauland/tandem/target/release/jjf daemon start
```

### Expected Server Logs
```
Client connected to sync for repo my-project
Client connected to sync for repo my-project
```

### Test Sync
Make a change in one repo and verify it appears in the other.

**Note**: Full bidirectional sync requires the daemon to push local jj changes, which needs additional integration with jj-lib's operation log watching.

---

## Test 10: REST API - Changes and Bookmarks

### List Changes
```bash
curl -s http://localhost:3000/api/repos/$REPO_ID/changes \
  -H "Authorization: Bearer $TOKEN"
```

### Expected Response
```json
[]
```
(Empty until changes are synced from a jj repo)

### List Bookmarks
```bash
curl -s http://localhost:3000/api/repos/$REPO_ID/bookmarks \
  -H "Authorization: Bearer $TOKEN"
```

### Expected Response
```json
[]
```

---

## Test 11: Content Endpoint

### Store Test Content (via server)
This requires content to be synced first. Manual test:

```bash
curl -s http://localhost:3000/api/repos/$REPO_ID/content/abc123 \
  -H "Authorization: Bearer $TOKEN"
```

### Expected Response
HTTP 404 (content not found - expected for non-existent hash)

---

## Test 12: Events WebSocket

### Connect to Events
```bash
# Requires websocat or similar
websocat ws://localhost:3000/events/$REPO_ID
```

### Expected Initial Message
```json
{"type":"connected","repo_id":"<repo-id>"}
```

---

## Test 13: jj-lib Integration

### Verify jj Repository Reading
```bash
cd /tmp/test-project
/home/lau/code/laulauland/tandem/target/release/jjf list
```

### Expected Output
List of changes from the jj repository (may be empty for new repos).

---

## Known Limitations

1. **Tree Hash Placeholder**: Tree hashes use first 20 bytes of change_id (jj-lib async limitation)
2. **Password Storage**: Plaintext comparison (use bcrypt in production)
3. **Token in CLI**: Currently passed as flag (should use keychain)
4. **Daemon Status**: `jjf daemon status` not fully implemented
5. **Presence Warnings**: Require daemon IPC (currently stub)
6. **Log Interception**: `jjf alias` log presence injection is stub

---

## Cleanup

```bash
# Stop server (Ctrl+C in Terminal 1)

# Remove test data
rm -rf /tmp/test-project /tmp/my-project
rm tandem.db data/

# Or keep for further testing
```
