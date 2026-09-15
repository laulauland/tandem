//! Deterministic overlap of stacked edits at the real HTTP publication boundary.
use crate::common;
use jj_tandem_client::TandemClient;
use jj_tandem_workspace::{Daemon, DaemonOptions, SnapshotOutcome};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

struct Host {
    addr: String,
    token: String,
    remaining: Arc<AtomicUsize>,
    first_token: Arc<Mutex<String>>,
    runtime: Option<tokio::runtime::Runtime>,
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}
impl Host {
    fn start(root: &std::path::Path) -> Self {
        let token = jj_tandem_server::generate_admin_token();
        let server = jj_tandem_server::Server::new_with_faults_for_test(
            root.join("server"),
            Some(root.join("bucket").to_str().unwrap()),
            &token,
            jj_tandem_repository::FaultPoints::inert(),
        )
        .unwrap();
        server.durably_initialize().unwrap();
        let remaining = Arc::new(AtomicUsize::new(0));
        let gate = remaining.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first_token = Arc::new(Mutex::new(String::new()));
        let first = first_token.clone();
        let released = Arc::new(tokio::sync::Notify::new());
        let app = jj_tandem_server::router(Arc::new(server)).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let gate = gate.clone();
                let barrier = barrier.clone();
                let first = first.clone();
                let released = released.clone();
                async move {
                    if request.method() == axum::http::Method::POST
                        && matches!(request.uri().path(), "/api/publish" | "/api/heads")
                        && gate
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                            .is_ok()
                    {
                        let is_first = request
                            .headers()
                            .get("authorization")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            == format!("Bearer {}", first.lock().unwrap());
                        tokio::time::timeout(Duration::from_secs(15), barrier.wait())
                            .await
                            .expect("both prepared publications must reach the barrier");
                        if !is_first {
                            tokio::time::timeout(Duration::from_secs(15), released.notified())
                                .await
                                .unwrap();
                        }
                        let response = next.run(request).await;
                        if is_first {
                            assert_eq!(
                                response.status(),
                                axum::http::StatusCode::OK,
                                "selected first publisher must commit before releasing its peer"
                            );
                            released.notify_one();
                        }
                        return response;
                    }
                    next.run(request).await
                }
            },
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        runtime.spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr,
            token,
            remaining,
            first_token,
            runtime: Some(runtime),
        }
    }
}

struct Ack {
    commit_id: String,
    operation_id: String,
}

fn published(outcome: SnapshotOutcome) -> Ack {
    match outcome {
        SnapshotOutcome::Published(p) => Ack {
            commit_id: p.commit_id,
            operation_id: p.operation_id,
        },
        other => panic!("expected publication, got {other:?}"),
    }
}

