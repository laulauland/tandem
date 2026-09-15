//! Lease loss after scanning, while native jj rebases a remote descendant.
use crate::common;
use jj_tandem_client::TandemClient;
use jj_tandem_workspace::{Daemon, DaemonOptions, SnapshotOutcome};
use prost::Message as _;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Gate {
    path: Mutex<Option<String>>,
    release: tokio::sync::Notify,
    reject: AtomicUsize,
}

struct Host {
    addr: String,
    token: String,
    runtime: Option<tokio::runtime::Runtime>,
}
impl Host {
    fn start(
        root: &std::path::Path,
        addr: &str,
        token: &str,
        gate: Arc<Gate>,
        entered: std::sync::mpsc::Sender<()>,
        claims: std::sync::mpsc::Sender<u16>,
    ) -> Self {
        let server = jj_tandem_server::Server::new_with_faults_for_test(
            root.join("server"),
            Some(root.join("bucket").to_str().unwrap()),
            token,
            jj_tandem_repository::FaultPoints::inert(),
        )
        .unwrap();
        server.durably_initialize().unwrap();
        let app = jj_tandem_server::router(Arc::new(server)).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let gate = gate.clone();
                let entered = entered.clone();
                let claims = claims.clone();
                async move {
                    let block = {
                        let mut path = gate.path.lock().unwrap();
                        if request.method() == axum::http::Method::GET
                            && path.as_deref() == Some(request.uri().path())
                        {
                            path.take();
                            true
                        } else {
                            false
                        }
                    };
                    if block {
                        let _ = entered.send(());
                        tokio::time::timeout(WAIT, gate.release.notified())
                            .await
                            .expect("release descendant read");
                    }
                    if request.uri().path() == "/api/workspaces/a/writer" {
                        if gate
                            .reject
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                            .is_ok()
                        {
                            let _ = claims.send(503);
                            return axum::response::IntoResponse::into_response(
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            );
                        }
                        let response = next.run(request).await;
                        let _ = claims.send(response.status().as_u16());
                        return response;
                    }
                    next.run(request).await
                }
            },
        ));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(addr))
            .unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        runtime.spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr,
            token: token.into(),
            runtime: Some(runtime),
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

