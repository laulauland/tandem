Source demos/config.tape
Output demos/tandem-exe-dev.gif

# ============================================================================
# tandem: distributed jj workspaces across 3 VMs on exe.dev
#
# Two AI agents on separate VMs collaborating on code through a shared
# tandem server. Each agent sees the other's commits instantly.
# ============================================================================

Sleep 1s

# -- Create 3 VMs on exe.dev ------------------------------------------------

Type "# Create three exe.dev VMs: server + two agents"
Enter
Sleep 1s

Type "ssh exe.dev new --name tandem-server"
Enter
Wait@30s
Sleep 2s

Type "ssh exe.dev new --name tandem-agent-a"
Enter
Wait@30s
Sleep 2s

Type "ssh exe.dev new --name tandem-agent-b"
Enter
Wait@30s
Sleep 2s

# -- Copy tandem binary + scripts -------------------------------------------

Type "# Copy tandem binary to all VMs"
Enter
Sleep 1s

Type "BIN=target/x86_64-unknown-linux-musl/release/tandem"
Enter
Sleep 300ms

Type "scp $BIN tandem-server.exe.xyz:~/tandem"
Enter
Wait@60s
Sleep 1s

Type "scp $BIN tandem-agent-a.exe.xyz:~/tandem"
Enter
Wait@60s
Sleep 1s

Type "scp $BIN tandem-agent-b.exe.xyz:~/tandem"
Enter
Wait@60s
Sleep 2s

# -- Start the tandem server ------------------------------------------------

Type "# Start tandem server"
Enter
Sleep 1s

Type "scp demos/scripts/server-start.sh tandem-server.exe.xyz:/tmp/start.sh"
Enter
Wait@15s
Sleep 500ms

Type "ssh tandem-server.exe.xyz bash /tmp/start.sh"
Enter
Wait@15s
Sleep 2s

# -- Set up SSH tunnels ------------------------------------------------------

Type "# SSH tunnels: bridge raw TCP between VMs via localhost"
Enter
Sleep 1s

Type "ssh -f -N -L 15555:localhost:5555 tandem-server.exe.xyz"
Enter
Wait@10s
Sleep 1s

Type "ssh -f -N -R 13013:localhost:15555 tandem-agent-a.exe.xyz"
Enter
Wait@10s
Sleep 1s

Type "ssh -f -N -R 13013:localhost:15555 tandem-agent-b.exe.xyz"
Enter
Wait@10s
Sleep 2s

# -- Agent A: write auth module ----------------------------------------------

Type "# --- Agent A: write auth module ---"
Enter
Sleep 1s

Type "scp demos/scripts/agent-a.sh tandem-agent-a.exe.xyz:/tmp/setup.sh"
Enter
Wait@15s
Sleep 500ms

Type "ssh tandem-agent-a.exe.xyz bash /tmp/setup.sh"
Enter
Wait@30s
Sleep 2s

Type "# Agent A sees their commit"
Enter
Sleep 500ms

Type "ssh tandem-agent-a.exe.xyz 'cd ~/work && ~/tandem --config=fsmonitor.backend=none log'"
Enter
Wait@15s
Sleep 4s

# -- Agent B: see Agent A, then add API routes --------------------------------

Type "# --- Agent B: init workspace, see Agent A's work ---"
Enter
Sleep 1s

Type "scp demos/scripts/agent-b.sh tandem-agent-b.exe.xyz:/tmp/setup.sh"
Enter
Wait@15s
Sleep 500ms

Type "ssh tandem-agent-b.exe.xyz bash /tmp/setup.sh"
Enter
Wait@30s
Sleep 2s

Type "# Agent B sees both workspaces in the log"
Enter
Sleep 500ms

Type "ssh tandem-agent-b.exe.xyz 'cd ~/work && ~/tandem --config=fsmonitor.backend=none log'"
Enter
Wait@15s
Sleep 4s

Type "# Agent B reads Agent A's auth.rs from the shared store"
Enter
Sleep 500ms

Type "ssh tandem-agent-b.exe.xyz 'cd ~/work && ~/tandem --config=fsmonitor.backend=none file show -r @-- src/auth.rs'"
Enter
Wait@15s
Sleep 4s

# -- Server: everything is there --------------------------------------------

Type "# --- Server: all commits from both agents ---"
Enter
Sleep 1s

Type "ssh tandem-server.exe.xyz 'cd ~/project && ~/tandem --config=fsmonitor.backend=none log --no-graph --ignore-working-copy'"
Enter
Wait@15s
Sleep 4s

Type "# Server has everything. Ready for: jj git push"
Enter
Sleep 3s

# -- Fin ---------------------------------------------------------------------

Type "# Two agents, three VMs, one store. That's tandem."
Enter
Sleep 4s
