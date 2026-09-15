//! Opt-in matched six-request versus one-request experiment. No daemon change.
use anyhow::{ensure, Context, Result};
use jj_tandem_client::TandemClient;
use jj_tandem_protocol::{hex, http::etag_for_version, wire};
use jj_tandem_test_support::prepared::{verify_published, LocalPreparation};
use serde::Serialize;
use std::time::Instant;

#[derive(Serialize)]
struct Sample {
    mode: &'static str,
    round: usize,
    prepare_us: u128,
    publish_us: u128,
    total_us: u128,
    requests: usize,
    request_body_bytes: usize,
    response_body_bytes: usize,
    commit_id: String,
    operation_id: String,
}

struct Lane {
    mode: &'static str,
    url: String,
    token: String,
    local: LocalPreparation,
    http: reqwest::blocking::Client,
    version: u64,
}

impl Lane {
    fn connect(mode: &'static str, prefix: &str) -> Result<Self> {
        let url = std::env::var(format!("{prefix}_URL")).context("missing experiment URL")?;
        let token =
            std::env::var(format!("{prefix}_TOKEN")).context("missing experiment credential")?;
        let client = TandemClient::connect(&url, &token)?;
        let local = LocalPreparation::import(&client, "bench")?;
        Ok(Self {
            mode,
            url,
            token,
            local,
            http: reqwest::blocking::Client::new(),
            version: client.get_heads_state()?.version,
        })
    }

    fn run(&mut self, round: usize) -> Result<Sample> {
        let payload = format!("round {round}, one edited file\n");
        let start = Instant::now();
        let change = self.local.prepare(payload.as_bytes())?;
        let prepared = Instant::now();
        let mut requests = 0;
        let mut sent = 0;
        let mut received = 0;
        let mut post =
            |path: &str, body: Vec<u8>| -> Result<(reqwest::header::HeaderMap, Vec<u8>)> {
                requests += 1;
                sent += body.len();
                let response = self
                    .http
                    .post(format!("{}{path}", self.url))
                    .bearer_auth(&self.token)
                    .header("If-Match", etag_for_version(self.version))
                    .header(
                        "Content-Type",
                        if path == "/api/heads" {
                            "application/json"
                        } else {
                            "application/octet-stream"
                        },
                    )
                    .body(body)
                    .send()?;
                ensure!(
                    response.status().is_success(),
                    "experiment request failed: {}",
                    response.status()
                );
                let headers = response.headers().clone();
                let bytes = response.bytes()?.to_vec();
                received += bytes.len();
                Ok((headers, bytes))
            };
        let heads = if self.mode == "combined" {
            let (_, bytes) = post(
                "/api/publish",
                wire::encode_prepared_publish(&change.request),
            )?;
            serde_json::from_slice::<wire::HeadsBody>(&bytes)?
        } else {
            for object in &change.request.objects {
                let path = format!("/api/objects/{}", wire::kind_name(object.kind).unwrap());
                let (headers, bytes) = post(&path, object.data.clone())?;
                ensure!(
                    headers[wire::HEADER_OBJECT_ID].to_str()? == hex::to_hex(&object.id),
                    "baseline identity mismatch"
                );
                ensure!(bytes == object.data, "baseline normalization mismatch");
            }
            post(
                "/api/ops:upload",
                wire::encode_operation_upload(&change.request.view, &change.request.operation),
            )?;
            let (_, bytes) = post("/api/heads", serde_json::to_vec(&change.request.heads)?)?;
            serde_json::from_slice::<wire::HeadsBody>(&bytes)?
        };
        let finish = Instant::now();
        self.version = heads.version;
        ensure!(
            requests == if self.mode == "combined" { 1 } else { 6 },
            "unexpected mutation request count"
        );
        let result = Sample {
            mode: self.mode,
            round,
            prepare_us: prepared.duration_since(start).as_micros(),
            publish_us: finish.duration_since(prepared).as_micros(),
            total_us: finish.duration_since(start).as_micros(),
            requests,
            request_body_bytes: sent,
            response_body_bytes: received,
            commit_id: hex::to_hex(change.commit_id.as_bytes()),
            operation_id: change.request.heads.new_id.clone(),
        };
        // Validation deliberately outside the measured preparation/publish span.
        let fresh = TandemClient::connect_with_cache(&self.url, &self.token, &[], None)?;
        verify_published(
            &fresh,
            "bench",
            &hex::from_hex(&change.request.heads.new_id)?,
            &change.commit_id,
            payload.as_bytes(),
        )?;
        self.local.accept(change)?;
        Ok(result)
    }
}

