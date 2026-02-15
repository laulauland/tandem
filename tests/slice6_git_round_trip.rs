mod common;

use tempfile::TempDir;

/// Slice 6: Git round-trip — verify that files written through tandem
/// are stored as real git objects in the server repo.
///
/// After an agent writes src/feature.rs via tandem-backed jj, the server
/// repo should be a real jj+git repo where:
/// 1. `jj log` on the server shows the commit
/// 2. `jj file show` on the server returns the file bytes
/// 3. `git log` on the server shows the commit in git
/// 4. `git show HEAD:src/feature.rs` returns the file bytes
#[test]
fn slice6_git_round_trip_server_has_real_git_objects() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Verify server created a git repo
    assert!(
        server_repo.join(".git").exists(),
        "server should create a .git directory"
    );
    assert!(
        server_repo.join(".jj").exists(),
        "server should create a .jj directory"
    );

    // Initialize tandem workspace
    let init = common::run_tandem_in(
        &workspace_dir,
        &["init", "--tandem-server", &addr, "."],
        &home,
    );
    common::assert_ok(&init, "tandem init");

    // Write a file with distinctive content
    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let file_content = b"pub fn feature() -> &'static str {\n    \"tandem git round-trip\"\n}\n";
    std::fs::write(src_dir.join("feature.rs"), file_content).unwrap();

    // Describe the current working copy commit (which has the files)
    let desc_out = common::run_tandem_in(
        &workspace_dir,
        &["describe", "-m", "add feature module"],
        &home,
    );
    common::assert_ok(&desc_out, "jj describe");

    // Create a new empty change on top, making the described commit @-
    let new_out = common::run_tandem_in(&workspace_dir, &["new"], &home);
    common::assert_ok(&new_out, "jj new");

    // Verify file is readable from the client workspace
    let cat = common::run_tandem_in(
        &workspace_dir,
        &["file", "show", "-r", "@-", "src/feature.rs"],
        &home,
    );
    common::assert_ok(&cat, "client file show");
    assert_eq!(
        cat.stdout, file_content,
        "client should read back the exact file bytes"
    );

    // ── Server-side verification: jj commands ─────────────────────
    // Run jj commands directly on the server repo to verify it's a real jj repo

    // --ignore-working-copy is needed because the server's working copy
    // is stale (tandem client wrote operations the server workspace doesn't track)
    let server_log = common::run_tandem_in_with_env(
        &server_repo,
        &["log", "--ignore-working-copy", "--no-graph", "-n", "10"],
        &[],
        &home,
    );
    common::assert_ok(&server_log, "server jj log");
    let log_text = String::from_utf8_lossy(&server_log.stdout);
    assert!(
        log_text.contains("add feature module"),
        "server jj log should show the commit description\nactual log:\n{log_text}"
    );

    // Find the commit ID for the "add feature module" commit on the server
    // Use template to get just the commit ID
    // First, find the commit on the client side (we know this works)
    let client_log_ids = common::run_tandem_in(
        &workspace_dir,
        &["log", "--no-graph", "-r", "@-", "-T", "commit_id ++ \"\\n\""],
        &home,
    );
    common::assert_ok(&client_log_ids, "client jj log for commit id");
    let commit_id = String::from_utf8_lossy(&client_log_ids.stdout)
        .trim()
        .to_string();
    assert!(
        !commit_id.is_empty(),
        "should find the commit on client side"
    );

    // Now verify it exists on the server
    let server_log_ids = common::run_tandem_in_with_env(
        &server_repo,
        &[
            "log", "--ignore-working-copy",
            "--no-graph",
            "-r", &commit_id,
            "-T", "description",
        ],
        &[],
        &home,
    );
    common::assert_ok(&server_log_ids, "server jj log with commit id");
    let server_desc = String::from_utf8_lossy(&server_log_ids.stdout);
    assert!(
        server_desc.contains("add feature module"),
        "server should see the commit description\nactual: {server_desc}"
    );

    // Read the file from the server repo using jj
    let server_cat = common::run_tandem_in_with_env(
        &server_repo,
        &["file", "show", "--ignore-working-copy", "-r", &commit_id, "src/feature.rs"],
        &[],
        &home,
    );
    common::assert_ok(&server_cat, "server jj file show");
    assert_eq!(
        server_cat.stdout, file_content,
        "server jj file show should return exact file bytes"
    );

    // ── Server-side verification: git commands ────────────────────
    // Verify the git repo contains the objects

    // Use git log to see commits
    let git_log = common::run_git_in(&server_repo, &["log", "--oneline", "--all"]);
    common::assert_ok(&git_log, "git log");
    let git_log_text = String::from_utf8_lossy(&git_log.stdout);
    // Git should have at least one commit (the "add feature module" commit)
    assert!(
        !git_log_text.trim().is_empty(),
        "git log should show commits\nactual:\n{git_log_text}"
    );

    // ── Git push round-trip ───────────────────────────────────────
    // Create a bare git remote, push to it, verify content

    let bare_remote = tmp.path().join("bare-remote.git");
    let git_init_bare = common::run_git_in(
        tmp.path(),
        &["init", "--bare", bare_remote.to_str().unwrap()],
    );
    common::assert_ok(&git_init_bare, "git init --bare");

    // Add the bare repo as a remote on the server repo's git
    let git_add_remote = common::run_git_in(
        &server_repo,
        &["remote", "add", "origin", bare_remote.to_str().unwrap()],
    );
    common::assert_ok(&git_add_remote, "git remote add");

    // Create a bookmark on the server repo pointing to the commit
    let bookmark_create = common::run_tandem_in_with_env(
        &server_repo,
        &["bookmark", "create", "--ignore-working-copy", "main", "-r", &commit_id],
        &[],
        &home,
    );
    common::assert_ok(&bookmark_create, "jj bookmark create");

    // Push to git remote
    let git_push = common::run_tandem_in_with_env(
        &server_repo,
        &["git", "push", "--ignore-working-copy", "--bookmark", "main"],
        &[],
        &home,
    );
    common::assert_ok(&git_push, "jj git push");

    // Clone the bare remote and verify file content
    let clone_dir = tmp.path().join("clone");
    let git_clone = common::run_git_in(
        tmp.path(),
        &["clone", bare_remote.to_str().unwrap(), clone_dir.to_str().unwrap()],
    );
    common::assert_ok(&git_clone, "git clone");

    // Verify the file exists in the clone with correct content
    let cloned_content = std::fs::read(clone_dir.join("src/feature.rs"))
        .expect("feature.rs should exist in clone");
    assert_eq!(
        cloned_content, file_content,
        "cloned file should have exact same bytes"
    );

    let _ = server.kill();
    let _ = server.wait();
}

