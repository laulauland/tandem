//! The HTTP API surface, exercised directly rather than through jj.
//!
//! The slice tests prove that jj still works over the new transport. This
//! file proves the transport itself: the endpoints the design doc names, the
//! cache headers on the immutable ones, the ETag/If-Match CAS on the mutable
//! one, and the SSE wake-ups.

mod common;

use std::io::{BufRead, BufReader};
use std::time::Duration;

use tempfile::TempDir;

const REQUEST_MAGIC: &[u8; 4] = b"TBQ1";
const RESPONSE_MAGIC: &[u8; 4] = b"TBS1";

struct Fixture {
    _tmp: TempDir,
    addr: String,
    home: std::path::PathBuf,
    workspace: std::path::PathBuf,
    server: std::process::Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// A server and an initialized workspace, over the home directory the caller
/// prepared. It is a function rather than inline setup so that every test —
/// including the one that has to write a jj config before the first command
/// runs — gets a `Fixture`, and with it the `Drop` that reaps the server
/// however the test ends.
fn fixture_with_home(tmp: TempDir, home: std::path::PathBuf) -> Fixture {
    let server_repo = tmp.path().join("server-repo");
    std::fs::create_dir_all(&server_repo).unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let addr = common::free_addr();
    let mut server = common::spawn_server(&server_repo, &addr);
    common::wait_for_server(&addr, &mut server);

    let init = common::run_tandem_in(&workspace, &["init", "--server", &addr, "."], &home);
    common::assert_ok(&init, "tandem init");

    Fixture {
        _tmp: tmp,
        addr,
        home,
        workspace,
        server,
    }
}

/// A server with one workspace that has already published something, so the
/// head version is past zero and there are real objects to read.
fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());
    let fx = fixture_with_home(tmp, home);

    std::fs::write(fx.workspace.join("hello.txt"), b"hello over http\n").unwrap();
    let new = common::run_tandem_in(&fx.workspace, &["new", "-m", "http surface"], &fx.home);
    common::assert_ok(&new, "jj new");

    fx
}

/// `TBQ1 | count:u32 | { kind:u16, len:u32, data }*`, spelled out by hand.
///
/// The point of this file is to check the wire format from the outside, so it
/// does not reach for `src/wire.rs` — a bug shared by the encoder and the test
/// would cancel itself out. What it does avoid is spelling the same arithmetic
/// twice within the file.
fn encode_request_frame(items: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut frame = REQUEST_MAGIC.to_vec();
    frame.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (kind, data) in items {
        frame.extend_from_slice(&kind.to_le_bytes());
        frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
        frame.extend_from_slice(data);
    }
    frame
}

