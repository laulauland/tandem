//! The in-process harness the simulation and the property suite share.
//!
//! One process holds the server and every client. That is the whole point:
//! a schedule that spawns subprocesses is at the mercy of the scheduler, so it
//! cannot be replayed from a seed, and it pays a process spawn for every step.
//! Here a step is a function call, a crash is a fault flag, and a restart is a
//! `Server` built again over the same directory.

#![allow(dead_code)]

pub mod agent;
pub mod cluster;
pub mod oracle;
pub mod rng;
pub mod schedule;

use std::sync::OnceLock;

use anyhow::{Context, Result};
use jj_lib::settings::UserSettings;
use tempfile::TempDir;

/// The jj configuration every test runs under, shared with `tests/common` so
/// that the two suites cannot drift into different identities.
pub const JJ_TEST_CONFIG: &str = include_str!("../jj-test-config.toml");

/// A home directory nothing else in this process shares, created once.
///
/// jj reads the user's real configuration unless it is told otherwise, and it
/// writes a repo registry into it. A test suite that skipped this would read
/// whatever the person running it has configured and write into their home.
static FAKE_HOME: OnceLock<TempDir> = OnceLock::new();

/// Point this process at a home of its own. Safe to call from any test.
///
/// Every write to the environment happens inside the initializer, so it happens
/// exactly once and before any other thread can observe the result. Writing
/// them on every call was idempotent in the sense that the values never
/// changed, but `setenv` racing `getenv` is a data race on glibc whatever the
/// value is — and the simulation runs six lanes of clusters while their agents
/// are reading the environment to find their jj configuration.
pub fn isolate_process_environment() {
    FAKE_HOME.get_or_init(|| {
        let home = tempfile::tempdir().expect("create the suite's fake home");
        let config_dir = home.path().join(".config").join("jj");
        std::fs::create_dir_all(&config_dir).expect("create the suite's jj config directory");
        std::fs::write(config_dir.join("config.toml"), JJ_TEST_CONFIG)
            .expect("write the suite's jj config");

        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        // The op-heads store prefers these over the store files on disk. In a
        // process holding several workspaces, one value cannot be right for all
        // of them, so there must be no value at all.
        std::env::remove_var("TANDEM_SERVER");
        std::env::remove_var("TANDEM_WORKSPACE");
        home
    });
}

/// The settings every in-process client and every direct repo read uses — the
/// same path the server takes, so that neither side is reading a different
/// configuration than the other.
pub fn test_settings() -> Result<UserSettings> {
    isolate_process_environment();
    let config_env = jj_cli::config::ConfigEnv::from_environment();
    let mut raw_config =
        jj_cli::config::config_from_environment(jj_cli::config::default_config_layers());
    config_env
        .reload_user_config(&mut raw_config)
        .context("load the suite's jj config")?;
    let resolved = config_env
        .resolve_config(&raw_config)
        .context("resolve the suite's jj config")?;
    UserSettings::from_config(resolved).context("create the suite's jj settings")
}
