//! Setting a workspace up, and what it inherits when it starts.
//!
//! Three things a subprocess is the only honest way to check: that the binary
//! writes a `.jj` a real `jj` can read, that two inits with no `--workspace`
//! do not collide, and that a new workspace lands where the server's default
//! workspace already is rather than back at the root commit.

use crate::common;
use crate::common::workspace::first_commit_id;
use crate::common::ServerFixture;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Output;

#[test]
fn single_agent_file_round_trip() {
    let mut fx = ServerFixture::start();
    let home = fx.home.clone();
    let workspace_dir = fx.init_workspace("workspace", None);

    // Verify .jj structure was created
    assert!(workspace_dir.join(".jj").exists(), ".jj dir should exist");
    let store_type = std::fs::read_to_string(workspace_dir.join(".jj/repo/store/type")).unwrap();
    assert_eq!(store_type.trim(), "tandem", "store type should be tandem");

    // Write a file
    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_content = b"fn main() { println!(\"hello tandem\"); }\n";
    std::fs::write(src_dir.join("hello.rs"), file_content).unwrap();

    // Create a commit: jj new creates a new empty change, making the previous
    // working copy (with the file) become @-.
    let new_out = common::run_tandem_in(&workspace_dir, &["new", "-m", "add hello"], &home);
    common::assert_ok(&new_out, "jj new");

    // Check log
    let log = common::run_tandem_in(&workspace_dir, &["log", "--no-graph", "-n", "5"], &home);
    common::assert_ok(&log, "jj log");
    let log_text = String::from_utf8_lossy(&log.stdout);
    assert!(
        log_text.contains("add hello"),
        "log should show commit description\n{log_text}"
    );

    // Check file show (read file from parent commit)
    let cat = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "src/hello.rs"],
        &home,
    );
    common::assert_ok(&cat, "jj file show");
    assert_eq!(
        cat.stdout, file_content,
        "file show should return exact file bytes"
    );

    // Check diff
    let diff = common::run_tandem_in(&workspace_dir, &["diff", "-r", "@-"], &home);
    common::assert_ok(&diff, "jj diff");
    let diff_text = String::from_utf8_lossy(&diff.stdout);
    assert!(
        diff_text.contains("hello.rs"),
        "diff should mention hello.rs\n{diff_text}"
    );

    // ── Server restart ────────────────────────────────────────────────
    fx.restart_on_new_addr();

    // After restart, use TANDEM_SERVER env to point to new address
    let log2 = common::run_tandem_in_with_env(
        &workspace_dir,
        &["log", "--no-graph", "-n", "5"],
        &[("TANDEM_SERVER", &fx.addr)],
        &home,
    );
    common::assert_ok(&log2, "jj log after restart");
    let log2_text = String::from_utf8_lossy(&log2.stdout);
    assert!(
        log2_text.contains("add hello"),
        "log after restart\n{log2_text}"
    );

    let cat2 = common::run_tandem_in_with_env(
        &workspace_dir,
        &["file", "show", "-r", "@-", "src/hello.rs"],
        &[("TANDEM_SERVER", &fx.addr)],
        &home,
    );
    common::assert_ok(&cat2, "jj file show after restart");
    assert_eq!(
        cat2.stdout, file_content,
        "file show after restart should return exact bytes"
    );
}

fn parse_workspace_name_from_init(output: &Output) -> String {
    let stderr = common::stderr_str(output);
    let prefix = "Initialized tandem workspace '";
    let start = stderr
        .find(prefix)
        .unwrap_or_else(|| panic!("init stderr missing workspace message:\n{stderr}"));
    let rest = &stderr[start + prefix.len()..];
    let end = rest
        .find('\'')
        .unwrap_or_else(|| panic!("init stderr missing closing quote:\n{stderr}"));
    rest[..end].to_string()
}

fn workspace_heads_keys(server_repo: &Path) -> BTreeSet<String> {
    let heads_path = server_repo.join(".jj/repo/tandem/heads.json");
    let text = std::fs::read_to_string(&heads_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", heads_path.display()));
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parse {} as JSON: {e}\n{text}", heads_path.display()));

    let workspace_heads = parsed
        .get("workspaceHeads")
        .or_else(|| parsed.get("workspace_heads"))
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| {
            panic!(
                "{} missing workspace heads map (workspaceHeads/workspace_heads)\n{text}",
                heads_path.display()
            )
        });

    workspace_heads.keys().cloned().collect()
}

