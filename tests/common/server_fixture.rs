//! One `tandem serve` subprocess, and the temp directory it lives in.
//!
//! Nearly every integration test opened with the same eight lines: a temp
//! directory, an isolated home, a server repo directory, a free port, a spawn,
//! and a wait — and closed with a kill and a wait that a panic skipped, leaving
//! the server behind. The bucket tests already had `BucketHarness` for their
//! half of this; the rest of the suite has this.
//!
//! The `Drop` is the point as much as the constructor: a test that fails now
//! takes its server with it.

use std::path::{Path, PathBuf};
use std::process::{Child, Output};
use std::time::Duration;

use tempfile::TempDir;

use super::{
    assert_ok, control_socket_path, free_addr, isolated_home, run_tandem_in,
    spawn_server_with_args_env_and_log, wait_for_server, wait_for_socket,
};

pub struct ServerFixture {
    pub tmp: TempDir,
    pub home: PathBuf,
    /// The directory the server serves.
    pub repo: PathBuf,
    pub addr: String,
    /// Where `--control-socket` points. Only passed to the server when the
    /// fixture was built with `control_socket()`.
    pub socket: PathBuf,
    pub server: Child,
    /// The token this server was started with. Every request in the suite
    /// carries it, and `init_workspace` trades it for a scoped one.
    pub admin_token: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    has_socket: bool,
    log: Option<PathBuf>,
}

impl ServerFixture {
    /// A server with the arguments every test would pass anyway.
    pub fn start() -> Self {
        Self::builder().start()
    }

    pub fn builder() -> ServerBuilder {
        ServerBuilder::default()
    }

    /// The temp root everything of this fixture's lives under.
    pub fn path(&self) -> &Path {
        self.tmp.path()
    }

    /// A directory under the temp root, created.
    pub fn dir(&self, name: &str) -> PathBuf {
        let dir = self.tmp.path().join(name);
        std::fs::create_dir_all(&dir).expect("create a directory under the fixture");
        dir
    }

    /// The admin token, for a test that talks to the API by hand.
    pub fn token(&self) -> &str {
        &self.admin_token
    }

    /// The control socket path as the CLI wants it.
    pub fn socket_str(&self) -> &str {
        self.socket.to_str().expect("socket path")
    }

    /// Run a tandem command against this fixture's home.
    pub fn run(&self, dir: &Path, args: &[&str]) -> Output {
        run_tandem_in(dir, args, &self.home)
    }

    /// Initialize a workspace in a new directory under the temp root.
    ///
    /// `workspace` names it on the server; `None` leaves the name to the
    /// server, which is what a person who did not pass `--workspace` gets.
    pub fn init_workspace(&self, dir_name: &str, workspace: Option<&str>) -> PathBuf {
        self.init_workspace_with_env(dir_name, workspace, &[])
    }

    /// The same, with extra environment for the `tandem init` — for the test
    /// that has to say which cache directory the new workspace uses.
    pub fn init_workspace_with_env(
        &self,
        dir_name: &str,
        workspace: Option<&str>,
        env: &[(&str, &str)],
    ) -> PathBuf {
        let dir = self.dir(dir_name);
        let mut args = vec!["init", "--server", &self.addr, "--token", &self.admin_token];
        if let Some(name) = workspace {
            args.extend(["--workspace", name]);
        }
        args.push(".");
        let out = super::run_tandem_in_with_env(&dir, &args, env, &self.home);
        assert_ok(&out, &format!("initialize workspace in {dir_name}"));
        dir
    }

    /// What the server has logged so far, for a test that counts what it was
    /// asked for. Empty unless the fixture was built with `log_to_file()`.
    pub fn log_text(&self) -> String {
        match &self.log {
            Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
            None => String::new(),
        }
    }

    /// How many requests for one RPC method the server has logged.
    ///
    /// Two substrings rather than one: the text log paints field names with
    /// ANSI escapes, so `rpc_method="getObject"` never appears as a contiguous
    /// run of characters even though both halves of it do.
    pub fn rpc_request_count(&self, method: &str) -> usize {
        let quoted = format!("\"{method}\"");
        self.log_text()
            .lines()
            .filter(|line| line.contains("rpc request") && line.contains(&quoted))
            .count()
    }

