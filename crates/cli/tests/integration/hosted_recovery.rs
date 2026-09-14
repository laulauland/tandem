use std::process::{Command, Stdio};
use std::time::Duration;

use crate::common::{
    self, assert_ok, free_addr, isolate_env, isolated_home, run_tandem_in_with_env,
    spawn_server_with_args_and_env_with_lines,
};

#[test]
fn named_repository_recovers_exact_bytes_and_accepts_another_publish() {
    let temporary = tempfile::tempdir().unwrap();
    let home = isolated_home(temporary.path());
    let cache = temporary.path().join("host-cache");
    let bucket = temporary.path().join("bucket");
    let address = free_addr();
    let bucket_text = std::env::var("TANDEM_TEST_S3_BUCKET")
        .map(|base| {
            // Credentials stay in the AWS environment used by object_store. The
            // bucket URL carries only the bucket and an isolated object prefix.
            let prefix = format!(
                "stage1-hosted-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock")
                    .as_nanos()
            );
            match base.split_once('?') {
                Some((location, query)) => {
                    format!("{}/{prefix}?{query}", location.trim_end_matches('/'))
                }
                None => format!("{}/{prefix}", base.trim_end_matches('/')),
            }
        })
        .unwrap_or_else(|_| bucket.to_string_lossy().to_string());
    let host_secret = jj_tandem_server::generate_admin_token();
    let environment = [("TANDEM_ADMIN_TOKEN", host_secret.as_str())];
    let (child, lines) = spawn_server_with_args_and_env_with_lines(
        &cache,
        &address,
        &[
            "--hosted",
            "--bucket",
            bucket_text.as_str(),
            "--log-level",
            "info",
        ],
        &environment,
        &home,
    );
    let mut server = ProcessGuard::with_lines(child, lines);
    let serving_pid = server.child.id();
    let owner = create_owner_when_ready(&address, &host_secret, &mut server);
    let owner_token = owner["token"].as_str().unwrap();
    let second_owner = create_owner(&address, &host_secret);
    let second_owner_token = second_owner["token"].as_str().unwrap();
    let repository_address = format!("http://{address}/acme/stage-one");
    let second_repository_address = format!("http://{address}/beta/stage-two");
    let first = temporary.path().join("first");
    let first_text = first.to_string_lossy().to_string();
    let token_environment = [("TANDEM_TOKEN", owner_token)];
    assert_ok(
        &run_tandem_in_with_env(
            temporary.path(),
            &[
                "clone",
                &repository_address,
                &first_text,
                "--workspace",
                "agent-one",
            ],
            &token_environment,
            &home,
        ),
        "initial hosted clone",
    );
    let second = temporary.path().join("second");
    let second_text = second.to_string_lossy().to_string();
    assert_ok(
        &run_tandem_in_with_env(
            temporary.path(),
            &[
                "clone",
                &second_repository_address,
                &second_text,
                "--workspace",
                "agent-two",
            ],
            &[("TANDEM_TOKEN", second_owner_token)],
            &home,
        ),
        "second owner's hosted clone",
    );
    assert_eq!(
        server.child.id(),
        serving_pid,
        "both repositories use one host"
    );
    let cross_owner = reqwest::blocking::Client::new()
        .get(format!("{second_repository_address}/api/info"))
        .bearer_auth(owner_token)
        .send()
        .unwrap();
    assert_eq!(cross_owner.status(), reqwest::StatusCode::NOT_FOUND);

    let uploaded = reqwest::blocking::Client::new()
        .post(format!("{repository_address}/api/objects/file"))
        .bearer_auth(owner_token)
        .body(b"owner-one-private-object".to_vec())
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
    let object_id = uploaded
        .headers()
        .get("tandem-object-id")
        .expect("object response id")
        .to_str()
        .unwrap();
    let object_denied = reqwest::blocking::Client::new()
        .get(format!("{repository_address}/api/objects/file/{object_id}"))
        .bearer_auth(second_owner_token)
        .send()
        .unwrap();
    assert_eq!(object_denied.status(), reqwest::StatusCode::NOT_FOUND);
    let mut forged_owner = owner_token.as_bytes().to_vec();
    let last = forged_owner.last_mut().unwrap();
    *last = if *last == b'a' { b'b' } else { b'a' };
    let forged_owner = String::from_utf8(forged_owner).unwrap();
    let forged = reqwest::blocking::Client::new()
        .get(format!("{repository_address}/api/info"))
        .bearer_auth(forged_owner)
        .send()
        .unwrap();
    assert_eq!(
        forged.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a syntactically valid forged owner token was accepted"
    );

    let workspace: serde_json::Value = reqwest::blocking::Client::new()
        .post(format!("{repository_address}/api/tokens"))
        .bearer_auth(owner_token)
        .json(&serde_json::json!({"workspaceId":"agent-one","ttlSeconds":3600}))
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    let scoped = workspace["token"].as_str().unwrap();
    let repository_scope_denied = reqwest::blocking::Client::new()
        .post(format!(
            "{second_repository_address}/api/workspaces/agent-one/writer"
        ))
        .bearer_auth(scoped)
        .json(&serde_json::json!({"holder":"agent-one","ttlSeconds":30}))
        .send()
        .unwrap();
    assert_eq!(
        repository_scope_denied.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a workspace token was accepted by another repository"
    );
    let scope_denied = reqwest::blocking::Client::new()
        .post(format!(
            "{repository_address}/api/workspaces/agent-two/writer"
        ))
        .bearer_auth(scoped)
        .json(&serde_json::json!({"holder":"intruder","ttlSeconds":30}))
        .send()
        .unwrap();
    assert_eq!(
        scope_denied.status(),
        reqwest::StatusCode::FORBIDDEN,
        "workspace token escaped its workspace scope"
    );

    let expected = b"\0\x01\xffstage one\n";
    let second_expected = b"\xff\0stage two belongs to beta\n";
    std::fs::write(first.join("payload.bin"), expected).unwrap();
    std::fs::write(second.join("payload.bin"), second_expected).unwrap();
    std::thread::scope(|scope| {
        let first_publish = scope.spawn(|| publish_one_change(&first, &home));
        let second_publish = scope.spawn(|| publish_one_change(&second, &home));
        first_publish.join().unwrap();
        second_publish.join().unwrap();
    });
    server.stop();
    std::fs::rename(&cache, temporary.path().join("discarded-cache")).unwrap();
    let interrupted_recovery = cache.join("repositories/acme/stage-one/.jj");
    std::fs::create_dir_all(&interrupted_recovery).unwrap();
    std::fs::write(
        interrupted_recovery.join("partial"),
        b"interrupted reconstruction",
    )
    .unwrap();

    let (child, lines) = spawn_server_with_args_and_env_with_lines(
        &cache,
        &address,
        &[
            "--hosted",
            "--bucket",
            bucket_text.as_str(),
            "--log-level",
            "info",
        ],
        &environment,
        &home,
    );
    let mut recovered = ProcessGuard::with_lines(child, lines);
    recovered.wait_for_listening(&address);
    let fresh = temporary.path().join("fresh");
    let fresh_text = fresh.to_string_lossy().to_string();
    let fresh_environment = [
        ("TANDEM_TOKEN", owner_token),
        ("TANDEM_DISABLE_CACHE", "true"),
    ];
    assert_ok(
        &run_tandem_in_with_env(
            temporary.path(),
            &[
                "clone",
                &repository_address,
                &fresh_text,
                "--workspace",
                "agent-one",
            ],
            &fresh_environment,
            &home,
        ),
        "clone after cache loss",
    );
    assert_eq!(std::fs::read(fresh.join("payload.bin")).unwrap(), expected);
    assert!(!interrupted_recovery.join("partial").exists());
    let second_fresh = temporary.path().join("second-fresh");
    let second_fresh_text = second_fresh.to_string_lossy().to_string();
    let second_fresh_environment = [
        ("TANDEM_TOKEN", second_owner_token),
        ("TANDEM_DISABLE_CACHE", "true"),
    ];
    assert_ok(
        &run_tandem_in_with_env(
            temporary.path(),
            &[
                "clone",
                &second_repository_address,
                &second_fresh_text,
                "--workspace",
                "agent-two",
            ],
            &second_fresh_environment,
            &home,
        ),
        "second owner clone after total host cache loss",
    );
    assert_eq!(
        std::fs::read(second_fresh.join("payload.bin")).unwrap(),
        second_expected
    );

    recovered.stop();
    let heads = cache.join("repositories/acme/stage-one/.jj/repo/tandem/heads.json");
    std::fs::write(&heads, b"{").unwrap();
    let (child, lines) = spawn_server_with_args_and_env_with_lines(
        &cache,
        &address,
        &[
            "--hosted",
            "--bucket",
            bucket_text.as_str(),
            "--log-level",
            "info",
        ],
        &environment,
        &home,
    );
    let mut recovered = ProcessGuard::with_lines(child, lines);
    let other_owner = create_owner_when_ready(&address, &host_secret, &mut recovered);
    let other_owner_token = other_owner["token"].as_str().unwrap();
    let after_corruption = temporary.path().join("after-corruption");
    let after_corruption_text = after_corruption.to_string_lossy().to_string();
    assert_ok(
        &run_tandem_in_with_env(
            temporary.path(),
            &[
                "clone",
                &repository_address,
                &after_corruption_text,
                "--workspace",
                "agent-one",
            ],
            &fresh_environment,
            &home,
        ),
        "clone after corrupt cache reconstruction",
    );
    assert_eq!(
        std::fs::read(after_corruption.join("payload.bin")).unwrap(),
        expected
    );

    std::fs::write(
        after_corruption.join("payload.bin"),
        b"published after recovery\0\xff\n",
    )
    .unwrap();
    publish_one_change(&after_corruption, &home);
    let third = temporary.path().join("third");
    let third_text = third.to_string_lossy().to_string();
    assert_ok(
        &run_tandem_in_with_env(
            temporary.path(),
            &[
                "clone",
                &repository_address,
                &third_text,
                "--workspace",
                "agent-one",
            ],
            &fresh_environment,
            &home,
        ),
        "clone after second publish",
    );
    assert_eq!(
        std::fs::read(third.join("payload.bin")).unwrap(),
        b"published after recovery\0\xff\n"
    );

    let denied = run_tandem_in_with_env(
        temporary.path(),
        &[
            "clone",
            &repository_address,
            temporary.path().join("denied").to_str().unwrap(),
            "--workspace",
            "intruder",
        ],
        &[("TANDEM_TOKEN", other_owner_token)],
        &home,
    );
    assert!(
        !denied.status.success(),
        "another owner accessed the repository"
    );
}