/// The other direction: `TBS1 | count:u32 | { status:u8, blob, blob }*`, walked
/// record by record. Answers the status and the id of each, and insists the
/// frame ends exactly where its count says it does.
fn walk_response_frame(body: &[u8]) -> Vec<(u8, Vec<u8>)> {
    assert_eq!(&body[..4], RESPONSE_MAGIC);
    let count = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;

    let mut pos = 8;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let status = body[pos];
        pos += 1;
        let id_len = u32::from_le_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let id = body[pos..pos + id_len].to_vec();
        pos += id_len;
        let payload_len = u32::from_le_bytes(body[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4 + payload_len;
        records.push((status, id));
    }
    assert_eq!(pos, body.len(), "the frame ends where it says it does");
    records
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn json_at(addr: &str, path: &str) -> serde_json::Value {
    common::api_get(addr, path)
        .json()
        .expect("decode JSON body")
}

#[test]
fn info_endpoint_carries_the_compatibility_handshake() {
    let fx = fixture();
    let info = json_at(&fx.addr, "/api/info");

    assert_eq!(info["protocolMajor"], 0);
    assert_eq!(info["protocolMinor"], 1);
    assert_eq!(info["backendName"], "tandem");
    assert_eq!(info["opStoreName"], "tandem_op_store");
    assert!(
        info["commitIdLength"].as_u64().unwrap() > 0,
        "commitIdLength must be set: {info}"
    );
    assert_eq!(
        info["rootOperationId"].as_str().unwrap().len(),
        128,
        "the root operation id is 64 bytes of hex"
    );
    assert_eq!(
        info["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["watchHeads"]
    );
}

#[test]
fn object_reads_are_immutable_and_writes_answer_with_the_id() {
    let fx = fixture();

    // A file blob written through jj is readable by its content address.
    let data = b"a blob written straight over http\n".to_vec();
    let written = common::http_client()
        .post(common::api_url(&fx.addr, "/api/objects/file"))
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(data.clone())
        .send()
        .expect("POST /api/objects/file");
    assert!(written.status().is_success(), "{:?}", written.status());

    let id = written
        .headers()
        .get("tandem-object-id")
        .expect("tandem-object-id header")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        written.bytes().unwrap().to_vec(),
        data,
        "a file write answers with its normalized bytes"
    );

    let read = common::api_get(&fx.addr, &format!("/api/objects/file/{id}"));
    let cache_control = read
        .headers()
        .get(reqwest::header::CACHE_CONTROL)
        .expect("Cache-Control on a content-addressed read")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        cache_control.contains("immutable"),
        "content-addressed reads must be immutable, got {cache_control:?}"
    );
    assert_eq!(read.bytes().unwrap().to_vec(), data);
}

#[test]
fn an_unknown_object_kind_is_a_bad_request_and_a_missing_id_is_a_not_found() {
    let fx = fixture();
    let client = common::http_client();

    let bad_kind = client
        .get(common::api_url(&fx.addr, "/api/objects/banana/00"))
        .send()
        .unwrap();
    assert_eq!(bad_kind.status().as_u16(), 400);
    let body: serde_json::Value = bad_kind.json().unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("banana"),
        "the error should name the kind it refused: {body}"
    );

    let missing = client
        .get(common::api_url(
            &fx.addr,
            &format!("/api/objects/file/{}", "ab".repeat(20)),
        ))
        .send()
        .unwrap();
    assert_eq!(missing.status().as_u16(), 404);
}

#[test]
fn the_batch_endpoint_writes_every_item_and_reports_each_one() {
    let fx = fixture();

    // Two good file blobs and one commit blob that is not a valid proto.
    let good_a = b"batch blob a\n".to_vec();
    let good_b = b"batch blob b\n".to_vec();
    let frame = encode_request_frame(&[
        (2u16, good_a.clone()),
        (2u16, good_b.clone()),
        (0u16, b"\xff\xff\xff not a commit proto".to_vec()),
    ]);

    let response = common::http_client()
        .post(common::api_url(&fx.addr, "/api/objects:batch"))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/vnd.tandem.batch",
        )
        .body(frame)
        .send()
        .expect("POST /api/objects:batch");
    assert!(response.status().is_success(), "{:?}", response.status());

    let body = response.bytes().unwrap().to_vec();
    let records = walk_response_frame(&body);
    assert_eq!(records.len(), 3, "one record per item");
    assert_eq!(
        records
            .iter()
            .map(|(status, _)| *status)
            .collect::<Vec<_>>(),
        vec![0, 0, 1],
        "the bad commit proto is reported"
    );

    // The two blobs are now readable at their ids.
    let hex = hex_of(&records[0].1);
    let read = common::api_get(&fx.addr, &format!("/api/objects/file/{hex}"));
    assert_eq!(read.bytes().unwrap().to_vec(), good_a);
}

#[test]
fn a_malformed_batch_frame_is_refused_without_a_crash() {
    let fx = fixture();
    let client = common::http_client();

    for body in [
        b"not a frame at all".to_vec(),
        {
            // Right magic, a count that the frame cannot possibly hold.
            let mut frame = REQUEST_MAGIC.to_vec();
            frame.extend_from_slice(&u32::MAX.to_le_bytes());
            frame
        },
        {
            // Right magic and count, a blob length that runs off the end.
            let mut frame = REQUEST_MAGIC.to_vec();
            frame.extend_from_slice(&1u32.to_le_bytes());
            frame.extend_from_slice(&2u16.to_le_bytes());
            frame.extend_from_slice(&u32::MAX.to_le_bytes());
            frame
        },
    ] {
        let response = client
            .post(common::api_url(&fx.addr, "/api/objects:batch"))
            .body(body)
            .send()
            .expect("POST a bad batch frame");
        assert_eq!(
            response.status().as_u16(),
            400,
            "a bad frame is a bad request"
        );
    }

    // The server is still alive and answering.
    let _ = json_at(&fx.addr, "/api/heads");
}

