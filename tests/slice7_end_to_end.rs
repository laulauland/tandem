//! Slice 7: End-to-end multi-agent with git shipping
//!
//! Acceptance criteria:
//! - Agent A writes src/auth.rs, commits
//! - Agent B writes src/api.rs, commits concurrently
//! - Both see each other's files via `jj file show`
//! - Server pushes to GitHub (bare git remote)
//! - `git clone` of remote contains both files with correct content
//! - Agent A creates a bookmark, Agent B sees it
//! - File contents are byte-identical from all perspectives

mod common;

use std::path::Path;
use std::process::Output;
use tempfile::TempDir;

/// Run a tandem command, handling "working copy is stale" by running
/// `workspace update-stale` and retrying. This is expected during
/// concurrent multi-agent operations.
fn run_tandem_resilient(dir: &Path, args: &[&str], home: &Path) -> Output {
    let output = common::run_tandem_in(dir, args, home);
    if output.status.success() {
        return output;
    }
    let err = common::stderr_str(&output);
    if err.contains("working copy is stale") || err.contains("update-stale") {
        let update = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
        common::assert_ok(&update, "workspace update-stale (resilient)");
        common::run_tandem_in(dir, args, home)
    } else {
        output
    }
}

/// Ensure the workspace is not stale before running queries.
fn settle(dir: &Path, label: &str, home: &Path) {
    let update = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
    if !update.status.success() {
        let err = common::stderr_str(&update);
        if !err.contains("nothing to do") && !err.contains("already up to date") {
            common::assert_ok(&update, &format!("{label} settle"));
        }
    }
}

/// Find a commit_id by description substring (avoids divergent change_id issues).
fn find_commit_id(dir: &Path, desc_substring: &str, home: &Path) -> String {
    let revset = format!("description(substring:\"{desc_substring}\")");
    let out = common::run_tandem_in(
        dir,
        &["log", "--no-graph", "-r", &revset, "-T", "commit_id ++ \"\\n\""],
        home,
    );
    common::assert_ok(&out, &format!("find commit_id for '{desc_substring}'"));
    let text = common::stdout_str(&out);
    let commit_id = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    assert!(
        !commit_id.is_empty(),
        "should find commit_id for '{desc_substring}', got log output:\n{text}"
    );
    commit_id
}