/// Verify that multiple files survive the git round-trip.
#[test]
fn slice6_multiple_files_git_round_trip() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    // Initialize workspace
    let init = common::run_tandem_in(
        &workspace_dir,
        &["init", "--tandem-server", &addr, "."],
        &home,
    );
    common::assert_ok(&init, "tandem init");

    // Write multiple files
    let src_dir = workspace_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    let auth_content = b"pub mod auth {\n    pub fn login() {}\n}\n";
    let api_content = b"pub mod api {\n    pub fn handler() {}\n}\n";
    let readme_content = b"# My Project\n\nA project built with tandem.\n";

    std::fs::write(src_dir.join("auth.rs"), auth_content).unwrap();
    std::fs::write(src_dir.join("api.rs"), api_content).unwrap();
    std::fs::write(workspace_dir.join("README.md"), readme_content).unwrap();

    // Describe the working copy commit with the files, then create new
    let desc_out = common::run_tandem_in(
        &workspace_dir,
        &["describe", "-m", "add multiple files"],
        &home,
    );
    common::assert_ok(&desc_out, "jj describe");
    let new_out = common::run_tandem_in(&workspace_dir, &["new"], &home);
    common::assert_ok(&new_out, "jj new");

    // Verify all files via client
    for (path, expected) in [
        ("src/auth.rs", &auth_content[..]),
        ("src/api.rs", &api_content[..]),
        ("README.md", &readme_content[..]),
    ] {
        let cat = common::run_tandem_in(
            &workspace_dir,
            &["file", "show", "-r", "@-", path],
            &home,
        );
        common::assert_ok(&cat, &format!("client file show {path}"));
        assert_eq!(cat.stdout, expected, "file {path} content mismatch via client");
    }

    // Verify all files via server jj
    // Get commit ID from the client side (same approach as first test)
    let client_log_ids = common::run_tandem_in(
        &workspace_dir,
        &["log", "--no-graph", "-r", "@-", "-T", "commit_id ++ \"\\n\""],
        &home,
    );
    common::assert_ok(&client_log_ids, "client jj log for commit id");
    let commit_id = String::from_utf8_lossy(&client_log_ids.stdout)
        .trim()
        .to_string();
    assert!(!commit_id.is_empty(), "should find commit from client");

    for (path, expected) in [
        ("src/auth.rs", &auth_content[..]),
        ("src/api.rs", &api_content[..]),
        ("README.md", &readme_content[..]),
    ] {
        let cat = common::run_tandem_in_with_env(
            &server_repo,
            &["file", "show", "--ignore-working-copy", "-r", &commit_id, path],
            &[],
            &home,
        );
        common::assert_ok(&cat, &format!("server file show {path}"));
        assert_eq!(cat.stdout, expected, "file {path} content mismatch via server");
    }

    // Git push and verify
    let bare_remote = tmp.path().join("bare-remote.git");
    common::assert_ok(
        &common::run_git_in(tmp.path(), &["init", "--bare", bare_remote.to_str().unwrap()]),
        "git init --bare",
    );
    common::assert_ok(
        &common::run_git_in(
            &server_repo,
            &["remote", "add", "origin", bare_remote.to_str().unwrap()],
        ),
        "git remote add",
    );
    common::assert_ok(
        &common::run_tandem_in_with_env(
            &server_repo,
            &["bookmark", "create", "--ignore-working-copy", "main", "-r", &commit_id],
            &[],
            &home,
        ),
        "jj bookmark create",
    );
    common::assert_ok(
        &common::run_tandem_in_with_env(
            &server_repo,
            &["git", "push", "--ignore-working-copy", "--bookmark", "main"],
            &[],
            &home,
        ),
        "jj git push",
    );

    // Clone and verify all files
    let clone_dir = tmp.path().join("clone");
    common::assert_ok(
        &common::run_git_in(
            tmp.path(),
            &["clone", bare_remote.to_str().unwrap(), clone_dir.to_str().unwrap()],
        ),
        "git clone",
    );

    for (path, expected) in [
        ("src/auth.rs", &auth_content[..]),
        ("src/api.rs", &api_content[..]),
        ("README.md", &readme_content[..]),
    ] {
        let cloned = std::fs::read(clone_dir.join(path))
            .unwrap_or_else(|_| panic!("{path} should exist in clone"));
        assert_eq!(cloned, expected, "cloned {path} content mismatch");
    }

    let _ = server.kill();
    let _ = server.wait();
}