fn overlap(rebase: bool, a_first: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let home = common::isolated_home(temporary.path());
    let host = Host::start(temporary.path());
    let admin = TandemClient::connect_with_cache(&host.addr, &host.token, &[], None).unwrap();
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut roots = Vec::new();
    for name in ["a", "b"] {
        let token = admin
            .mint_workspace_token(name, Some(600))
            .unwrap()
            .unwrap()
            .token;
        if (name == "a") == a_first {
            *host.first_token.lock().unwrap() = token.clone();
        }
        let root = temporary.path().join(name);
        std::fs::create_dir(&root).unwrap();
        common::assert_ok(
            &common::run_tandem_in_with_env(
                &root,
                &["init", "--server", &host.addr, "--workspace", name, "."],
                &[("TANDEM_TOKEN", &token)],
                &home,
            ),
            "initialize scoped workspace",
        );
        roots.push(root);
    }
    let (a, b) = (&roots[0], &roots[1]);
    let mut da = Daemon::open(&settings, &DaemonOptions::new(a)).unwrap();
    std::fs::write(a.join("a.txt"), b"A original\n").unwrap();
    let first = published(da.snapshot_once().unwrap());
    common::assert_ok(
        &common::run_tandem_in(b, &["new", &first.commit_id], &home),
        "stack B on A",
    );
    std::fs::write(b.join("b.txt"), b"B original\0\xff").unwrap();
    let mut db = Daemon::open(&settings, &DaemonOptions::new(b)).unwrap();
    published(db.snapshot_once().unwrap());
    std::fs::write(a.join("a.txt"), b"A overlapping rewrite\n").unwrap();
    if !rebase {
        std::fs::write(b.join("b.txt"), b"B overlapping snapshot\0\xff").unwrap();
    }
    host.remaining.store(2, Ordering::Release);
    let (a_ack, b_ack) = std::thread::scope(|scope| {
        let a_task = scope.spawn(|| published(da.snapshot_once().unwrap()));
        let b_task = scope.spawn(|| {
            if rebase {
                common::assert_ok(
                    &common::run_tandem_in(
                        b,
                        &["rebase", "--ignore-working-copy", "-r", "@", "-d", "root()"],
                        &home,
                    ),
                    "B explicitly rebases during A publication",
                );
                None
            } else {
                Some(published(db.snapshot_once().unwrap()))
            }
        });
        (a_task.join().unwrap(), b_task.join().unwrap())
    });
    assert_eq!(host.remaining.load(Ordering::Acquire), 0);
    if !rebase {
        assert!(matches!(
            db.snapshot_once().unwrap(),
            SnapshotOutcome::Stale
        ));
        assert_eq!(
            std::fs::read(b.join("b.txt")).unwrap(),
            b"B overlapping snapshot\0\xff"
        );
    }
    // Read the authoritative heads before any observer command can create an operation.
    use prost::Message as _;
    let state = admin.get_heads_state().unwrap();
    use jj_lib::object_id::ObjectId as _;
    let mut diagnosis = String::new();
    for head in &state.heads {
        let op = jj_lib::protos::simple_op_store::Operation::decode(
            admin.get_operation(head).unwrap().as_slice(),
        )
        .unwrap();
        let view = jj_tandem_jj::proto_convert::view_from_proto(
            jj_lib::protos::simple_op_store::View::decode(
                admin.get_view(&op.view_id).unwrap().as_slice(),
            )
            .unwrap(),
        )
        .unwrap();
        diagnosis.push_str(&format!("op={}\n", jj_tandem_protocol::hex::to_hex(head)));
        for (name, id) in &view.wc_commit_ids {
            let commit = jj_lib::protos::simple_store::Commit::decode(
                admin
                    .get_object(jj_tandem_protocol::wire::KIND_COMMIT, id.as_bytes())
                    .unwrap()
                    .as_slice(),
            )
            .unwrap();
            diagnosis.push_str(&format!(
                " workspace={name:?} commit={} change={} parents={:?} predecessors={:?} heads={:?}\n",
                id.hex(),
                jj_tandem_protocol::hex::to_hex(&commit.change_id),
                commit
                    .parents
                    .iter()
                    .map(|id| jj_tandem_protocol::hex::to_hex(id))
                    .collect::<Vec<_>>(),
                commit
                    .predecessors
                    .iter()
                    .map(|id| jj_tandem_protocol::hex::to_hex(id))
                    .collect::<Vec<_>>(),
                view.head_ids.iter().map(|id| id.hex()).collect::<Vec<_>>()
            ));
        }
    }
    let mut queue = state.heads.clone();
    let mut reachable = std::collections::BTreeSet::new();
    while let Some(id) = queue.pop() {
        if id == admin.repo_info().root_operation_id || !reachable.insert(id.clone()) {
            continue;
        }
        let operation = jj_lib::protos::simple_op_store::Operation::decode(
            admin.get_operation(&id).unwrap().as_slice(),
        )
        .unwrap();
        queue.extend(operation.parents);
    }
    for ack in std::iter::once(&a_ack).chain(b_ack.as_ref()) {
        assert!(
            reachable.contains(&jj_tandem_protocol::hex::from_hex(&ack.operation_id).unwrap()),
            "acknowledged operation remains reachable"
        );
    }

    assert_eq!(
        std::fs::read(a.join("a.txt")).unwrap(),
        b"A overlapping rewrite\n"
    );
    assert_eq!(std::fs::read(b.join("a.txt")).unwrap(), b"A original\n");
    let b_bytes: &[u8] = if rebase {
        b"B original\0\xff"
    } else {
        b"B overlapping snapshot\0\xff"
    };
    assert_eq!(std::fs::read(b.join("b.txt")).unwrap(), b_bytes);
    for (root, revision, path, expected) in [(
        a,
        a_ack.commit_id.as_str(),
        "a.txt",
        b"A overlapping rewrite\n".as_slice(),
    )]
    .into_iter()
    .chain(
        b_ack
            .as_ref()
            .map(|ack| (b, ack.commit_id.as_str(), "b.txt", b_bytes)),
    ) {
        let read = common::run_tandem_in_with_env(
            root,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                revision,
                path,
            ],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &home,
        );
        common::assert_ok(&read, "read exact acknowledged bytes");
        assert_eq!(read.stdout, expected);
    }
    if rebase {
        let operation_id = &state.workspace_heads["b"];
        assert!(reachable.contains(operation_id));
        let operation = jj_lib::protos::simple_op_store::Operation::decode(
            admin.get_operation(operation_id).unwrap().as_slice(),
        )
        .unwrap();
        let view = jj_tandem_jj::proto_convert::view_from_proto(
            jj_lib::protos::simple_op_store::View::decode(
                admin.get_view(&operation.view_id).unwrap().as_slice(),
            )
            .unwrap(),
        )
        .unwrap();
        use jj_lib::object_id::ObjectId as _;
        let commit_id = &view.wc_commit_ids[&jj_lib::ref_name::WorkspaceNameBuf::from("b")];
        let commit = jj_lib::protos::simple_store::Commit::decode(
            admin
                .get_object(jj_tandem_protocol::wire::KIND_COMMIT, commit_id.as_bytes())
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(
            commit.parents,
            vec![admin.repo_info().root_commit_id.clone()]
        );
        let read = common::run_tandem_in_with_env(
            b,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                &commit_id.hex(),
                "b.txt",
            ],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &home,
        );
        common::assert_ok(&read, "read explicitly rebased B revision");
        assert_eq!(read.stdout, b_bytes);
    }
    if let Some(b_ack) = &b_ack {
        let b_commit = jj_lib::protos::simple_store::Commit::decode(
            admin
                .get_object(
                    jj_tandem_protocol::wire::KIND_COMMIT,
                    &jj_tandem_protocol::hex::from_hex(&b_ack.commit_id).unwrap(),
                )
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let mut commits = Vec::new();
        for head in &state.heads {
            let op = jj_lib::protos::simple_op_store::Operation::decode(
                admin.get_operation(head).unwrap().as_slice(),
            )
            .unwrap();
            let view = jj_tandem_jj::proto_convert::view_from_proto(
                jj_lib::protos::simple_op_store::View::decode(
                    admin.get_view(&op.view_id).unwrap().as_slice(),
                )
                .unwrap(),
            )
            .unwrap();
            commits.extend(view.head_ids.iter().map(|id| id.as_bytes().to_vec()));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut found_original = false;
        let mut found_new = false;
        while let Some(id) = commits.pop() {
            if id == admin.repo_info().root_commit_id || !seen.insert(id.clone()) {
                continue;
            }
            let commit = jj_lib::protos::simple_store::Commit::decode(
                admin
                    .get_object(jj_tandem_protocol::wire::KIND_COMMIT, &id)
                    .unwrap()
                    .as_slice(),
            )
            .unwrap();
            commits.extend(commit.parents.iter().cloned());
            if commit.change_id != b_commit.change_id {
                continue;
            }
            let revision = jj_tandem_protocol::hex::to_hex(&id);
            let read = common::run_tandem_in_with_env(
                b,
                &[
                    "file",
                    "show",
                    "--ignore-working-copy",
                    "-r",
                    &revision,
                    "b.txt",
                ],
                &[("TANDEM_DISABLE_CACHE", "1")],
                &home,
            );
            common::assert_ok(&read, "read a live divergent B revision");
            if read.stdout == b"B original\0\xff" {
                found_original = true;
                assert_eq!(
                    commit.parents,
                    vec![jj_tandem_protocol::hex::from_hex(&a_ack.commit_id).unwrap()]
                );
                let inherited = common::run_tandem_in_with_env(
                    b,
                    &[
                        "file",
                        "show",
                        "--ignore-working-copy",
                        "-r",
                        &revision,
                        "a.txt",
                    ],
                    &[("TANDEM_DISABLE_CACHE", "1")],
                    &home,
                );
                common::assert_ok(&inherited, "read A rewrite inherited by divergent B");
                assert_eq!(inherited.stdout, b"A overlapping rewrite\n");
            } else {
                assert_eq!(read.stdout, b"B overlapping snapshot\0\xff");
                found_new = true;
            }
        }
        assert!(
            found_original && found_new,
            "both divergent B versions must remain in current commit history: {diagnosis}"
        );
    }
    // Only the explicit user command may refresh B's directory after remote rewrites.
    let refresh = common::run_tandem_in(b, &["workspace", "update-stale"], &home);
    common::assert_ok(&refresh, "B explicitly refreshes its workspace");
    let refreshed = std::fs::read(b.join("b.txt")).unwrap();
    if refreshed != b_bytes {
        assert!(!rebase);
        assert_eq!(refreshed, b"B original\0\xff");
        assert!(String::from_utf8_lossy(&refresh.stderr).contains("divergent"));
    }
}

#[test]
fn stacked_prepared_snapshots_a_first() {
    overlap(false, true);
}
#[test]
fn stacked_prepared_snapshots_b_first() {
    overlap(false, false);
}
#[test]
fn stacked_prepared_snapshot_overlaps_explicit_b_rebase() {
    overlap(true, true);
    overlap(true, false);
}
