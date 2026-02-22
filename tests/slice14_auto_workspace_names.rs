//! Slice 14: Auto workspace names avoid default collisions
//!
//! Acceptance criteria:
//! - `tandem init` without `--workspace` does not use `default`
//! - Two implicit inits produce distinct workspace names
//! - After workspace A commits, workspace B can run `tandem log` (no stale collision)
//! - After commits from both workspaces, server heads state tracks both workspace names

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Output;

use tempfile::TempDir;

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
fn v1_slice14_implicit_workspace_names_are_unique_and_tracked() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let ws_a_dir = tmp.path().join("agent-a");
    std::fs::create_dir_all(&ws_a_dir).unwrap();
    let ws_b_dir = tmp.path().join("agent-b");
    std::fs::create_dir_all(&ws_b_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Init A without --workspace -> should auto-generate non-default name.
    let init_a = common::run_tandem_in(&ws_a_dir, &["init", "--server", &addr, "."], &home);
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
    let init_b = common::run_tandem_in(&ws_b_dir, &["init", "--server", &addr, "."], &home);
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

    let _ = server.kill();
    let _ = server.wait();
}
