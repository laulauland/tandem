//! `tandem watch`, end to end against a real server.
//!
//! What is pinned here, in one run of one watcher:
//! - it says on stderr which server it subscribed to
//! - it catches up: a watcher that starts late is told the state it missed
//! - it notifies: every publish after the subscription produces a line
//! - the line format is `version=<n> heads=<hex,...>` and versions only rise
//!
//! Every wait below is a deadline read on the watcher's own stdout, so the test
//! costs what the server costs and not a second per commit.

use crate::common;
use crate::common::lines::Lines;

use std::process::{Command, Stdio};
use std::time::Duration;

/// How long any single line may take to arrive. Generous: the failure this
/// guards is "never", not "slowly".
const LINE_TIMEOUT: Duration = Duration::from_secs(10);

fn version_of(line: &str) -> u64 {
    line.strip_prefix("version=")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no version in watch line: {line}"))
}

#[test]
fn watch_registers_catches_up_and_then_notifies() {
    let fx = common::ServerFixture::start();
    let home = fx.home.clone();
    let addr = fx.addr.clone();
    let workspace = fx.init_workspace("workspace", None);

    // Publish before anyone is watching. What the watcher says about this
    // commit is the catch-up.
    std::fs::write(workspace.join("before.txt"), b"before the watcher\n").unwrap();
    common::assert_ok(
        &common::run_tandem_in(&workspace, &["new", "-m", "before the watcher"], &home),
        "publish before watching",
    );

    let mut watch_cmd = Command::new(common::tandem_bin());
    watch_cmd
        .current_dir(fx.path())
        .args(["watch", "--server", &addr, "--token", fx.token()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    common::isolate_env(&mut watch_cmd, &home);
    let mut watch = watch_cmd.spawn().expect("spawn tandem watch");

    let mut out = Lines::from(watch.stdout.take().expect("watch stdout"));
    let mut err = Lines::from(watch.stderr.take().expect("watch stderr"));

    // ── It says what it subscribed to ─────────────────────────────────
    let registration = err.wait_for(LINE_TIMEOUT, |line| line.contains("watching heads on"));
    assert!(
        registration.is_some(),
        "watch never reported its subscription\nstderr:\n{}",
        err.transcript()
    );

    // ── It catches up ─────────────────────────────────────────────────
    let catch_up = out
        .wait_for(LINE_TIMEOUT, |line| line.starts_with("version="))
        .unwrap_or_else(|| {
            panic!(
                "watch printed no catch-up line\nstdout:\n{}\nstderr:\n{}",
                out.transcript(),
                err.transcript()
            )
        });
    let heads = catch_up
        .split("heads=")
        .nth(1)
        .unwrap_or_else(|| panic!("no heads= in the catch-up line: {catch_up}"));
    assert!(
        !heads.trim().is_empty(),
        "the catch-up line must name the heads the watcher missed: {catch_up}"
    );

    // ── It notifies, and the versions only rise ───────────────────────
    let mut versions = vec![version_of(&catch_up)];
    for round in 0..2 {
        let name = format!("after-{round}.txt");
        std::fs::write(workspace.join(&name), format!("round {round}\n")).unwrap();
        common::assert_ok(
            &common::run_tandem_in(&workspace, &["new", "-m", &format!("round {round}")], &home),
            "publish while watching",
        );

        let last = *versions.last().unwrap();
        let line = out
            .wait_for(LINE_TIMEOUT, |line| {
                line.starts_with("version=") && version_of(line) > last
            })
            .unwrap_or_else(|| {
                panic!(
                    "no notification above version {last} after round {round}\nstdout:\n{}\nstderr:\n{}",
                    out.transcript(),
                    err.transcript()
                )
            });
        assert!(
            line.contains(" heads="),
            "a notification must name the heads: {line}"
        );
        versions.push(version_of(&line));
    }

    assert!(
        versions.windows(2).all(|pair| pair[1] > pair[0]),
        "watch versions must rise, got {versions:?}"
    );

    let _ = watch.kill();
    let _ = watch.wait();
}
