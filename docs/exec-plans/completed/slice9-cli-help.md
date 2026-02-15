# Slice 9 — CLI help and discoverability

- **Date completed:** 2026-02-15
- **Implementation:** `src/main.rs` (clap command definitions, AFTER_HELP constants)

## What was implemented

Comprehensive help text and error messages for agent discoverability:

1. **Command-specific help**
   - `tandem --help` — prints usage without server connection
   - `tandem serve --help` — explains `--listen` and `--repo` flags
   - `tandem init --help` — explains `--tandem-server` and `--workspace` flags
   - `tandem watch --help` — explains `--server` flag

2. **After-help text**
   - Main help includes JJ COMMANDS section listing common jj commands
   - Explains environment variables (`TANDEM_SERVER`, `TANDEM_WORKSPACE`)
   - Provides setup examples

3. **Smart command routing**
   - `tandem` with no args prints help (not an error)
   - Unknown commands are passed to jj (e.g., `tandem log` → jj's log command)
   - jj's own help system works: `tandem log --help` shows jj's log help

4. **Environment variable fallbacks**
   - `TANDEM_SERVER` — fallback for `--tandem-server` flag
   - `TANDEM_WORKSPACE` — fallback for `--workspace` flag (default: "default")

5. **Error messages**
   - Connection failures include the address that was tried
   - Missing required arguments show what's needed
   - All errors go to stderr, not stdout

## Acceptance coverage

Manual testing validates:
- `tandem --help` works offline
- `tandem serve --help` shows flag documentation
- `tandem init --help` includes examples
- `tandem xyz` (unknown command) suggests alternatives via jj's help system
- Connection errors are clear and actionable

## Architecture notes

Good help text is P0 for agent usability. Without `--help` and command suggestions, agents spend most of their time guessing commands. This slice ensures agents can discover tandem's capabilities without reading source code.
