//! Slice 2: Two-agent file visibility
//!
//! Acceptance criteria:
//! - Agent A writes src/auth.rs, commits
//! - Agent B (different workspace) runs jj log — sees Agent A's commit
//! - Agent B runs jj file show -r <agent-a-commit> src/auth.rs — gets exact bytes
//! - Agent B writes src/api.rs, commits
//! - Agent A runs jj file show -r <agent-b-commit> src/api.rs — gets exact bytes
//! - Both agents see both files through jj's normal tree traversal

mod common;

use tempfile::TempDir;

#[test]
fn v1_slice2_two_agent_file_visibility() {
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

    // ── Initialize Agent A workspace ──────────────────────────────────────
    let init_a = common::run_tandem_in(
        &agent_a_dir,
        &["init", "--server", &addr, "--workspace", "agent-a", "."],
        &home,
    );
    common::assert_ok(&init_a, "agent-a init");
    assert_eq!(
        std::fs::read_to_string(agent_a_dir.join(".jj/repo/store/type"))
            .unwrap()
            .trim(),
        "tandem",
        "agent-a store type should be tandem"
    );

    // ── Initialize Agent B workspace ──────────────────────────────────────
    let init_b = common::run_tandem_in(
        &agent_b_dir,
        &["init", "--server", &addr, "--workspace", "agent-b", "."],
        &home,
    );
    common::assert_ok(&init_b, "agent-b init");
    assert_eq!(
        std::fs::read_to_string(agent_b_dir.join(".jj/repo/store/type"))
            .unwrap()
            .trim(),
        "tandem",
        "agent-b store type should be tandem"
    );

    // ── Agent A: write src/auth.rs and commit ─────────────────────────────
    let auth_content = b"pub fn authenticate(token: &str) -> bool {\n    !token.is_empty()\n}\n";
    let src_a = agent_a_dir.join("src");
    std::fs::create_dir_all(&src_a).unwrap();
    std::fs::write(src_a.join("auth.rs"), auth_content).unwrap();

    // describe sets the description on the current working-copy change (which
    // first snapshots the working copy, capturing auth.rs into the commit).
    let describe_a = common::run_tandem_in(&agent_a_dir, &["describe", "-m", "add auth"], &home);
    common::assert_ok(&describe_a, "agent-a describe");

    // jj new creates a new empty child change; the described change becomes @-
    let new_a = common::run_tandem_in(&agent_a_dir, &["new"], &home);
    common::assert_ok(&new_a, "agent-a new");

    // Sanity: Agent A can read its own file
    let self_cat_a = common::run_tandem_in(
        &agent_a_dir,
        &["file", "show", "-r", "@-", "src/auth.rs"],
        &home,
    );
    common::assert_ok(&self_cat_a, "agent-a self file-show");
    assert_eq!(
        self_cat_a.stdout, auth_content,
        "agent-a should read its own auth.rs"
    );

    // Extract Agent A's auth commit commit_id for cross-workspace reference.
    // change_id can be divergent under concurrent operation reconciliation.
    let commit_a_out = common::run_tandem_in(
        &agent_a_dir,
        &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        &home,
    );
    common::assert_ok(&commit_a_out, "agent-a extract commit_id");
    let commit_a_id = common::stdout_str(&commit_a_out).trim().to_string();
    assert!(
        !commit_a_id.is_empty(),
        "should extract Agent A's commit_id"
    );

    // ── Agent B sees Agent A's commit ─────────────────────────────────────
    let log_b = common::run_tandem_in(&agent_b_dir, &["log", "--no-graph", "-r", "all()"], &home);
    common::assert_ok(&log_b, "agent-b log");
    let log_b_text = common::stdout_str(&log_b);
    assert!(
        log_b_text.contains("add auth"),
        "Agent B's log should show Agent A's commit 'add auth'\nlog output:\n{log_b_text}"
    );

    // ── Agent B reads Agent A's file (exact bytes) ────────────────────────
    let cat_auth = common::run_tandem_in(
        &agent_b_dir,
        &["file", "show", "-r", &commit_a_id, "src/auth.rs"],
        &home,
    );
    common::assert_ok(&cat_auth, "agent-b file show auth.rs");
    assert_eq!(
        cat_auth.stdout, auth_content,
        "Agent B should get exact bytes of Agent A's src/auth.rs"
    );

    // ── Agent B: write src/api.rs and commit ──────────────────────────────
    let api_content =
        b"pub fn handle_request(req: &str) -> String {\n    format!(\"OK: {req}\")\n}\n";
    let src_b = agent_b_dir.join("src");
    std::fs::create_dir_all(&src_b).unwrap();
    std::fs::write(src_b.join("api.rs"), api_content).unwrap();

    let describe_b = common::run_tandem_in(&agent_b_dir, &["describe", "-m", "add api"], &home);
    common::assert_ok(&describe_b, "agent-b describe");

    let new_b = common::run_tandem_in(&agent_b_dir, &["new"], &home);
    common::assert_ok(&new_b, "agent-b new");

    // Sanity: Agent B can read its own file
    let self_cat_b = common::run_tandem_in(
        &agent_b_dir,
        &["file", "show", "-r", "@-", "src/api.rs"],
        &home,
    );
    common::assert_ok(&self_cat_b, "agent-b self file-show");
    assert_eq!(
        self_cat_b.stdout, api_content,
        "agent-b should read its own api.rs"
    );

    // Extract Agent B's api commit commit_id for cross-workspace reference.
    let commit_b_out = common::run_tandem_in(
        &agent_b_dir,
        &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
        &home,
    );
    common::assert_ok(&commit_b_out, "agent-b extract commit_id");
    let commit_b_id = common::stdout_str(&commit_b_out).trim().to_string();
    assert!(
        !commit_b_id.is_empty(),
        "should extract Agent B's commit_id"
    );

    // ── Agent A reads Agent B's file (exact bytes) ────────────────────────
    let cat_api = common::run_tandem_in(
        &agent_a_dir,
        &["file", "show", "-r", &commit_b_id, "src/api.rs"],
        &home,
    );
    common::assert_ok(&cat_api, "agent-a file show api.rs");
    assert_eq!(
        cat_api.stdout, api_content,
        "Agent A should get exact bytes of Agent B's src/api.rs"
    );

    // ── Both agents see both commits in their logs ────────────────────────
    let log_a_final =
        common::run_tandem_in(&agent_a_dir, &["log", "--no-graph", "-r", "all()"], &home);
    common::assert_ok(&log_a_final, "agent-a final log");
    let log_a_text = common::stdout_str(&log_a_final);
    assert!(
        log_a_text.contains("add auth"),
        "Agent A's final log should show 'add auth'\n{log_a_text}"
    );
    assert!(
        log_a_text.contains("add api"),
        "Agent A's final log should show 'add api'\n{log_a_text}"
    );

    let log_b_final =
        common::run_tandem_in(&agent_b_dir, &["log", "--no-graph", "-r", "all()"], &home);
    common::assert_ok(&log_b_final, "agent-b final log");
    let log_b_final_text = common::stdout_str(&log_b_final);
    assert!(
        log_b_final_text.contains("add auth"),
        "Agent B's final log should show 'add auth'\n{log_b_final_text}"
    );
    assert!(
        log_b_final_text.contains("add api"),
        "Agent B's final log should show 'add api'\n{log_b_final_text}"
    );

    // ── Both agents see both files through tree traversal ─────────────────
    // Agent A's @- has auth.rs, Agent B's @- has api.rs.
    // After the op merge, each agent's tree is independent, but
    // cross-reading via commit_id works (already verified above).
    // Additionally check that diff shows file additions:
    let diff_a = common::run_tandem_in(&agent_a_dir, &["diff", "-r", &commit_a_id], &home);
    common::assert_ok(&diff_a, "agent-a diff on auth commit");
    let diff_a_text = common::stdout_str(&diff_a);
    assert!(
        diff_a_text.contains("auth.rs"),
        "diff of Agent A's commit should show auth.rs\n{diff_a_text}"
    );

    let diff_b = common::run_tandem_in(&agent_b_dir, &["diff", "-r", &commit_b_id], &home);
    common::assert_ok(&diff_b, "agent-b diff on api commit");
    let diff_b_text = common::stdout_str(&diff_b);
    assert!(
        diff_b_text.contains("api.rs"),
        "diff of Agent B's commit should show api.rs\n{diff_b_text}"
    );

    // ── Cleanup ───────────────────────────────────────────────────────────
    let _ = server.kill();
    let _ = server.wait();
}