use jj_lib::object_id::ObjectId as _;
fn main() -> Result<()> {
    if std::env::var_os("TANDEM_VERIFY_ONLY").is_some() {
        for (prefix, name, payload) in [
            (
                "TANDEM_EXISTING",
                "bench",
                b"round 42, one edited file\n".as_slice(),
            ),
            (
                "TANDEM_COMBINED",
                "bench",
                b"round 42, one edited file\n".as_slice(),
            ),
            ("TANDEM_AGENT_A", "a", b"remote A exact\0".as_slice()),
            ("TANDEM_AGENT_B", "b", b"remote B exact\xff".as_slice()),
        ] {
            let client = TandemClient::connect_with_cache(
                &std::env::var(format!("{prefix}_URL"))?,
                &std::env::var(format!("{prefix}_TOKEN"))?,
                &[],
                None,
            )?;
            let commit = jj_lib::backend::CommitId::new(hex::from_hex(&std::env::var(format!(
                "{prefix}_COMMIT"
            ))?)?);
            verify_published(
                &client,
                name,
                &hex::from_hex(&std::env::var(format!("{prefix}_OPERATION"))?)?,
                &commit,
                payload,
            )?;
        }
        println!(
            "{}",
            serde_json::json!({"check":"cold-recovery","status":"passed"})
        );
        return Ok(());
    }
    let mut baseline = Lane::connect("existing", "TANDEM_EXISTING")?;
    let mut combined = Lane::connect("combined", "TANDEM_COMBINED")?;
    for round in 0..43 {
        // Alternate order to reduce a monotonic time-of-run bias.
        let samples = if round % 2 == 0 {
            [baseline.run(round)?, combined.run(round)?]
        } else {
            [combined.run(round)?, baseline.run(round)?]
        };
        if round >= 3 {
            for sample in samples {
                println!("{}", serde_json::to_string(&sample)?);
            }
        }
    }
    run_concurrent()?;
    Ok(())
}

fn run_concurrent() -> Result<()> {
    let mut lanes = Vec::new();
    for (prefix, name, payload) in [
        ("TANDEM_AGENT_A", "a", b"remote A exact\0".as_slice()),
        ("TANDEM_AGENT_B", "b", b"remote B exact\xff".as_slice()),
    ] {
        let url = std::env::var(format!("{prefix}_URL"))?;
        let token = std::env::var(format!("{prefix}_TOKEN"))?;
        let client = TandemClient::connect_with_cache(&url, &token, &[], None)?;
        let change = LocalPreparation::import(&client, name)?.prepare(payload)?;
        lanes.push((url, token, client, change, name, payload));
    }
    ensure!(
        lanes[0].0 == lanes[1].0,
        "concurrency requires the same repository URL"
    );
    let version = lanes[0].2.get_heads_state()?.version;
    let barrier = std::sync::Barrier::new(2);
    let statuses = std::thread::scope(|scope| {
        let handles: Vec<_> = lanes
            .iter()
            .map(|(url, token, _, change, _, _)| {
                let barrier = &barrier;
                scope.spawn(move || {
                    let http = reqwest::blocking::Client::new();
                    barrier.wait();
                    http.post(format!("{url}/api/publish"))
                        .bearer_auth(token)
                        .header("If-Match", etag_for_version(version))
                        .body(wire::encode_prepared_publish(&change.request))
                        .send()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut commits = Vec::new();
    for ((url, token, client, change, _, _), response) in lanes.iter().zip(statuses) {
        let status = response?.status();
        if status == reqwest::StatusCode::PRECONDITION_FAILED {
            let retry = reqwest::blocking::Client::new()
                .post(format!("{url}/api/publish"))
                .bearer_auth(token)
                .header(
                    "If-Match",
                    etag_for_version(client.get_heads_state()?.version),
                )
                .body(wire::encode_prepared_publish(&change.request))
                .send()?;
            ensure!(
                retry.status().is_success(),
                "remote concurrent retry failed"
            );
        } else {
            ensure!(status.is_success(), "remote concurrent publish failed");
        }

        commits.push(change.commit_id.hex());
    }
    for (url, token, _, change, name, payload) in &lanes {
        let fresh = TandemClient::connect_with_cache(url, token, &[], None)?;
        verify_published(
            &fresh,
            name,
            &hex::from_hex(&change.request.heads.new_id)?,
            &change.commit_id,
            payload,
        )?;
    }
    println!(
        "{}",
        serde_json::json!({"a_operation": lanes[0].3.request.heads.new_id, "b_operation": lanes[1].3.request.heads.new_id, "check":"concurrency","status":"passed","a_commit":commits[0],"b_commit":commits[1]})
    );
    Ok(())
}
