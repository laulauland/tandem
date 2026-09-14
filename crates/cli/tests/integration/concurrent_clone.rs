use crate::common::{self, ServerFixture};
use std::sync::{Arc, Barrier};

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
    let fx = ServerFixture::builder()
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
                assert!(
                    matches!(outcome, jj_tandem_workspace::SnapshotOutcome::Published(_)),
                    "{name}: first daemon snapshot must publish without stale repair: {outcome:?}"
                );
                assert_eq!(std::fs::read(dir.join("payload.bin")).unwrap(), bytes);
                (dir, name, bytes)
            })
        })
        .collect();
    let published: Vec<_> = publishers
        .into_iter()
        .map(|publisher| publisher.join().unwrap())
        .collect();
    let observer = &workspaces[0].0;
    for (_, workspace, bytes) in &published {
        let revision = format!("{workspace}@");
        let read = common::run_tandem_in_with_env(
            observer,
            &["file", "show", "-r", &revision, "payload.bin"],
            &[("TANDEM_DISABLE_CACHE", "1")],
            &fx.home,
        );
        common::assert_ok(&read, "read another workspace revision");
        assert_eq!(read.stdout, *bytes);
    }
}
