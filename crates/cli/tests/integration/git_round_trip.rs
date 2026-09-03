//! One end-to-end trip: two agents, real files, a bookmark, and a git clone.
//!
//! This is the test a subprocess is genuinely the coverage for. Everything in
//! it — the CLI, jj's own working-copy handling, the git backend on the
//! server, `git clone` reading what tandem wrote — is a program tandem does
//! not control, and an in-process harness would have to fake all of it.
//!
//! It was three tests once, which is three server starts and three clones for
//! one claim. What they asserted separately is asserted here in one pass:
//! files at the repo root and in nested directories, both agents reading each
//! other's bytes, the server reading both, a bookmark crossing between agents,
//! and the clone matching byte for byte.

use crate::common;
use crate::common::workspace::{
    find_commit_id, find_commit_id_on_server, run_tandem_resilient, settle_workspace,
};
use crate::common::ServerFixture;

#[test]
fn two_agents_files_bookmarks_and_a_git_clone() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();
    let server_repo = fx.repo.clone();
    let tmp = fx.path().to_path_buf();

    // ── Initialize both agent workspaces ──────────────────────────────
    let agent_a_dir = fx.init_workspace("agent-a", Some("agent-a"));
    let agent_b_dir = fx.init_workspace("agent-b", Some("agent-b"));

    // ── Define file contents ──────────────────────────────────────────
    let auth_content = b"pub fn authenticate(token: &str) -> bool {\n    token.len() > 8\n}\n\n\
          pub fn validate_session(session_id: &str) -> bool {\n    !session_id.is_empty()\n}\n";
    let readme_content = b"# tandem\n\nWritten by agent-a, read by everyone.\n";
    let api_content = b"pub fn handle_request(method: &str, path: &str) -> String {\n    \
          format!(\"{method} {path} -> 200 OK\")\n}\n\n\
          pub fn health_check() -> &'static str {\n    \"healthy\"\n}\n";

    // ── Agent A: write src/auth.rs and commit ─────────────────────────
    let src_a = agent_a_dir.join("src");
    std::fs::create_dir_all(&src_a).unwrap();
    std::fs::write(src_a.join("auth.rs"), auth_content).unwrap();
    // A file at the repo root as well as one in a directory: a tree walk
    // that only handles nested paths passes without this.
    std::fs::write(agent_a_dir.join("README.md"), readme_content).unwrap();

    let (describe_a, _) =
        run_tandem_resilient(&agent_a_dir, &["describe", "-m", "add auth module"], &home);
    common::assert_ok(&describe_a, "agent-a describe");

    let (new_a, _) = run_tandem_resilient(&agent_a_dir, &["new"], &home);
    common::assert_ok(&new_a, "agent-a new");

    // ── Agent B: write src/api.rs and commit ──────────────────────────
    let src_b = agent_b_dir.join("src");
    std::fs::create_dir_all(&src_b).unwrap();
    std::fs::write(src_b.join("api.rs"), api_content).unwrap();

    let (describe_b, _) =
        run_tandem_resilient(&agent_b_dir, &["describe", "-m", "add api module"], &home);
    common::assert_ok(&describe_b, "agent-b describe");

    let (new_b, _) = run_tandem_resilient(&agent_b_dir, &["new"], &home);
    common::assert_ok(&new_b, "agent-b new");

    // ── Settle both workspaces ────────────────────────────────────────
    settle_workspace(&agent_a_dir, &home);
    settle_workspace(&agent_b_dir, &home);

    // ── Cross-visibility: both agents see each other's commits ────────
    let log_a = common::run_tandem_in(&agent_a_dir, &["log", "--no-graph", "-r", "all()"], &home);
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

    let log_b = common::run_tandem_in(&agent_b_dir, &["log", "--no-graph", "-r", "all()"], &home);
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
    let (bookmark_create, _) = run_tandem_resilient(
        &agent_a_dir,
        &[
            "bookmark",
            "create",
            "agent-a/feature-x",
            "-r",
            &commit_auth,
        ],
        &home,
    );
    common::assert_ok(
        &bookmark_create,
        "agent-a bookmark create agent-a/feature-x",
    );

    // ── Agent B sees the bookmark ─────────────────────────────────────
    settle_workspace(&agent_b_dir, &home);
    let bookmark_list = common::run_tandem_in(&agent_b_dir, &["bookmark", "list"], &home);
    common::assert_ok(&bookmark_list, "agent-b bookmark list");
    let bookmark_text = common::stdout_str(&bookmark_list);
    assert!(
        bookmark_text.contains("agent-a/feature-x"),
        "agent-b should see the 'agent-a/feature-x' bookmark\nbookmark list:\n{bookmark_text}"
    );

    // ── Server-side verification: both files exist ────────────────────
    let server_cat_auth = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &commit_auth,
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
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &commit_api,
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
    let bare_remote = tmp.join("bare-remote.git");
    common::assert_ok(
        &common::run_git_in(&tmp, &["init", "--bare", bare_remote.to_str().unwrap()]),
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
            "new",
            "--ignore-working-copy",
            "-m",
            "merge: auth + api",
            &commit_auth,
            &commit_api,
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
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &merge_id,
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
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &merge_id,
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
            "bookmark",
            "create",
            "--ignore-working-copy",
            "main",
            "-r",
            &merge_id,
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
    let clone_dir = tmp.join("clone");
    common::assert_ok(
        &common::run_git_in(
            &tmp,
            &[
                "clone",
                bare_remote.to_str().unwrap(),
                clone_dir.to_str().unwrap(),
            ],
        ),
        "git clone",
    );

    let cloned_auth =
        std::fs::read(clone_dir.join("src/auth.rs")).expect("auth.rs should exist in clone");
    assert_eq!(
        cloned_auth, auth_content,
        "cloned auth.rs should be byte-identical"
    );

    let cloned_api =
        std::fs::read(clone_dir.join("src/api.rs")).expect("api.rs should exist in clone");
    assert_eq!(
        cloned_api, api_content,
        "cloned api.rs should be byte-identical"
    );

    let cloned_readme =
        std::fs::read(clone_dir.join("README.md")).expect("README.md should exist in clone");
    assert_eq!(
        cloned_readme, readme_content,
        "the cloned README.md should be byte-identical"
    );
}
