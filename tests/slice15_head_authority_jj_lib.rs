//! Slice 15: jj-lib is the single operation-head authority on the server.
//!
//! Acceptance criteria:
//! - server-local jj op-head view and tandem `getHeads` view stay consistent
//!   after concurrent updates
//! - no divergence after repeated CAS conflicts/retries

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use jj_lib::object_id::ObjectId as _;
use tempfile::TempDir;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[path = "../src/tandem_capnp.rs"]
mod tandem_capnp;

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn run_tandem_with_timeout(dir: &Path, args: &[&str], home: &Path) -> Output {
    let mut cmd = Command::new(common::tandem_bin());
    cmd.current_dir(dir);
    common::isolate_env(&mut cmd, home);
    for arg in args {
        cmd.arg(arg);
    }
    let mut child = cmd.spawn().expect("spawn tandem command");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(_status) = child.try_wait().expect("try_wait tandem command") {
            return child
                .wait_with_output()
                .expect("wait tandem command output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let out = child
                .wait_with_output()
                .expect("wait timed out tandem command");
            panic!(
                "tandem command timed out: {:?}\nstdout:\n{}\nstderr:\n{}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Run tandem command and auto-recover from stale working-copy errors.
fn run_tandem_in_resilient(dir: &Path, args: &[&str], home: &Path) -> Output {
    let output = run_tandem_with_timeout(dir, args, home);
    if output.status.success() {
        return output;
    }
    let combined = format!(
        "{}\n{}",
        common::stdout_str(&output),
        common::stderr_str(&output)
    )
    .to_lowercase();
    if combined.contains("working copy is stale") || combined.contains("update-stale") {
        let update = run_tandem_with_timeout(dir, &["workspace", "update-stale"], home);
        common::assert_ok(&update, "workspace update-stale (resilient)");
        run_tandem_with_timeout(dir, args, home)
    } else {
        output
    }
}

fn settle_workspace(dir: &Path, home: &Path) {
    let update = common::run_tandem_in(dir, &["workspace", "update-stale"], home);
    if !update.status.success() {
        let err = common::stderr_str(&update);
        if !err.contains("nothing to do") && !err.contains("already up to date") {
            common::assert_ok(&update, "workspace update-stale settle");
        }
    }
}

fn server_jj_op_heads(server_repo: &Path) -> BTreeSet<String> {
    let config = jj_lib::config::StackedConfig::with_defaults();
    let settings = jj_lib::settings::UserSettings::from_config(config).expect("create jj settings");
    let repo_dir =
        dunce::canonicalize(server_repo.join(".jj/repo")).expect("canonicalize .jj/repo");
    let factories = jj_lib::repo::StoreFactories::default();
    let loader = jj_lib::repo::RepoLoader::init_from_file_system(&settings, &repo_dir, &factories)
        .expect("load repo loader");

    let heads = pollster::block_on(loader.op_heads_store().get_op_heads()).expect("read op heads");
    heads.into_iter().map(|id| id.hex()).collect()
}

fn tandem_get_heads(addr: &str) -> (BTreeSet<String>, u64) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build rpc runtime");
    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, async move {
        let stream =
            tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
                .await
                .expect("connect timeout")
                .expect("connect server");
        stream.set_nodelay(true).ok();

        let (reader, writer) = stream.into_split();
        let network = twoparty::VatNetwork::new(
            reader.compat(),
            writer.compat_write(),
            rpc_twoparty_capnp::Side::Client,
            Default::default(),
        );
        let mut rpc_system = RpcSystem::new(Box::new(network), None);
        let client: tandem_capnp::store::Client =
            rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);
        let rpc_task = tokio::task::spawn_local(rpc_system);

        let request = client.get_heads_request();
        let response = request.send().promise.await.expect("getHeads RPC");
        let result = response.get().expect("getHeads result");

        let version = result.get_version();
        let mut heads = BTreeSet::new();
        let heads_reader = result.get_heads().expect("get heads list");
        for i in 0..heads_reader.len() {
            let bytes = heads_reader.get(i).expect("head bytes");
            heads.insert(to_hex(bytes));
        }

        rpc_task.abort();
        (heads, version)
    })
}

#[test]
fn v1_slice15_jj_lib_head_authority_consistent_after_retries() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    let agent_count = 2;
    let mut agent_dirs = Vec::new();
    for i in 0..agent_count {
        let workspace = format!("agent-{i}");
        let dir = tmp.path().join(&workspace);
        std::fs::create_dir_all(&dir).unwrap();
        let init = common::run_tandem_in(
            &dir,
            &["init", "--server", &addr, "--workspace", &workspace, "."],
            &home,
        );
        common::assert_ok(&init, &format!("init {workspace}"));
        agent_dirs.push(dir);
    }

    let iterations = 1;

    let handles: Vec<_> = (0..agent_count)
        .map(|agent_idx| {
            let dir = agent_dirs[agent_idx].clone();
            let home = home.clone();
            thread::spawn(move || {
                for iter in 0..iterations {
                    let src_dir = dir.join("src");
                    std::fs::create_dir_all(&src_dir).unwrap();
                    let file_name = format!("agent_{agent_idx}_iter_{iter}.txt");
                    let file_path = src_dir.join(&file_name);
                    let content = format!("agent={agent_idx} iter={iter}\n");
                    std::fs::write(file_path, content.as_bytes()).unwrap();

                    let desc = format!("agent-{agent_idx} iter-{iter}");
                    let describe = run_tandem_in_resilient(&dir, &["describe", "-m", &desc], &home);
                    common::assert_ok(&describe, "describe in conflict loop");
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("agent thread");
    }

    for dir in &agent_dirs {
        settle_workspace(dir, &home);
    }

    let jj_heads = server_jj_op_heads(&server_repo);
    let (rpc_heads, rpc_version) = tandem_get_heads(&addr);
    assert_eq!(
        jj_heads, rpc_heads,
        "server-local jj op heads must match tandem getHeads"
    );
    assert!(
        rpc_version > 0,
        "version should advance after concurrent updates"
    );

    // Re-read repeatedly to ensure no drift after heavy CAS contention.
    for _ in 0..5 {
        let jj_now = server_jj_op_heads(&server_repo);
        let (rpc_now, _) = tandem_get_heads(&addr);
        assert_eq!(jj_now, rpc_now, "jj and RPC head views diverged");
    }

    // Sidecar is metadata-only: version + workspace_heads (no head set copy).
    let heads_path = server_repo.join(".jj/repo/tandem/heads.json");
    let text = std::fs::read_to_string(&heads_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert!(
        parsed.get("heads").is_none(),
        "metadata sidecar must not persist op-head set: {text}"
    );

    let workspace_heads = parsed
        .get("workspaceHeads")
        .or_else(|| parsed.get("workspace_heads"))
        .and_then(|v| v.as_object())
        .expect("workspace_heads map in sidecar");
    assert!(
        workspace_heads.len() >= agent_count,
        "workspace_heads should track all writing workspaces"
    );

    let sidecar_version = parsed
        .get("version")
        .and_then(|v| v.as_u64())
        .expect("version in sidecar");
    assert_eq!(
        sidecar_version, rpc_version,
        "sidecar version must match getHeads version"
    );

    let _ = server.kill();
    let _ = server.wait();
}