    /// Stop this server and start another over the same repo, on a new address.
    ///
    /// A test that does this is checking that the repo holds the history, not
    /// the process — so the address has to change, or a client could reach the
    /// old server and nobody would know.
    pub fn restart_on_new_addr(&mut self) {
        self.stop();
        self.addr = free_addr();
        let mut server = spawn(
            &self.repo,
            &self.addr,
            &self.args,
            &self.env,
            &self.home,
            self.log.as_deref(),
        );
        wait_for_server(&self.addr, &mut server);
        if self.has_socket {
            wait_for_socket(&self.socket, Duration::from_secs(5));
        }
        self.server = server;
    }

    /// Kill the server and reap it. Harmless if it has already exited — which
    /// it has, for the tests that shut it down with a signal to watch it clean
    /// up after itself.
    pub fn stop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

impl Drop for ServerFixture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
pub struct ServerBuilder {
    args: Vec<String>,
    env: Vec<(String, String)>,
    control_socket: bool,
    home: Option<(TempDir, PathBuf)>,
    log_to_file: bool,
}

impl ServerBuilder {
    /// Extra arguments for `tandem serve`.
    pub fn args(mut self, args: &[&str]) -> Self {
        self.args.extend(args.iter().map(|arg| arg.to_string()));
        self
    }

    /// Extra environment for the server process.
    pub fn envs(mut self, env: &[(&str, &str)]) -> Self {
        self.env
            .extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        self
    }

    /// Give the server a control socket, and wait for it to appear before
    /// handing the fixture back.
    pub fn control_socket(mut self) -> Self {
        self.control_socket = true;
        self
    }

    /// Use a temp directory and home the caller has already prepared — for the
    /// test that has to write a jj config before the first command runs.
    pub fn in_home(mut self, tmp: TempDir, home: PathBuf) -> Self {
        self.home = Some((tmp, home));
        self
    }

    /// Keep the server's log in a file under the fixture, and turn the level
    /// up far enough that reads appear in it. For tests that assert on what
    /// the server was asked for rather than on what it answered.
    pub fn log_to_file(mut self) -> Self {
        self.log_to_file = true;
        self
    }

    pub fn start(self) -> ServerFixture {
        let (tmp, home) = self.home.unwrap_or_else(|| {
            let tmp = TempDir::new().expect("create the fixture's temp directory");
            let home = isolated_home(tmp.path());
            (tmp, home)
        });
        let repo = tmp.path().join("server-repo");
        std::fs::create_dir_all(&repo).expect("create the server repo directory");
        let socket = control_socket_path(tmp.path());

        let mut args = self.args;
        if self.control_socket {
            args.push("--control-socket".to_string());
            args.push(socket.to_string_lossy().into_owned());
        }

        let log = self.log_to_file.then(|| {
            args.extend(["--log-level".to_string(), "debug".to_string()]);
            tmp.path().join("server.log")
        });

        // Each fixture gets its own admin token, through the environment,
        // which is where `tandem up` puts it too.
        let admin_token = jj_tandem::auth::generate_admin_token();
        let mut env = self.env;
        env.push(("TANDEM_ADMIN_TOKEN".to_string(), admin_token.clone()));

        let addr = free_addr();
        let mut server = spawn(&repo, &addr, &args, &env, &home, log.as_deref());
        wait_for_server(&addr, &mut server);
        if self.control_socket {
            wait_for_socket(&socket, Duration::from_secs(5));
        }

        ServerFixture {
            tmp,
            home,
            repo,
            addr,
            socket,
            server,
            admin_token,
            args,
            env,
            has_socket: self.control_socket,
            log,
        }
    }
}

fn spawn(
    repo: &Path,
    addr: &str,
    args: &[String],
    env: &[(String, String)],
    home: &Path,
    log: Option<&Path>,
) -> Child {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    spawn_server_with_args_env_and_log(repo, addr, &args, &env, home, log)
}
