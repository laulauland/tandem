//! The shared harness for the bucket tests.
//!
//! Every bucket test drives the same shape: a server started against a bucket,
//! workspaces initialized against that server, and assertions read off the
//! server's own disk. Tier 1 keeps the bucket on the filesystem; tier 2 points
//! the same tests at SeaweedFS through `TANDEM_TEST_S3_BUCKET`.
//!
//! Only what every bucket test needs lives here. A test file adds its own
//! methods in an `impl` block of its own — the type is local to the test crate,
//! so that works without touching this file.

use std::path::{Path, PathBuf};
use std::process::Child;

use tempfile::TempDir;

use super::{
    assert_ok, free_addr, isolated_home, run_tandem_in, spawn_server_with_args_env_and_log,
    wait_for_server,
};

pub struct BucketHarness {
    pub tmp: TempDir,
    pub home: PathBuf,
    pub repo: PathBuf,
    pub bucket_dir: PathBuf,
    pub bucket_spec: String,
    pub addr: String,
    pub server: Option<Child>,
    /// The token this harness's server accepts. Fixed for the harness, so a
    /// restart keeps accepting the workspace tokens already on disk.
    pub admin_token: String,
}

impl BucketHarness {
    pub fn new(name: &str) -> Self {
        let tmp = tempfile::tempdir().expect("temp dir");
        let home = isolated_home(tmp.path());
        let repo = tmp.path().join("server-repo");
        std::fs::create_dir_all(&repo).expect("create server repo dir");
        let bucket_dir = tmp.path().join("bucket");

        // Tier 2 shares one SeaweedFS bucket across tests, so give each run its
        // own prefix.
        let bucket_spec = match std::env::var("TANDEM_TEST_S3_BUCKET") {
            Ok(base) => s3_spec_with_prefix(&base, name),
            Err(_) => bucket_dir.to_string_lossy().to_string(),
        };

        Self {
            tmp,
            home,
            repo,
            bucket_dir,
            bucket_spec,
            addr: free_addr(),
            server: None,
            admin_token: jj_tandem::auth::generate_admin_token(),
        }
    }

    pub fn start_server(&mut self) {
        self.start_server_logging(None);
    }

    /// Start a server whose log lands in a file, so a test can count what the
    /// server did rather than only what it left behind.
    pub fn start_server_logging(&mut self, log: Option<&Path>) {
        assert!(self.server.is_none(), "server already running");
        let mut args: Vec<&str> = vec!["--bucket", &self.bucket_spec];
        if log.is_some() {
            args.extend(["--log-level", "debug"]);
        }
        let env = [("TANDEM_ADMIN_TOKEN", self.admin_token.as_str())];
        let mut child = spawn_server_with_args_env_and_log(
            &self.repo, &self.addr, &args, &env, &self.home, log,
        );
        wait_for_server(&self.addr, &mut child, Some(&self.admin_token));
        self.server = Some(child);
    }

    pub fn stop_server(&mut self) {
        if let Some(mut child) = self.server.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn init_workspace(&self, name: &str) -> PathBuf {
        let dir = self.tmp.path().join(name);
        std::fs::create_dir_all(&dir).expect("create workspace dir");
        let out = run_tandem_in(
            &dir,
            &[
                "init",
                "--server",
                &self.addr,
                "--token",
                &self.admin_token,
                "--workspace",
                name,
                ".",
            ],
            &self.home,
        );
        assert_ok(&out, &format!("init {name}"));
        dir
    }

    pub fn run(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        run_tandem_in(dir, args, &self.home)
    }

    /// The server's own idea of where it is: `.jj/repo/tandem/heads.json`.
    pub fn local_version(&self) -> u64 {
        let path = self.repo.join(".jj/repo/tandem/heads.json");
        let bytes = std::fs::read(&path).expect("read local heads metadata");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse heads.json");
        value["version"].as_u64().expect("version field")
    }

    pub fn op_head_files(&self) -> Vec<String> {
        let mut heads = read_dir_names(&self.repo.join(".jj/repo/op_heads/heads"));
        heads.sort();
        heads
    }
}

impl Drop for BucketHarness {
    fn drop(&mut self) {
        self.stop_server();
    }
}

/// The file names in a directory, or nothing at all if it does not exist.
pub fn read_dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| {
            entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .to_string()
        })
        .collect()
}

fn s3_spec_with_prefix(base: &str, name: &str) -> String {
    let unique = format!(
        "{name}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    match base.split_once('?') {
        Some((location, query)) => format!("{}/{unique}?{query}", location.trim_end_matches('/')),
        None => format!("{}/{unique}", base.trim_end_matches('/')),
    }
}

/// Whether this run points the bucket tests at a real S3 API.
pub fn using_s3() -> bool {
    std::env::var("TANDEM_TEST_S3_BUCKET").is_ok()
}