#[test]
fn lease_loss_during_descendant_rebase_aborts_and_same_daemon_retries() {
    let temporary = tempfile::tempdir().unwrap();
    let home = common::isolated_home(temporary.path());
    let gate = Arc::new(Gate::default());
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (claims_tx, claims_rx) = std::sync::mpsc::channel();
    let token = jj_tandem_server::generate_admin_token();
    let host = Host::start(
        temporary.path(),
        "127.0.0.1:0",
        &token,
        gate.clone(),
        entered_tx.clone(),
        claims_tx.clone(),
    );
    let a = temporary.path().join("a");
    let b = temporary.path().join("b");
    for (name, root) in [("a", &a), ("b", &b)] {
        std::fs::create_dir(root).unwrap();
        common::assert_ok(
            &common::run_tandem_in_with_env(
                root,
                &["init", "--server", &host.addr, "--workspace", name, "."],
                &[("TANDEM_TOKEN", &host.token), ("TANDEM_DISABLE_CACHE", "1")],
                &home,
            ),
            "initialize workspace",
        );
    }
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut options = DaemonOptions::new(&a);
    options.writer_ttl = Duration::from_secs(1);
    let mut daemon = Daemon::open(&settings, &options).unwrap();
    std::fs::write(a.join("a.txt"), b"A original\n").unwrap();
    let SnapshotOutcome::Published(first) = daemon.snapshot_once().unwrap() else {
        panic!("initial A publish")
    };
    common::assert_ok(
        &common::run_tandem_in_with_env(
            &b,
            &["new", &first.commit_id],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &home,
        ),
        "stack B on A",
    );
    // B's distinct tree is written only by this cache-disabled subprocess, so
    // A's jj Store and immutable disk cache cannot already contain it.
    let descendant = format!("B descendant {}", temporary.path().display());
    std::fs::write(b.join("b.txt"), descendant.as_bytes()).unwrap();
    common::assert_ok(
        &common::run_tandem_in_with_env(&b, &["status"], &[("TANDEM_DISABLE_CACHE", "1")], &home),
        "snapshot B descendant",
    );
    let admin = TandemClient::connect_with_cache(&host.addr, &host.token, &[], None).unwrap();
    // Settle the operation-head merge before the measured rewrite attempt.
    common::assert_ok(
        &common::run_tandem_in_with_env(
            &a,
            &["log", "--ignore-working-copy", "--no-graph", "-r", "@"],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &home,
        ),
        "settle heads without populating daemon tree cache",
    );
    let before = admin.get_heads_state().unwrap();
    let operation = jj_lib::protos::simple_op_store::Operation::decode(
        admin
            .get_operation(&before.workspace_heads["b"])
            .unwrap()
            .as_slice(),
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
    let b_commit = &view.wc_commit_ids[&jj_lib::ref_name::WorkspaceNameBuf::from("b")];
    let commit = jj_lib::protos::simple_store::Commit::decode(
        admin
            .get_object(jj_tandem_protocol::wire::KIND_COMMIT, b_commit.as_bytes())
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    *gate.path.lock().unwrap() = Some(format!(
        "/api/objects/tree/{}",
        jj_tandem_protocol::hex::to_hex(&commit.root_tree[0])
    ));
    std::fs::write(a.join("a.txt"), b"A rewritten\0\xff").unwrap();
    let (mut daemon, outcome) = std::thread::scope(|scope| {
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.release.notify_one();
            }
        }
        let release = Release(gate.clone());
        let task = scope.spawn(move || {
            let result = daemon.snapshot_once();
            (daemon, result)
        });
        entered_rx
            .recv_timeout(WAIT)
            .expect("rebase must read B's tree after scanning A");
        while claims_rx.try_recv().is_ok() {}
        gate.reject.store(1, Ordering::Release);
        loop {
            if claims_rx.recv_timeout(WAIT).unwrap() == 503 {
                break;
            }
        }
        assert_eq!(
            claims_rx.recv_timeout(WAIT).unwrap(),
            200,
            "renewal recovers but loss stays latched"
        );
        drop(release);
        task.join().unwrap()
    });
    assert!(matches!(
        outcome.unwrap(),
        SnapshotOutcome::NotTheWriter { .. }
    ));
    let after = admin.get_heads_state().unwrap();
    assert_eq!(after.version, before.version);
    assert_eq!(after.heads, before.heads);
    let SnapshotOutcome::Published(retried) = daemon.snapshot_once().unwrap() else {
        panic!("same-byte retry must publish")
    };
    drop(daemon);
    drop(admin);
    let address = host.addr.clone();
    drop(host);
    std::fs::remove_dir_all(temporary.path().join("server")).unwrap();
    let _recovered = Host::start(
        temporary.path(),
        &address,
        &token,
        gate,
        entered_tx,
        claims_tx,
    );
    let recovered = TandemClient::connect_with_cache(&address, &token, &[], None).unwrap();
    assert_eq!(
        recovered.get_heads_state().unwrap().workspace_heads["a"],
        jj_tandem_protocol::hex::from_hex(&retried.operation_id).unwrap()
    );
    let read = common::run_tandem_in_with_env(
        &a,
        &[
            "file",
            "show",
            "--ignore-working-copy",
            "-r",
            &retried.commit_id,
            "a.txt",
        ],
        &[("TANDEM_DISABLE_CACHE", "1")],
        &home,
    );
    common::assert_ok(&read, "read retried bytes after bucket-only recovery");
    assert_eq!(read.stdout, b"A rewritten\0\xff");
    let descendant_read = common::run_tandem_in_with_env(
        &a,
        &["file", "show", "--ignore-working-copy", "-r", "b@", "b.txt"],
        &[("TANDEM_DISABLE_CACHE", "1")],
        &home,
    );
    common::assert_ok(
        &descendant_read,
        "read rebased descendant after bucket-only recovery",
    );
    assert_eq!(descendant_read.stdout, descendant.as_bytes());
    assert_eq!(std::fs::read(b.join("a.txt")).unwrap(), b"A original\n");
}