#[test]
fn implicit_workspace_names_are_unique_and_tracked() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();
    let server_repo = fx.repo.clone();

    let ws_a_dir = fx.dir("agent-a");
    let ws_b_dir = fx.dir("agent-b");

    // Init A without --workspace -> should auto-generate non-default name.
    let init_a = common::run_tandem_in(
        &ws_a_dir,
        &["init", "--server", &fx.addr, "--token", fx.token(), "."],
        &home,
    );
    common::assert_ok(&init_a, "workspace A init (implicit workspace)");
    let ws_a_name = parse_workspace_name_from_init(&init_a);
    assert_ne!(
        ws_a_name, "default",
        "implicit workspace name for A should not be literal 'default'"
    );

    // A commits a file.
    let a_bytes = b"pub fn from_a() -> &'static str { \"A\" }\n";
    std::fs::create_dir_all(ws_a_dir.join("src")).unwrap();
    std::fs::write(ws_a_dir.join("src/a.rs"), a_bytes).unwrap();

    let describe_a = common::run_tandem_in(&ws_a_dir, &["describe", "-m", "A adds a.rs"], &home);
    common::assert_ok(&describe_a, "workspace A describe");
    let new_a = common::run_tandem_in(&ws_a_dir, &["new"], &home);
    common::assert_ok(&new_a, "workspace A new");

    let change_a = common::run_tandem_in(
        &ws_a_dir,
        &["log", "-r", "@-", "--no-graph", "-T", "change_id"],
        &home,
    );
    common::assert_ok(&change_a, "workspace A get change id");
    let change_a_id = common::stdout_str(&change_a).trim().to_string();
    assert!(
        !change_a_id.is_empty(),
        "workspace A change id should exist"
    );

    // Init B without --workspace after A has committed.
    let init_b = common::run_tandem_in(
        &ws_b_dir,
        &["init", "--server", &fx.addr, "--token", fx.token(), "."],
        &home,
    );
    common::assert_ok(&init_b, "workspace B init (implicit workspace)");
    let ws_b_name = parse_workspace_name_from_init(&init_b);
    assert_ne!(
        ws_b_name, "default",
        "implicit workspace name for B should not be literal 'default'"
    );
    assert_ne!(
        ws_a_name, ws_b_name,
        "implicit workspace names must be unique across directories"
    );

    // B can log without stale-working-copy collision failure.
    let log_b = common::run_tandem_in(&ws_b_dir, &["log", "--no-graph", "-n", "20"], &home);
    common::assert_ok(&log_b, "workspace B log after A commit");
    let log_b_err = common::stderr_str(&log_b).to_lowercase();
    assert!(
        !log_b_err.contains("working copy is stale"),
        "workspace B log should not fail via stale working copy collision\nstderr:\n{}",
        common::stderr_str(&log_b)
    );

    // B can read exact bytes from A's commit.
    let cat_a_from_b = common::run_tandem_in(
        &ws_b_dir,
        &["file", "show", "-r", &change_a_id, "src/a.rs"],
        &home,
    );
    common::assert_ok(&cat_a_from_b, "workspace B reads A file bytes");
    assert_eq!(
        cat_a_from_b.stdout, a_bytes,
        "workspace B should get exact bytes for A's src/a.rs"
    );

    // B commits its own file.
    let b_bytes = b"pub fn from_b() -> &'static str { \"B\" }\n";
    std::fs::create_dir_all(ws_b_dir.join("src")).unwrap();
    std::fs::write(ws_b_dir.join("src/b.rs"), b_bytes).unwrap();

    let describe_b = common::run_tandem_in(&ws_b_dir, &["describe", "-m", "B adds b.rs"], &home);
    common::assert_ok(&describe_b, "workspace B describe");
    let new_b = common::run_tandem_in(&ws_b_dir, &["new"], &home);
    common::assert_ok(&new_b, "workspace B new");

    let change_b = common::run_tandem_in(
        &ws_b_dir,
        &["log", "-r", "@-", "--no-graph", "-T", "change_id"],
        &home,
    );
    common::assert_ok(&change_b, "workspace B get change id");
    let change_b_id = common::stdout_str(&change_b).trim().to_string();
    assert!(
        !change_b_id.is_empty(),
        "workspace B change id should exist"
    );

    // A can read exact bytes from B's commit.
    let cat_b_from_a = common::run_tandem_in(
        &ws_a_dir,
        &["file", "show", "-r", &change_b_id, "src/b.rs"],
        &home,
    );
    common::assert_ok(&cat_b_from_a, "workspace A reads B file bytes");
    assert_eq!(
        cat_b_from_a.stdout, b_bytes,
        "workspace A should get exact bytes for B's src/b.rs"
    );

    // Server workspace_heads map should include both implicit workspace names.
    let workspace_heads = workspace_heads_keys(&server_repo);
    assert!(
        workspace_heads.contains(&ws_a_name),
        "workspace_heads should include workspace A name '{ws_a_name}', keys={workspace_heads:?}"
    );
    assert!(
        workspace_heads.contains(&ws_b_name),
        "workspace_heads should include workspace B name '{ws_b_name}', keys={workspace_heads:?}"
    );
}

#[test]
fn init_uses_server_workspace_parent_context_by_default() {
    let fx = ServerFixture::start();
    let home = fx.home.clone();
    let server_repo = fx.repo.clone();

    // Seed the server default workspace with a real commit.
    let seed_bytes = b"seed from server workspace\n";
    std::fs::write(server_repo.join("seed.txt"), seed_bytes).unwrap();

    let describe_server = common::run_tandem_in(
        &server_repo,
        &["describe", "-m", "seed server workspace"],
        &home,
    );
    common::assert_ok(&describe_server, "describe server seed commit");

    let new_server = common::run_tandem_in(&server_repo, &["new"], &home);
    common::assert_ok(&new_server, "new server seed commit");

    let expected_parent = common::run_tandem_in(
        &server_repo,
        &[
            "log",
            "-r",
            "@-",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        &home,
    );
    let expected_parent_id = first_commit_id(&expected_parent, "read server default @-");

    // Initialize a new remote workspace.
    let workspace_dir = fx.init_workspace("agent-workspace", Some("agent-a"));

    // New workspace @- should match server default @- (jj workspace-add style default).
    let actual_parent = common::run_tandem_in(
        &workspace_dir,
        &[
            "log",
            "-r",
            "@-",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        &home,
    );
    let actual_parent_id = first_commit_id(&actual_parent, "read new workspace @-");

    assert_eq!(
        actual_parent_id, expected_parent_id,
        "new workspace @- should match server default workspace parent target"
    );

    // Verify exact file bytes are present at @-.
    let cat_seed = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "seed.txt"],
        &home,
    );
    common::assert_ok(&cat_seed, "read seed.txt from new workspace @-");
    assert_eq!(
        cat_seed.stdout, seed_bytes,
        "new workspace should inherit seeded file bytes from server context"
    );
}
