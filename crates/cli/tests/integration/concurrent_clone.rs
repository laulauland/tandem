use crate::common::{self, ServerFixture};
use std::sync::{Arc, Barrier};

#[test]
fn repeated_snapshots_scan_a_ten_thousand_file_tree_without_losing_the_final_edit() {
    let fx = ServerFixture::start();
    let workspace = fx.init_workspace("large-tree", Some("scanner"));
    let tree = workspace.join("tree");
    std::fs::create_dir(&tree).unwrap();
    for index in 0..10_000 {
        std::fs::write(
            tree.join(format!("file-{index:05}")),
            format!("initial {index}\n"),
        )
        .unwrap();
    }
    let settings =
        jj_lib::settings::UserSettings::from_config(jj_lib::config::StackedConfig::with_defaults())
            .unwrap();
    let mut daemon = jj_tandem_workspace::Daemon::open(
        &settings,
        &jj_tandem_workspace::DaemonOptions::new(&workspace),
    )
    .unwrap();
    assert!(matches!(
        daemon.snapshot_once().unwrap(),
        jj_tandem_workspace::SnapshotOutcome::Published(_)
    ));
    let mut scan_millis = Vec::new();
    for generation in 0..43u8 {
        let bytes = vec![0, generation, 255, b'\n'];
        std::fs::write(tree.join("file-09999"), &bytes).unwrap();
        let started = std::time::Instant::now();
        let jj_tandem_workspace::SnapshotOutcome::Published(published) =
            daemon.snapshot_once().unwrap()
        else {
            panic!("rewritten scan file must publish");
        };
        if generation >= 3 {
            scan_millis.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
        assert_eq!(std::fs::read(tree.join("file-09999")).unwrap(), bytes);
        let read = common::run_tandem_in_with_env(
            &workspace,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                &published.commit_id,
                "tree/file-09999",
            ],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &fx.home,
        );
        common::assert_ok(&read, "read acknowledged scan generation");
        assert_eq!(read.stdout, bytes);
    }
    println!("stage6_scan_millis={scan_millis:?}");
}

#[test]
fn concurrent_new_workspaces_can_publish_without_stale_repair() {
    concurrent_clones_after_rewrites("bench", false);
}

#[test]
fn concurrent_clones_from_nonempty_default_context_can_publish_without_repair() {
    concurrent_clones_after_rewrites("default", false);
}

#[test]
fn concurrent_clones_after_reattached_workspace_rewrites_can_publish_without_repair() {
    concurrent_clones_after_rewrites("bench", true);
}

fn concurrent_clones_after_rewrites(seed_name: &str, reattach: bool) {
    let bucket = tempfile::tempdir().unwrap();
    let mut fx = ServerFixture::builder()
        .args(&["--bucket", bucket.path().to_str().unwrap()])
        .log_to_file()
        .start();
    // Match the qualification repository: an existing non-default workspace
    // has rewritten its working-copy commit repeatedly before clones arrive.
    let mut seed = fx.init_workspace("seed", Some(seed_name));
    for generation in 0..4u8 {
        std::fs::write(seed.join("seed.bin"), [0, 255, generation, 10]).unwrap();
        common::assert_ok(&fx.run(&seed, &["status"]), "seed rewritten history");
    }
    if seed_name == "default" {
        // New clones inherit default's parents, not its mutable working copy.
        common::assert_ok(&fx.run(&seed, &["new"]), "establish nonempty parent");
    }
    if reattach {
        seed = fx.init_workspace("reattached", Some(seed_name));
        for generation in 4..8u8 {
            std::fs::write(seed.join("seed.bin"), [0, 255, generation, 10]).unwrap();
            common::assert_ok(&fx.run(&seed, &["status"]), "rewrite after reattach");
        }
    }
    let barrier = Arc::new(Barrier::new(4));
    let clones: Vec<_> = (0..4)
        .map(|index| {
            let name = format!("parallel-{index}");
            let dir = fx.dir(&name);
            let home = fx.home.clone();
            let addr = fx.addr.clone();
            let token: String = common::http_client()
                .post(format!("http://{}/api/tokens", fx.addr))
                .bearer_auth(fx.token())
                .json(&serde_json::json!({
                    "workspaceId": name,
                    "ttlSeconds": 3_600,
                }))
                .send()
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<serde_json::Value>()
                .unwrap()["token"]
                .as_str()
                .unwrap()
                .to_string();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let out = common::run_tandem_in_with_env(
                    &dir,
                    &["clone", &addr, ".", "--workspace", &name],
                    &[
                        ("TANDEM_TOKEN", &token),
                        ("TANDEM_DISABLE_CACHE", "1"),
                        ("TANDEM_BENCH_INJECT_RTT_MS", "20"),
                    ],
                    &home,
                );
                common::assert_ok(&out, "concurrent clone");
                (dir, name)
            })
        })
        .collect();
    let workspaces: Vec<_> = clones
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let publish_barrier = Arc::new(Barrier::new(workspaces.len()));
    let expect_seed = seed_name == "default";
    let publishers: Vec<_> = workspaces
        .iter()
        .cloned()
        .map(|(dir, name)| {
            let publish_barrier = publish_barrier.clone();
            std::thread::spawn(move || {
                if expect_seed {
                    assert_eq!(
                        std::fs::read(dir.join("seed.bin")).unwrap(),
                        [0, 255, 3, 10]
                    );
                }
                let bytes = format!("first edit in {name}\0\n").into_bytes();
                std::fs::write(dir.join("payload.bin"), &bytes).unwrap();
                publish_barrier.wait();
                let settings = jj_lib::settings::UserSettings::from_config(
                    jj_lib::config::StackedConfig::with_defaults(),
                )
                .unwrap();
                let mut daemon = jj_tandem_workspace::Daemon::open(
                    &settings,
                    &jj_tandem_workspace::DaemonOptions::new(&dir),
                )
                .unwrap();
                let outcome = daemon.snapshot_once().unwrap();
                let jj_tandem_workspace::SnapshotOutcome::Published(published) = outcome else {
                    panic!("{name}: first daemon snapshot must publish without stale repair: {outcome:?}");
                };
                assert_eq!(std::fs::read(dir.join("payload.bin")).unwrap(), bytes);
                (dir, name, bytes, published.operation_id)
            })
        })
        .collect();
    let published: Vec<_> = publishers
        .into_iter()
        .map(|publisher| publisher.join().unwrap())
        .collect();
    fx.stop();
    std::fs::remove_dir_all(&fx.repo).unwrap();
    fx.restart_with_env(&[]);
    let observer = &workspaces[0].0;
    let operations = common::run_tandem_in_with_env(
        observer,
        &[
            "op",
            "log",
            "--ignore-working-copy",
            "--no-graph",
            "-T",
            "id ++ \"\\n\"",
        ],
        &[("TANDEM_DISABLE_CACHE", "1")],
        &fx.home,
    );
    common::assert_ok(&operations, "walk recovered operation ancestry");
    let operations = String::from_utf8(operations.stdout).unwrap();
    for (_, workspace, bytes, operation) in &published {
        assert!(
            operations.lines().any(|line| line == operation),
            "acknowledged operation must remain reachable after cold recovery"
        );
        let revision = format!("{workspace}@");
        let read = common::run_tandem_in_with_env(
            observer,
            &[
                "file",
                "show",
                "--ignore-working-copy",
                "-r",
                &revision,
                "payload.bin",
            ],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &fx.home,
        );
        common::assert_ok(&read, "read another workspace revision");
        assert_eq!(read.stdout, *bytes);
    }
}
