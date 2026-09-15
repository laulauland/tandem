//! Observe the existing S3 transport below the SDK retry loop. No headers,
//! signed URLs, bodies or unclassified error messages enter these events.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use object_store::client::{
    ClientOptions, HttpClient, HttpConnector, HttpError, HttpRequest, HttpResponse, HttpService,
    ReqwestConnector,
};

pub(crate) fn key_class(key: &str) -> &'static str {
    if key.split('/').any(|part| part == "wal") {
        "wal"
    } else if key.split('/').any(|part| part == "index") {
        "index"
    } else {
        "other"
    }
}

static NEXT_CALL: AtomicU64 = AtomicU64::new(1);
tokio::task_local! { static CALL: Call; }
struct Call {
    id: u64,
    state: Mutex<AttemptState>,
}
#[derive(Default)]
struct AttemptState {
    count: u64,
    ended: Option<Instant>,
    outcome: String,
}

pub(crate) async fn observe<F: std::future::Future>(future: F) -> F::Output {
    CALL.scope(
        Call {
            id: NEXT_CALL.fetch_add(1, Ordering::Relaxed),
            state: Mutex::new(AttemptState::default()),
        },
        future,
    )
    .await
}

#[derive(Debug)]
pub(crate) struct ObservedConnector;
impl HttpConnector for ObservedConnector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(ObservedService(
            ReqwestConnector::default().connect(options)?,
        )))
    }
}
#[derive(Debug)]
struct ObservedService(HttpClient);
fn epoch_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

#[async_trait::async_trait]
impl HttpService for ObservedService {
    async fn call(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let start = Instant::now();
        let start_epoch_us = epoch_us();
        let method = request.method().to_string();
        let context = CALL
            .try_with(|call| {
                let mut state = call.state.lock().unwrap();
                state.count += 1;
                (
                    call.id,
                    state.count,
                    state
                        .ended
                        .map(|end| start.duration_since(end).as_micros() as u64)
                        .unwrap_or(0),
                    state.outcome.clone(),
                )
            })
            .ok();
        let (call_id, attempt, inter_attempt_us, previous_outcome) =
            context.unwrap_or((0, 1, 0, String::new()));
        tracing::debug!(call_id, attempt, %method, start_epoch_us, inter_attempt_us, %previous_outcome, "S3 HTTP attempt started");
        let result = self.0.execute(request).await;
        let end_epoch_us = epoch_us();
        let elapsed_us = start.elapsed().as_micros() as u64;
        let status = result
            .as_ref()
            .ok()
            .map(|response| response.status().as_u16());
        let etag_present = result
            .as_ref()
            .ok()
            .map(|response| response.headers().contains_key("etag"));
        let outcome = match &result {
            Ok(response) => format!("http_{}", response.status().as_u16()),
            Err(error) => format!("transport_{:?}", error.kind()),
        };
        let connection = result.as_ref().ok().and_then(|response| {
            response
                .extensions()
                .get::<hyper_util::client::legacy::connect::HttpInfo>()
        });
        let local_socket = connection.map(|info| info.local_addr().to_string());
        let remote_socket = connection.map(|info| info.remote_addr().to_string());
        tracing::debug!(call_id, attempt, %method, start_epoch_us, end_epoch_us, elapsed_us, status, etag_present, inter_attempt_us, %previous_outcome, %outcome, local_socket, remote_socket, end_boundary = "response_headers_or_transport_error", inter_attempt_scope = "SDK handling, signing, backoff and scheduling; not isolated sleep", retry_reason_coverage = "preceding response status or transport error kind; SDK decision not exposed", transport_timing_coverage = "socket addresses when exposed; DNS/TCP/TLS timings unavailable", "S3 HTTP attempt finished");
        let _ = CALL.try_with(|call| {
            let mut state = call.state.lock().unwrap();
            state.ended = Some(Instant::now());
            state.outcome = outcome;
        });
        result
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Capture(std::sync::Arc<Mutex<Vec<u8>>>);
#[cfg(test)]
pub(crate) fn test_capture() -> Capture {
    Capture::default()
}
#[cfg(test)]
impl Capture {
    pub(crate) fn events(&self) -> Vec<serde_json::Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
#[cfg(test)]
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