#[test]
fn heads_carry_an_etag_and_a_stale_if_match_is_a_conflict() {
    let fx = fixture();
    let client = common::http_client();

    let response = client
        .get(common::api_url(&fx.addr, "/api/heads"))
        .send()
        .unwrap();
    assert!(response.status().is_success());
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .expect("heads carry an ETag")
        .to_str()
        .unwrap()
        .to_string();
    let heads: serde_json::Value = response.json().unwrap();
    let version = heads["version"].as_u64().unwrap();
    assert!(version > 0, "the fixture already published: {heads}");
    assert_eq!(etag, format!("\"{version}\""));

    let head_id = heads["heads"][0].as_str().expect("a head").to_string();

    // A publish against a version that has moved on is a 412 carrying the
    // state the caller lost the race to.
    let stale = client
        .post(common::api_url(&fx.addr, "/api/heads"))
        .header(reqwest::header::IF_MATCH, "\"0\"")
        .json(&serde_json::json!({
            "oldIds": [head_id],
            "newId": head_id,
            "workspaceId": "",
        }))
        .send()
        .unwrap();
    assert_eq!(stale.status().as_u16(), 412);
    let conflict: serde_json::Value = stale.json().unwrap();
    assert_eq!(
        conflict["version"].as_u64().unwrap(),
        version,
        "a conflict reports the version that won"
    );

    // A publish with no If-Match at all is refused outright.
    let unconditional = client
        .post(common::api_url(&fx.addr, "/api/heads"))
        .json(&serde_json::json!({ "newId": head_id }))
        .send()
        .unwrap();
    assert_eq!(unconditional.status().as_u16(), 428);
}

#[test]
fn operations_and_views_are_readable_and_prefixes_resolve() {
    let fx = fixture();

    let heads = json_at(&fx.addr, "/api/heads");
    let head_id = heads["heads"][0].as_str().expect("a head").to_string();

    let operation = common::api_get(&fx.addr, &format!("/api/ops/{head_id}"));
    assert!(
        operation
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("immutable"),
        "operations are content-addressed too"
    );
    assert!(!operation.bytes().unwrap().is_empty());

    let single = json_at(&fx.addr, &format!("/api/ops?prefix={}", &head_id[..8]));
    assert_eq!(single["resolution"], "singleMatch");
    assert_eq!(single["id"].as_str().unwrap(), head_id);

    let none = json_at(&fx.addr, "/api/ops?prefix=ffffffffffffffff");
    assert_eq!(none["resolution"], "noMatch");

    // Every operation in the store shares the empty prefix.
    let ambiguous = json_at(&fx.addr, "/api/ops?prefix=");
    assert_eq!(ambiguous["resolution"], "ambiguous");
}

#[test]
fn the_event_stream_wakes_a_reader_when_the_heads_move() {
    let fx = fixture();

    let response = common::http_client()
        .get(common::api_url(&fx.addr, "/api/events"))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .timeout(Duration::from_secs(20))
        .send()
        .expect("GET /api/events");
    assert!(response.status().is_success());
    assert!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "the events endpoint is an SSE stream"
    );

    // Read the stream on a thread so the commit below can move the heads.
    let reader = std::thread::spawn(move || {
        let mut lines = BufReader::new(response).lines();
        while let Some(Ok(line)) = lines.next() {
            if let Some(payload) = line.strip_prefix("data:") {
                return payload.trim().to_string();
            }
        }
        String::new()
    });

    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(fx.workspace.join("wake.txt"), b"wake up\n").unwrap();
    let new = common::run_tandem_in(&fx.workspace, &["new", "-m", "wake the stream"], &fx.home);
    common::assert_ok(&new, "jj new to move the heads");

    let payload = reader.join().expect("event reader thread");
    let event: serde_json::Value =
        serde_json::from_str(&payload).unwrap_or_else(|e| panic!("event {payload:?}: {e}"));
    assert!(
        event["version"].as_u64().unwrap() > 0,
        "the wake-up names the version that happened: {event}"
    );
}