struct ProcessGuard {
    child: std::process::Child,
    lines: Option<common::lines::Lines>,
}
impl ProcessGuard {
    fn new(child: std::process::Child) -> Self {
        Self { child, lines: None }
    }
    fn with_lines(child: std::process::Child, lines: common::lines::Lines) -> Self {
        Self {
            child,
            lines: Some(lines),
        }
    }
    fn wait_for_listening(&mut self, address: &str) {
        let lines = self.lines.as_mut().expect("host readiness stream");
        if lines
            .wait_for(Duration::from_secs(10), |line| {
                line.contains("tandem server listening on")
            })
            .is_none()
        {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("host at {address} exited before announcing readiness ({status})");
            }
            panic!("host at {address} did not announce readiness before deadline");
        }
    }
    fn stop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn publish_one_change(workspace: &std::path::Path, home: &std::path::Path) {
    let mut command = Command::new(common::tandem_bin());
    command.args(["daemon", workspace.to_str().unwrap(), "--debounce-ms", "10"]);
    isolate_env(&mut command, home);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut daemon = ProcessGuard::new(child);
    let mut lines = common::lines::Lines::from(stdout);
    assert!(
        lines
            .wait_for(Duration::from_secs(10), |line| line
                .starts_with("published op="))
            .is_some(),
        "workspace daemon did not publish:\n{}",
        lines.transcript()
    );
    daemon.stop();
}

fn create_owner_when_ready(
    address: &str,
    host_secret: &str,
    host: &mut ProcessGuard,
) -> serde_json::Value {
    host.wait_for_listening(address);
    create_owner(address, host_secret)
}

fn create_owner(address: &str, host_secret: &str) -> serde_json::Value {
    common::http_client()
        .post(format!("http://{address}/api/owners"))
        .bearer_auth(host_secret)
        .send()
        .expect("send owner creation request")
        .error_for_status()
        .expect("host rejected owner creation after announcing readiness")
        .json()
        .expect("decode owner response")
}
