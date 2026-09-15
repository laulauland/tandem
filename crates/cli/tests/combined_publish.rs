use jj_tandem_protocol::wire;
use jj_tandem_test_support::cluster::Cluster;

#[test]
fn combined_publish_authenticates_before_decoding() {
    let cluster = Cluster::start().unwrap();
    let http = reqwest::blocking::Client::new();
    let url = format!("{}/api/publish", cluster.base_url());
    assert_eq!(
        http.post(&url)
            .body(b"bad".to_vec())
            .send()
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        http.post(&url)
            .bearer_auth(&cluster.admin_token)
            .header("If-Match", "\"0\"")
            .body(b"bad".to_vec())
            .send()
            .unwrap()
            .status(),
        400
    );
}

use jj_tandem_client::TandemClient;
use jj_tandem_test_support::prepared::{read_edited_file, LocalPreparation};

fn send(
    cluster: &Cluster,
    token: &str,
    request: &wire::PreparedPublish,
    version: u64,
) -> reqwest::blocking::Response {
    reqwest::blocking::Client::new()
        .post(format!("{}/api/publish", cluster.base_url()))
        .bearer_auth(token)
        .header(
            "If-Match",
            jj_tandem_protocol::http::etag_for_version(version),
        )
        .body(wire::encode_prepared_publish(request))
        .send()
        .unwrap()
}

#[test]
fn prepared_publish_survives_cold_restart_and_retry_after_discarded_response() {
    let mut cluster = Cluster::start().unwrap();
    let client = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    let local = LocalPreparation::import(&client, "writer").unwrap();
    let change = local.prepare(b"published exact bytes\0\xff").unwrap();
    let version = client.get_heads_state().unwrap().version;
    let response = send(&cluster, &cluster.admin_token, &change.request, version);
    assert_eq!(response.status(), 200, "{}", response.text().unwrap());
    cluster.cold_restart().unwrap();
    let fresh = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    assert_eq!(
        read_edited_file(&fresh, &change.commit_id).unwrap(),
        b"published exact bytes\0\xff"
    );
    assert_eq!(
        send(&cluster, &cluster.admin_token, &change.request, version).status(),
        412
    );
    let current = fresh.get_heads_state().unwrap().version;
    let retry = send(&cluster, &cluster.admin_token, &change.request, current);
    assert_eq!(retry.status(), 200, "{}", retry.text().unwrap());
    cluster.cold_restart().unwrap();
    let fresh = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    assert_eq!(
        read_edited_file(&fresh, &change.commit_id).unwrap(),
        b"published exact bytes\0\xff"
    );
}

#[test]
fn mismatches_and_malformed_payloads_leave_heads_and_durable_index_unchanged() {
    let cluster = Cluster::start().unwrap();
    let client = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    let local = LocalPreparation::import(&client, "writer").unwrap();
    let change = local.prepare(b"must not publish").unwrap();
    let before = client.get_heads_state().unwrap();
    for item in 0..change.request.objects.len() {
        let mut bad = change.request.clone();
        bad.objects[item].id[0] ^= 1;
        assert_eq!(
            send(&cluster, &cluster.admin_token, &bad, before.version).status(),
            409
        );
        assert_eq!(client.get_heads_state().unwrap().version, before.version);
        assert_eq!(client.get_heads_state().unwrap().heads, before.heads);
    }
    let mut bad = change.request.clone();
    bad.heads.new_id = "00".repeat(64);
    assert_eq!(
        send(&cluster, &cluster.admin_token, &bad, before.version).status(),
        409
    );
    let mut bad = change.request.clone();
    bad.objects[1].data = vec![255];
    assert_eq!(
        send(&cluster, &cluster.admin_token, &bad, before.version).status(),
        400
    );
    assert_eq!(client.get_heads_state().unwrap().heads, before.heads);
}

fn assert_published(
    client: &TandemClient,
    name: &str,
    operation: &str,
    commit: &jj_lib::backend::CommitId,
    bytes: &[u8],
) {
    jj_tandem_test_support::prepared::verify_published(
        client,
        name,
        &jj_tandem_protocol::hex::from_hex(operation).unwrap(),
        commit,
        bytes,
    )
    .unwrap();
}

#[test]
fn simultaneous_scoped_writers_preserve_both_publishes() {
    let mut cluster = Cluster::start().unwrap();
    let admin = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    let a = LocalPreparation::import(&admin, "a")
        .unwrap()
        .prepare(b"A\0exact")
        .unwrap();
    let b = LocalPreparation::import(&admin, "b")
        .unwrap()
        .prepare(b"B\xffexact")
        .unwrap();
    let ta = admin
        .mint_workspace_token("a", Some(600))
        .unwrap()
        .unwrap()
        .token;
    let tb = admin
        .mint_workspace_token("b", Some(600))
        .unwrap()
        .unwrap()
        .token;
    let version = admin.get_heads_state().unwrap().version;
    let barrier = std::sync::Barrier::new(2);
    let statuses = std::thread::scope(|scope| {
        let aa = scope.spawn(|| {
            barrier.wait();
            send(&cluster, &ta, &a.request, version).status()
        });
        let bb = scope.spawn(|| {
            barrier.wait();
            send(&cluster, &tb, &b.request, version).status()
        });
        [aa.join().unwrap(), bb.join().unwrap()]
    });
    assert!(statuses.contains(&reqwest::StatusCode::OK));
    for (status, token, change) in [(statuses[0], &ta, &a), (statuses[1], &tb, &b)] {
        if status == 412 {
            let retry = send(
                &cluster,
                token,
                &change.request,
                admin.get_heads_state().unwrap().version,
            );
            assert_eq!(retry.status(), 200, "{}", retry.text().unwrap());
        } else {
            assert_eq!(status, 200);
        }
    }
    cluster.cold_restart().unwrap();
    let fresh = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    assert_published(
        &fresh,
        "a",
        &a.request.heads.new_id,
        &a.commit_id,
        b"A\0exact",
    );
    assert_published(
        &fresh,
        "b",
        &b.request.heads.new_id,
        &b.commit_id,
        b"B\xffexact",
    );
}