#[test]
fn slice7_two_agents_files_bookmarks_git_round_trip() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let agent_a_dir = tmp.path().join("agent-a");
    std::fs::create_dir_all(&agent_a_dir).unwrap();
    let agent_b_dir = tmp.path().join("agent-b");
    std::fs::create_dir_all(&agent_b_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // ── Initialize both agent workspaces ──────────────────────────────
    let init_a = common::run_tandem_in(
        &agent_a_dir,
        &["init", "--tandem-server", &addr, "--workspace", "agent-a", "."],
        &home,
    );
    common::assert_ok(&init_a, "agent-a init");

    let init_b = common::run_tandem_in(
        &agent_b_dir,
        &["init", "--tandem-server", &addr, "--workspace", "agent-b", "."],
        &home,
    );
    common::assert_ok(&init_b, "agent-b init");

    // ── Define file contents ──────────────────────────────────────────
    let auth_content =
        b"pub fn authenticate(token: &str) -> bool {\n    token.len() > 8\n}\n\n\
          pub fn validate_session(session_id: &str) -> bool {\n    !session_id.is_empty()\n}\n";
    let api_content =
        b"pub fn handle_request(method: &str, path: &str) -> String {\n    \
          format!(\"{method} {path} -> 200 OK\")\n}\n\n\
          pub fn health_check() -> &'static str {\n    \"healthy\"\n}\n";

    // ── Agent A: write src/auth.rs and commit ─────────────────────────
    let src_a = agent_a_dir.join("src");
    std::fs::create_dir_all(&src_a).unwrap();
    std::fs::write(src_a.join("auth.rs"), auth_content).unwrap();

    let describe_a = run_tandem_resilient(&agent_a_dir, &["describe", "-m", "add auth module"], &home);
    common::assert_ok(&describe_a, "agent-a describe");

    let new_a = run_tandem_resilient(&agent_a_dir, &["new"], &home);
    common::assert_ok(&new_a, "agent-a new");

    // ── Agent B: write src/api.rs and commit ──────────────────────────
    let src_b = agent_b_dir.join("src");
    std::fs::create_dir_all(&src_b).unwrap();
    std::fs::write(src_b.join("api.rs"), api_content).unwrap();

    let describe_b = run_tandem_resilient(&agent_b_dir, &["describe", "-m", "add api module"], &home);
    common::assert_ok(&describe_b, "agent-b describe");

    let new_b = run_tandem_resilient(&agent_b_dir, &["new"], &home);
    common::assert_ok(&new_b, "agent-b new");

    // ── Settle both workspaces ────────────────────────────────────────
    settle(&agent_a_dir, "agent-a", &home);
    settle(&agent_b_dir, "agent-b", &home);

    // ── Cross-visibility: both agents see each other's commits ────────
    let log_a = common::run_tandem_in(
        &agent_a_dir,
        &["log", "--no-graph", "-r", "all()"],
        &home,
    );
    common::assert_ok(&log_a, "agent-a log all");
    let log_a_text = common::stdout_str(&log_a);
    assert!(
        log_a_text.contains("add auth module"),
        "agent-a should see own commit\n{log_a_text}"
    );
    assert!(
        log_a_text.contains("add api module"),
        "agent-a should see agent-b's commit\n{log_a_text}"
    );

    let log_b = common::run_tandem_in(
        &agent_b_dir,
        &["log", "--no-graph", "-r", "all()"],
        &home,
    );
    common::assert_ok(&log_b, "agent-b log all");
    let log_b_text = common::stdout_str(&log_b);
    assert!(
        log_b_text.contains("add auth module"),
        "agent-b should see agent-a's commit\n{log_b_text}"
    );
    assert!(
        log_b_text.contains("add api module"),
        "agent-b should see own commit\n{log_b_text}"
    );

    // ── Cross-read files: exact byte verification ─────────────────────
    let commit_auth = find_commit_id(&agent_a_dir, "add auth module", &home);
    let commit_api = find_commit_id(&agent_a_dir, "add api module", &home);

    // Agent A reads Agent B's file
    let cat_api_from_a = common::run_tandem_in(
        &agent_a_dir,
        &["file", "show", "-r", &commit_api, "src/api.rs"],
        &home,
    );
    common::assert_ok(&cat_api_from_a, "agent-a reads api.rs");
    assert_eq!(
        cat_api_from_a.stdout, api_content,
        "agent-a: api.rs byte mismatch"
    );

    // Agent B reads Agent A's file
    let cat_auth_from_b = common::run_tandem_in(
        &agent_b_dir,
        &["file", "show", "-r", &commit_auth, "src/auth.rs"],
        &home,
    );
    common::assert_ok(&cat_auth_from_b, "agent-b reads auth.rs");
    assert_eq!(
        cat_auth_from_b.stdout, auth_content,
        "agent-b: auth.rs byte mismatch"
    );

    // ── Agent A creates a bookmark ────────────────────────────────────
    let bookmark_create = run_tandem_resilient(
        &agent_a_dir,
        &["bookmark", "create", "feature-x", "-r", &commit_auth],
        &home,
    );
    common::assert_ok(&bookmark_create, "agent-a bookmark create feature-x");

    // ── Agent B sees the bookmark ─────────────────────────────────────
    settle(&agent_b_dir, "agent-b pre-bookmark-list", &home);
    let bookmark_list = common::run_tandem_in(
        &agent_b_dir,
        &["bookmark", "list"],
        &home,
    );
    common::assert_ok(&bookmark_list, "agent-b bookmark list");
    let bookmark_text = common::stdout_str(&bookmark_list);
    assert!(
        bookmark_text.contains("feature-x"),
        "agent-b should see 'feature-x' bookmark\nbookmark list:\n{bookmark_text}"
    );

    // ── Server-side verification: both files exist ────────────────────
    let server_cat_auth = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "file", "show", "--ignore-working-copy",
            "-r", &commit_auth,
            "src/auth.rs",
        ],
        &[],
        &home,
    );
    common::assert_ok(&server_cat_auth, "server file show auth.rs");
    assert_eq!(
        server_cat_auth.stdout, auth_content,
        "server: auth.rs byte mismatch"
    );

    let server_cat_api = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "file", "show", "--ignore-working-copy",
            "-r", &commit_api,
            "src/api.rs",
        ],
        &[],
        &home,
    );
    common::assert_ok(&server_cat_api, "server file show api.rs");
    assert_eq!(
        server_cat_api.stdout, api_content,
        "server: api.rs byte mismatch"
    );

    // ── Git round-trip: push to bare remote, clone, verify ────────────
    let bare_remote = tmp.path().join("bare-remote.git");
    common::assert_ok(
        &common::run_git_in(
            tmp.path(),
            &["init", "--bare", bare_remote.to_str().unwrap()],
        ),
        "git init --bare",
    );
    common::assert_ok(
        &common::run_git_in(
            &server_repo,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        ),
        "git remote add",
    );

    // Merge both agents' work into a single commit for shipping.
    // Create a merge commit that has both agents' commits as parents.
    // First, create a bookmark pointing to a merge of both.
    let merge_out = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "new", "--ignore-working-copy",
            "-m", "merge: auth + api",
            &commit_auth, &commit_api,
        ],
        &[],
        &home,
    );
    common::assert_ok(&merge_out, "server create merge commit");

    // Find the merge commit
    let merge_id = find_commit_id_on_server(&server_repo, "merge: auth + api", &home);

    // Verify merge commit has both files
    let merge_auth = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "file", "show", "--ignore-working-copy",
            "-r", &merge_id,
            "src/auth.rs",
        ],
        &[],
        &home,
    );
    common::assert_ok(&merge_auth, "merge has auth.rs");
    assert_eq!(merge_auth.stdout, auth_content, "merge: auth.rs mismatch");

    let merge_api = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "file", "show", "--ignore-working-copy",
            "-r", &merge_id,
            "src/api.rs",
        ],
        &[],
        &home,
    );
    common::assert_ok(&merge_api, "merge has api.rs");
    assert_eq!(merge_api.stdout, api_content, "merge: api.rs mismatch");

    // Create bookmark on the merge and push
    let bookmark_main = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "bookmark", "create", "--ignore-working-copy",
            "main", "-r", &merge_id,
        ],
        &[],
        &home,
    );
    common::assert_ok(&bookmark_main, "server bookmark create main");

    let git_push = common::run_tandem_in_with_env(
        &server_repo,
        &["git", "push", "--ignore-working-copy", "--bookmark", "main"],
        &[],
        &home,
    );
    common::assert_ok(&git_push, "jj git push");

    // ── Clone and verify file content ─────────────────────────────────
    let clone_dir = tmp.path().join("clone");
    common::assert_ok(
        &common::run_git_in(
            tmp.path(),
            &["clone", bare_remote.to_str().unwrap(), clone_dir.to_str().unwrap()],
        ),
        "git clone",
    );

    let cloned_auth = std::fs::read(clone_dir.join("src/auth.rs"))
        .expect("auth.rs should exist in clone");
    assert_eq!(
        cloned_auth, auth_content,
        "cloned auth.rs should be byte-identical"
    );

    let cloned_api = std::fs::read(clone_dir.join("src/api.rs"))
        .expect("api.rs should exist in clone");
    assert_eq!(
        cloned_api, api_content,
        "cloned api.rs should be byte-identical"
    );

    let _ = server.kill();
    let _ = server.wait();
}