// ─── Request size ─────────────────────────────────────────────────────────────
//
// axum caps a `Bytes` body at 2 MiB unless told otherwise, and nothing else in
// the suite writes an object that big — every slice test commits a handful of
// short text files. The two tests below are the ones that would have caught a
// default left in place: the first writes one blob straight over HTTP, the
// second commits a tracked file of the same size through jj, which is how a
// real workspace hits the ceiling.

/// A blob comfortably over axum's 2 MiB default, and not a round number of
/// pages, so a truncation shows up as a length mismatch rather than a clean
/// prefix.
fn oversized_blob() -> Vec<u8> {
    let mut data = Vec::with_capacity(3 * 1024 * 1024 + 17);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    while data.len() < 3 * 1024 * 1024 + 17 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        data.extend_from_slice(&seed.to_le_bytes());
    }
    data.truncate(3 * 1024 * 1024 + 17);
    data
}

#[test]
fn an_object_past_the_default_body_limit_is_written_and_read_back_whole() {
    let fx = fixture();
    let data = oversized_blob();

    let written = common::http_client()
        .post(common::api_url(&fx.addr, "/api/objects/file"))
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(data.clone())
        .send()
        .expect("POST a 3 MiB blob");
    assert!(
        written.status().is_success(),
        "a 3 MiB object must not be refused: HTTP {}",
        written.status().as_u16()
    );
    let id = written
        .headers()
        .get("tandem-object-id")
        .expect("tandem-object-id header")
        .to_str()
        .unwrap()
        .to_string();

    let read = common::api_get(&fx.addr, &format!("/api/objects/file/{id}"));
    assert_eq!(
        read.bytes().unwrap().to_vec(),
        data,
        "the blob must come back byte for byte"
    );
}

#[test]
fn a_batch_past_the_default_body_limit_writes_every_item() {
    let fx = fixture();
    let big = oversized_blob();
    let small = b"riding along with a big one\n".to_vec();

    let frame = encode_request_frame(&[(2u16, big.clone()), (2u16, small)]);

    let response = common::http_client()
        .post(common::api_url(&fx.addr, "/api/objects:batch"))
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/vnd.tandem.batch",
        )
        .body(frame)
        .send()
        .expect("POST an oversized batch");
    assert!(
        response.status().is_success(),
        "a batch over 2 MiB must not be refused: HTTP {}",
        response.status().as_u16()
    );

    let body = response.bytes().unwrap().to_vec();
    let records = walk_response_frame(&body);
    assert_eq!(records.len(), 2);
    for (index, (status, _)) in records.iter().enumerate() {
        assert_eq!(*status, 0, "item {index} should have been written");
    }

    let hex = hex_of(&records[0].1);
    let read = common::api_get(&fx.addr, &format!("/api/objects/file/{hex}"));
    assert_eq!(read.bytes().unwrap().to_vec(), big);
}

#[test]
fn a_tracked_file_past_the_default_body_limit_commits_and_reads_back() {
    let tmp = TempDir::new().unwrap();
    let home = common::isolated_home(tmp.path());

    // jj will not snapshot a new file over 1 MiB unless told to, and that
    // refusal would mask the thing under test. Write the config before the
    // first command so the test isolation does not fill it in with the
    // default.
    let config_dir = home.join(".config").join("jj");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "user.name = \"Test User\"\nuser.email = \"test@tandem.dev\"\n\
         [fsmonitor]\nbackend = \"none\"\n\
         [snapshot]\nmax-new-file-size = \"64MiB\"\n",
    )
    .unwrap();

    let fx = fixture_with_home(tmp, home);

    let data = oversized_blob();
    std::fs::write(fx.workspace.join("big.bin"), &data).unwrap();

    let new = common::run_tandem_in(
        &fx.workspace,
        &["new", "-m", "a big tracked file"],
        &fx.home,
    );
    common::assert_ok(&new, "commit a 3 MiB tracked file");

    let show = common::run_tandem_in(
        &fx.workspace,
        &["file", "show", "-r", "@-", "big.bin"],
        &fx.home,
    );
    common::assert_ok(&show, "read the big file back");
    assert_eq!(
        show.stdout, data,
        "the tracked file must survive the round trip"
    );
}