#[test]
fn scoped_token_cannot_publish_another_workspaces_prepared_view() {
    let cluster = Cluster::start().unwrap();
    let admin = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    let change = LocalPreparation::import(&admin, "victim")
        .unwrap()
        .prepare(b"denied")
        .unwrap();
    let token = admin
        .mint_workspace_token("attacker", Some(600))
        .unwrap()
        .unwrap()
        .token;
    let before = admin.get_heads_state().unwrap();
    assert_eq!(
        send(&cluster, &token, &change.request, before.version).status(),
        403
    );
    // Matching the request label is not enough: retain the forged view change.
    let mut forged = change.request.clone();
    forged.heads.workspace_id = "attacker".into();
    let denied = send(&cluster, &token, &forged, before.version);
    assert_eq!(denied.status(), 403, "{}", denied.text().unwrap());
    assert_eq!(admin.get_heads_state().unwrap().version, before.version);
    assert_eq!(admin.get_heads_state().unwrap().heads, before.heads);
}

#[test]
fn all_publish_crash_windows_recover_and_retry_the_same_prepared_graph() {
    use jj_tandem_repository::CrashWindow;
    for window in CrashWindow::ALL {
        let mut cluster = Cluster::start().unwrap();
        let client = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
        let change = LocalPreparation::import(&client, "writer")
            .unwrap()
            .prepare(b"crash exact\0\xff")
            .unwrap();
        let before = client.get_heads_state().unwrap();
        cluster.faults.crash_at(Some(window));
        assert!(
            !send(
                &cluster,
                &cluster.admin_token,
                &change.request,
                before.version
            )
            .status()
            .is_success(),
            "{window:?}"
        );
        assert!(cluster.halted());
        cluster.cold_restart().unwrap();
        let fresh = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
        let current = fresh.get_heads_state().unwrap();
        if matches!(
            window,
            CrashWindow::BeforeWalWrite | CrashWindow::AfterWalWrite
        ) {
            assert_eq!(current.heads, before.heads, "{window:?}");
        } else {
            assert_published(
                &fresh,
                "writer",
                &change.request.heads.new_id,
                &change.commit_id,
                b"crash exact\0\xff",
            );
        }
        let retry = send(
            &cluster,
            &cluster.admin_token,
            &change.request,
            current.version,
        );
        assert_eq!(retry.status(), 200, "{window:?}: {}", retry.text().unwrap());
        cluster.cold_restart().unwrap();
        let fresh = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
        assert_published(
            &fresh,
            "writer",
            &change.request.heads.new_id,
            &change.commit_id,
            b"crash exact\0\xff",
        );
    }
}

#[test]
fn lost_http_response_retries_without_repreparing_the_change() {
    use std::io::{Read, Write};
    let mut cluster = Cluster::start().unwrap();
    let client = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    let change = LocalPreparation::import(&client, "writer")
        .unwrap()
        .prepare(b"lost reply exact")
        .unwrap();
    let before = client.get_heads_state().unwrap();
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}/api/publish", proxy.local_addr().unwrap());
    let target = cluster.addr.clone();
    let dropped = std::thread::spawn(move || {
        let (mut downstream, _) = proxy.accept().unwrap();
        downstream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut request = Vec::new();
        let header_end = loop {
            let mut byte = [0];
            downstream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break request.len();
            }
        };
        let headers = std::str::from_utf8(&request).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap();
        request.resize(header_end + length, 0);
        downstream.read_exact(&mut request[header_end..]).unwrap();
        let mut upstream = std::net::TcpStream::connect(target).unwrap();
        upstream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        upstream.write_all(&request).unwrap();
        // The server has committed before it emits the 200 status line. Drop
        // the downstream socket without forwarding any response bytes.
        let mut status = Vec::new();
        loop {
            let mut byte = [0];
            upstream.read_exact(&mut byte).unwrap();
            status.push(byte[0]);
            if status.ends_with(b"\r\n") {
                break;
            }
        }
        assert!(std::str::from_utf8(&status).unwrap().contains(" 200 "));
    });
    let response = reqwest::blocking::Client::new()
        .post(proxy_url)
        .bearer_auth(&cluster.admin_token)
        .header(
            "If-Match",
            jj_tandem_protocol::http::etag_for_version(before.version),
        )
        .body(wire::encode_prepared_publish(&change.request))
        .send();
    assert!(response.is_err());
    dropped.join().unwrap();
    cluster.cold_restart().unwrap();
    let fresh = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    assert_published(
        &fresh,
        "writer",
        &change.request.heads.new_id,
        &change.commit_id,
        b"lost reply exact",
    );
    assert_eq!(
        send(
            &cluster,
            &cluster.admin_token,
            &change.request,
            before.version
        )
        .status(),
        412
    );
    assert_eq!(
        send(
            &cluster,
            &cluster.admin_token,
            &change.request,
            fresh.get_heads_state().unwrap().version
        )
        .status(),
        200
    );
    cluster.cold_restart().unwrap();
    let fresh = TandemClient::connect(&cluster.addr, &cluster.admin_token).unwrap();
    assert_published(
        &fresh,
        "writer",
        &change.request.heads.new_id,
        &change.commit_id,
        b"lost reply exact",
    );
}