/// Helper to find commit_id on the server repo (uses --ignore-working-copy).
fn find_commit_id_on_server(server_repo: &Path, desc_substring: &str, home: &Path) -> String {
    let revset = format!("description(substring:\"{desc_substring}\")");
    let out = common::run_tandem_in_with_env(
        server_repo,
        &[
            "log", "--ignore-working-copy", "--no-graph",
            "-r", &revset,
            "-T", "commit_id ++ \"\\n\"",
        ],
        &[],
        home,
    );
    common::assert_ok(&out, &format!("server find commit_id for '{desc_substring}'"));
    let text = common::stdout_str(&out);
    let commit_id = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    assert!(
        !commit_id.is_empty(),
        "server should find commit_id for '{desc_substring}', got:\n{text}"
    );
    commit_id
}

/// Verify byte-identical content from all three perspectives:
/// agent A, agent B, and the server.
#[test]
fn slice7_byte_identity_all_perspectives() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let agent_a_dir = tmp.path().join("agent-a");
    std::fs::create_dir_all(&agent_a_dir).unwrap();
    let agent_b_dir = tmp.path().join("agent-b");
    std::fs::create_dir_all(&agent_b_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Init both workspaces
    common::assert_ok(
        &common::run_tandem_in(
            &agent_a_dir,
            &["init", "--tandem-server", &addr, "--workspace", "agent-a", "."],
            &home,
        ),
        "agent-a init",
    );
    common::assert_ok(
        &common::run_tandem_in(
            &agent_b_dir,
            &["init", "--tandem-server", &addr, "--workspace", "agent-b", "."],
            &home,
        ),
        "agent-b init",
    );

    // Agent A writes multiple files in one commit
    let src_a = agent_a_dir.join("src");
    std::fs::create_dir_all(&src_a).unwrap();
    let auth_content = b"// auth.rs\npub fn auth() -> bool { true }\n";
    let config_content = b"// config.rs\npub const MAX_RETRIES: u32 = 3;\n";
    std::fs::write(src_a.join("auth.rs"), auth_content).unwrap();
    std::fs::write(src_a.join("config.rs"), config_content).unwrap();

    common::assert_ok(
        &run_tandem_resilient(&agent_a_dir, &["describe", "-m", "agent-a: auth + config"], &home),
        "agent-a describe",
    );
    common::assert_ok(
        &run_tandem_resilient(&agent_a_dir, &["new"], &home),
        "agent-a new",
    );

    // Agent B writes its own files
    let src_b = agent_b_dir.join("src");
    std::fs::create_dir_all(&src_b).unwrap();
    let api_content = b"// api.rs\npub fn handle() -> &'static str { \"ok\" }\n";
    let routes_content = b"// routes.rs\npub fn setup_routes() {}\n";
    std::fs::write(src_b.join("api.rs"), api_content).unwrap();
    std::fs::write(src_b.join("routes.rs"), routes_content).unwrap();

    common::assert_ok(
        &run_tandem_resilient(&agent_b_dir, &["describe", "-m", "agent-b: api + routes"], &home),
        "agent-b describe",
    );
    common::assert_ok(
        &run_tandem_resilient(&agent_b_dir, &["new"], &home),
        "agent-b new",
    );

    // Settle
    settle(&agent_a_dir, "agent-a", &home);
    settle(&agent_b_dir, "agent-b", &home);

    // Find commit IDs
    let commit_a = find_commit_id(&agent_a_dir, "agent-a: auth + config", &home);
    let commit_b = find_commit_id(&agent_a_dir, "agent-b: api + routes", &home);

    // Verify every file from all 3 perspectives (agent A, agent B, server)
    let files_a: Vec<(&str, &[u8])> = vec![
        ("src/auth.rs", auth_content),
        ("src/config.rs", config_content),
    ];
    let files_b: Vec<(&str, &[u8])> = vec![
        ("src/api.rs", api_content),
        ("src/routes.rs", routes_content),
    ];

    for (path, expected) in &files_a {
        // From agent A
        let out = common::run_tandem_in(
            &agent_a_dir,
            &["file", "show", "-r", &commit_a, path],
            &home,
        );
        common::assert_ok(&out, &format!("agent-a reads {path}"));
        assert_eq!(&out.stdout[..], *expected, "agent-a: {path} mismatch");

        // From agent B
        let out = common::run_tandem_in(
            &agent_b_dir,
            &["file", "show", "-r", &commit_a, path],
            &home,
        );
        common::assert_ok(&out, &format!("agent-b reads {path}"));
        assert_eq!(&out.stdout[..], *expected, "agent-b: {path} mismatch");

        // From server
        let out = common::run_tandem_in_with_env(
            &server_repo,
            &["file", "show", "--ignore-working-copy", "-r", &commit_a, path],
            &[],
            &home,
        );
        common::assert_ok(&out, &format!("server reads {path}"));
        assert_eq!(&out.stdout[..], *expected, "server: {path} mismatch");
    }

    for (path, expected) in &files_b {
        // From agent A
        let out = common::run_tandem_in(
            &agent_a_dir,
            &["file", "show", "-r", &commit_b, path],
            &home,
        );
        common::assert_ok(&out, &format!("agent-a reads {path}"));
        assert_eq!(&out.stdout[..], *expected, "agent-a: {path} mismatch");

        // From agent B
        let out = common::run_tandem_in(
            &agent_b_dir,
            &["file", "show", "-r", &commit_b, path],
            &home,
        );
        common::assert_ok(&out, &format!("agent-b reads {path}"));
        assert_eq!(&out.stdout[..], *expected, "agent-b: {path} mismatch");

        // From server
        let out = common::run_tandem_in_with_env(
            &server_repo,
            &["file", "show", "--ignore-working-copy", "-r", &commit_b, path],
            &[],
            &home,
        );
        common::assert_ok(&out, &format!("server reads {path}"));
        assert_eq!(&out.stdout[..], *expected, "server: {path} mismatch");
    }

    let _ = server.kill();
    let _ = server.wait();
}
