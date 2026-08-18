//! The HTTP client that sits under the three jj store traits.
//!
//! `TandemBackend`, `TandemOpStore` and `TandemOpHeadsStore` all share one of
//! these through an `Arc`. Every method is blocking, because the jj traits
//! that call them are driven by `pollster::block_on` on a plain thread.
//!
//! There is no background thread and no channel here. The transport this
//! replaced needed both, because Cap'n Proto's RPC types are `!Send` and had
//! to live on a reactor of their own. `reqwest::blocking::Client` is
//! `Send + Sync + Clone` and pools its own connections, so a call is just a
//! call.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::hex::{from_hex, to_hex};
use crate::wire;

// ─── Compatibility constants ──────────────────────────────────────────────────
//
// What the handshake is checked against is the wire vocabulary itself, so that
// the client cannot come to expect a spelling the server never sends.

use wire::{
    BACKEND_NAME as EXPECTED_BACKEND_NAME, OP_STORE_NAME as EXPECTED_OP_STORE_NAME, PROTOCOL_MAJOR,
    PROTOCOL_MINOR,
};

const ROOT_OPERATION_ID_LENGTH: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Long enough that a publish queued behind a busy server still lands, short
/// enough that a wedged server surfaces as an error instead of a hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const BENCH_INJECT_RTT_MS_ENV: &str = "TANDEM_BENCH_INJECT_RTT_MS";

// ─── Endpoint target ──────────────────────────────────────────────────────────

/// Where a client talks to. `--server` still takes a bare `host:port`, which
/// means `http://host:port`; an explicit `http://` or `https://` URL works too.
#[derive(Debug, Clone)]
pub struct ConnectorTarget {
    base_url: String,
    display: String,
}

impl ConnectorTarget {
    pub fn parse(endpoint: &str) -> Result<Self> {
        if let Some((scheme, rest)) = endpoint.split_once("://") {
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
                if rest.is_empty() {
                    bail!("invalid endpoint: missing host:port in {endpoint:?}");
                }
                let trimmed = endpoint.trim_end_matches('/');
                return Ok(Self {
                    base_url: trimmed.to_string(),
                    display: rest.trim_end_matches('/').to_string(),
                });
            }

            bail!(
                "unsupported tandem transport scheme {scheme:?}; tandem speaks HTTP, so use \
                 host:port or an http:// URL"
            );
        }

        if endpoint.is_empty() {
            bail!("invalid endpoint: missing host:port");
        }

        Ok(Self {
            base_url: format!("http://{}", endpoint.trim_end_matches('/')),
            display: endpoint.to_string(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn display_addr(&self) -> &str {
        &self.display
    }
}

// ─── Public types ─────────────────────────────────────────────────────────────

pub use wire::RepoCapability;

#[derive(Debug, Clone, Default)]
pub struct RepoInfoResponse {
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub backend_name: String,
    pub op_store_name: String,
    pub commit_id_length: usize,
    pub change_id_length: usize,
    pub root_commit_id: Vec<u8>,
    pub root_change_id: Vec<u8>,
    pub empty_tree_id: Vec<u8>,
    pub root_operation_id: Vec<u8>,
    pub capabilities: BTreeSet<RepoCapability>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UpdateHeadsResult {
    pub ok: bool,
    pub heads: Vec<Vec<u8>>,
    pub version: u64,
}

#[derive(Debug, Clone)]
pub struct HeadsState {
    pub heads: Vec<Vec<u8>>,
    pub version: u64,
    pub workspace_heads: BTreeMap<String, Vec<u8>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HeadsSnapshot {
    pub heads: Vec<Vec<u8>>,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrefixResult {
    NoMatch,
    SingleMatch,
    Ambiguous,
}

// ─── TandemClient ─────────────────────────────────────────────────────────────

pub struct TandemClient {
    http: reqwest::blocking::Client,
    target: ConnectorTarget,
    repo_info: RepoInfoResponse,
    injected_rtt: Duration,
}

impl std::fmt::Debug for TandemClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TandemClient")
            .field("server_addr", &self.target.display_addr())
            .finish()
    }
}

/// A client with the timeouts tandem wants and no ambient proxy: the server
/// is usually on a LAN address or localhost, where a system-wide proxy would
/// only get in the way.
pub fn build_http_client(request_timeout: Option<Duration>) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_nodelay(true)
        .no_proxy();
    builder = match request_timeout {
        Some(timeout) => builder.timeout(timeout),
        None => builder.timeout(None),
    };
    builder.build().context("build HTTP client")
}

pub fn bench_injected_rtt_delay() -> Duration {
    let Some(raw_value) = std::env::var(BENCH_INJECT_RTT_MS_ENV).ok() else {
        return Duration::ZERO;
    };

    match raw_value.trim().parse::<u64>() {
        Ok(0) => Duration::ZERO,
        Ok(ms) => Duration::from_millis(ms),
        Err(_) => {
            tracing::warn!(
                env = BENCH_INJECT_RTT_MS_ENV,
                value = %raw_value,
                "ignoring invalid bench RTT injection value"
            );
            Duration::ZERO
        }
    }
}

impl TandemClient {
    pub fn connect(addr: &str) -> Result<Arc<Self>> {
        Self::connect_with_requirements(addr, &[])
    }

    pub fn connect_with_requirements(
        addr: &str,
        required_capabilities: &[RepoCapability],
    ) -> Result<Arc<Self>> {
        let target = ConnectorTarget::parse(addr)?;
        let http = build_http_client(Some(REQUEST_TIMEOUT))?;

        let client = TandemClient {
            http,
            target,
            // Filled in by the handshake immediately below.
            repo_info: RepoInfoResponse::default(),
            injected_rtt: bench_injected_rtt_delay(),
        };

        let repo_info = client
            .fetch_repo_info()
            .map_err(|e| anyhow!("failed to read repo compatibility info from {addr}: {e:#}"))?;
        validate_repo_info(&repo_info, required_capabilities)
            .map_err(|e| anyhow!("server {addr} is incompatible: {e:#}"))?;

        Ok(Arc::new(TandemClient {
            repo_info,
            ..client
        }))
    }

    pub fn server_addr(&self) -> &str {
        self.target.display_addr()
    }

    pub fn repo_info(&self) -> &RepoInfoResponse {
        &self.repo_info
    }

    /// Probe one capability without failing the connection over it. Nothing
    /// in the tree needs this yet — `connect_with_requirements` covers the
    /// hard requirements — but it is the counterpart callers reach for when a
    /// feature is optional, so it stays with the rest of the handshake.
    #[allow(dead_code)]
    pub fn supports_capability(&self, capability: RepoCapability) -> bool {
        self.repo_info.capabilities.contains(&capability)
    }

    // ─── Request plumbing ─────────────────────────────────────────────

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.target.base_url())
    }

    /// The one place a request leaves. Benches inject a round-trip delay here
    /// so that a latency profile can be measured without a real network.
    fn send(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response> {
        if !self.injected_rtt.is_zero() {
            std::thread::sleep(self.injected_rtt);
        }
        let response = request
            .send()
            .with_context(|| format!("request to tandem server {} failed", self.server_addr()))?;
        Ok(response)
    }

    /// Turn a non-success response into the error the server described.
    fn check(
        response: reqwest::blocking::Response,
        what: &str,
    ) -> Result<reqwest::blocking::Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().unwrap_or_default();
        let detail = serde_json::from_str::<wire::ErrorBody>(&body)
            .map(|parsed| parsed.error)
            .unwrap_or(body);
        bail!("{what} failed: HTTP {} — {detail}", status.as_u16())
    }

    /// Read a content-addressed resource: the whole body, or the server's
    /// error. Every `get_*` below is this call and a URL.
    fn get_bytes(&self, path: &str, what: &str) -> Result<Vec<u8>> {
        let response = Self::check(self.send(self.http.get(self.url(path)))?, what)?;
        Ok(response.bytes()?.to_vec())
    }

    /// Write opaque bytes and hand back the checked response, which still
    /// carries the id header and — for objects — the normalized body.
    fn post_octets(
        &self,
        path: &str,
        data: &[u8],
        what: &str,
    ) -> Result<reqwest::blocking::Response> {
        Self::check(
            self.send(
                self.http
                    .post(self.url(path))
                    .header(reqwest::header::CONTENT_TYPE, wire::CONTENT_TYPE_OCTETS)
                    .body(data.to_vec()),
            )?,
            what,
        )
    }

    fn fetch_repo_info(&self) -> Result<RepoInfoResponse> {
        let response = Self::check(
            self.send(self.http.get(self.url("/api/info")))?,
            "repo info",
        )?;
        let body: wire::RepoInfoBody = response.json().context("decode repo info")?;

        let mut capabilities = BTreeSet::new();
        for name in &body.capabilities {
            if let Some(cap) = RepoCapability::from_name(name) {
                capabilities.insert(cap);
            }
        }

        Ok(RepoInfoResponse {
            protocol_major: body.protocol_major,
            protocol_minor: body.protocol_minor,
            backend_name: body.backend_name,
            op_store_name: body.op_store_name,
            commit_id_length: body.commit_id_length as usize,
            change_id_length: body.change_id_length as usize,
            root_commit_id: from_hex(&body.root_commit_id).context("root commit id")?,
            root_change_id: from_hex(&body.root_change_id).context("root change id")?,
            empty_tree_id: from_hex(&body.empty_tree_id).context("empty tree id")?,
            root_operation_id: from_hex(&body.root_operation_id).context("root operation id")?,
            capabilities,
        })
    }

    // ─── Store methods ────────────────────────────────────────────────

    #[allow(dead_code)]
    pub fn get_repo_info(&self) -> Result<RepoInfoResponse> {
        Ok(self.repo_info.clone())
    }

    pub fn get_object(&self, kind: u16, id: &[u8]) -> Result<Vec<u8>> {
        let kind_name =
            wire::kind_name(kind).ok_or_else(|| anyhow!("unknown object kind: {kind}"))?;
        self.get_bytes(
            &format!("/api/objects/{kind_name}/{}", to_hex(id)),
            "get object",
        )
    }

    pub fn put_object(&self, kind: u16, data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let kind_name =
            wire::kind_name(kind).ok_or_else(|| anyhow!("unknown object kind: {kind}"))?;
        let response =
            self.post_octets(&format!("/api/objects/{kind_name}"), data, "put object")?;
        let id = header_id(&response, wire::HEADER_OBJECT_ID)?;
        Ok((id, response.bytes()?.to_vec()))
    }

    /// Write several objects in one round trip.
    ///
    /// Nothing on the jj store traits batches yet, so this is the endpoint's
    /// only caller besides its tests — it exists because a client cache
    /// (stage 4) and a clone (stage 5) both fill from a list of ids.
    #[allow(dead_code)]
    pub fn put_objects_batch(&self, items: &[wire::BatchItem]) -> Result<Vec<wire::BatchOutcome>> {
        let response = Self::check(
            self.send(
                self.http
                    .post(self.url("/api/objects:batch"))
                    .header(reqwest::header::CONTENT_TYPE, wire::CONTENT_TYPE_BATCH)
                    .body(wire::encode_batch_request(items)),
            )?,
            "put objects batch",
        )?;
        wire::decode_batch_response(&response.bytes()?)
            .map_err(|e| anyhow!("decode batch response: {e}"))
    }

    pub fn get_operation(&self, id: &[u8]) -> Result<Vec<u8>> {
        self.get_bytes(&format!("/api/ops/{}", to_hex(id)), "get operation")
    }

    pub fn put_operation(&self, data: &[u8]) -> Result<Vec<u8>> {
        let response = self.post_octets("/api/ops", data, "put operation")?;
        header_id(&response, wire::HEADER_OPERATION_ID)
    }

    pub fn get_view(&self, id: &[u8]) -> Result<Vec<u8>> {
        self.get_bytes(&format!("/api/views/{}", to_hex(id)), "get view")
    }

    pub fn put_view(&self, data: &[u8]) -> Result<Vec<u8>> {
        let response = self.post_octets("/api/views", data, "put view")?;
        header_id(&response, wire::HEADER_VIEW_ID)
    }

    pub fn get_heads_state(&self) -> Result<HeadsState> {
        let response = Self::check(
            self.send(self.http.get(self.url("/api/heads")))?,
            "get heads",
        )?;
        let body: wire::HeadsBody = response.json().context("decode heads")?;
        heads_state_from_body(body)
    }

    pub fn update_op_heads(
        &self,
        old_ids: &[Vec<u8>],
        new_id: &[u8],
        expected_version: u64,
        workspace_id: &str,
    ) -> Result<UpdateHeadsResult> {
        let body = wire::UpdateHeadsBody {
            old_ids: old_ids.iter().map(|id| to_hex(id)).collect(),
            new_id: to_hex(new_id),
            workspace_id: workspace_id.to_string(),
        };

        let response = self.send(
            self.http
                .post(self.url("/api/heads"))
                .header(
                    reqwest::header::IF_MATCH,
                    etag_for_version(expected_version),
                )
                .json(&body),
        )?;

        // A version mismatch is a 412 carrying the state the client lost the
        // race to, which is exactly what its retry loop needs to see. It is a
        // conflict, not a failure, so it is not an error here.
        let conflicted = response.status() == reqwest::StatusCode::PRECONDITION_FAILED;
        let response = if conflicted {
            response
        } else {
            Self::check(response, "update op heads")?
        };

        let heads: wire::HeadsBody = response.json().context("decode heads update result")?;
        let state = heads_state_from_body(heads)?;
        Ok(UpdateHeadsResult {
            ok: !conflicted,
            heads: state.heads,
            version: state.version,
        })
    }

    /// Not part of the HTTP API surface: no server ever implemented it and no
    /// caller needs it. It stays as a `None` so the capability gate above it
    /// keeps its shape.
    #[allow(dead_code)]
    pub fn get_heads_snapshot(&self) -> Result<Option<HeadsSnapshot>> {
        Ok(None)
    }

    /// Copy tracking has no endpoint. It never had a working RPC either — the
    /// server answered `unimplemented` and never advertised the capability —
    /// so `None` here is the same answer the transport this replaced gave.
    pub fn get_related_copies(&self, _copy_id: &[u8]) -> Result<Option<Vec<Vec<u8>>>> {
        Ok(None)
    }

    pub fn resolve_op_prefix(&self, hex_prefix: &str) -> Result<(PrefixResult, Option<Vec<u8>>)> {
        if !hex_prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("operation id prefix {hex_prefix:?} is not hex");
        }
        let response = Self::check(
            self.send(
                self.http
                    .get(self.url(&format!("/api/ops?prefix={hex_prefix}"))),
            )?,
            "resolve operation prefix",
        )?;
        let body: wire::PrefixBody = response.json().context("decode prefix resolution")?;

        let result = match body.resolution.as_str() {
            "singleMatch" => PrefixResult::SingleMatch,
            "ambiguous" => PrefixResult::Ambiguous,
            _ => PrefixResult::NoMatch,
        };
        let matched = match (&result, body.id) {
            (PrefixResult::SingleMatch, Some(hex)) if !hex.is_empty() => {
                Some(from_hex(&hex).context("prefix match id")?)
            }
            _ => None,
        };
        Ok((result, matched))
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// The heads ETag is the CAS version, quoted the way an entity tag must be.
pub fn etag_for_version(version: u64) -> String {
    format!("\"{version}\"")
}

pub fn version_from_etag(etag: &str) -> Option<u64> {
    etag.trim().trim_matches('"').parse::<u64>().ok()
}

fn header_id(response: &reqwest::blocking::Response, header: &str) -> Result<Vec<u8>> {
    let value = response
        .headers()
        .get(header)
        .ok_or_else(|| anyhow!("server response is missing the {header} header"))?
        .to_str()
        .map_err(|e| anyhow!("{header} header is not text: {e}"))?;
    from_hex(value).with_context(|| format!("{header} header is not hex"))
}

fn heads_state_from_body(body: wire::HeadsBody) -> Result<HeadsState> {
    let mut heads = Vec::with_capacity(body.heads.len());
    for hex in &body.heads {
        heads.push(from_hex(hex).context("head id")?);
    }

    let mut workspace_heads = BTreeMap::new();
    for (workspace, hex) in &body.workspace_heads {
        if workspace.is_empty() || hex.is_empty() {
            continue;
        }
        workspace_heads.insert(
            workspace.clone(),
            from_hex(hex).context("workspace head id")?,
        );
    }

    Ok(HeadsState {
        heads,
        version: body.version,
        workspace_heads,
    })
}

fn validate_repo_info(
    info: &RepoInfoResponse,
    required_capabilities: &[RepoCapability],
) -> Result<()> {
    if info.protocol_major != PROTOCOL_MAJOR {
        bail!(
            "repo compatibility mismatch: protocol_major expected {PROTOCOL_MAJOR} but server advertised {}",
            info.protocol_major
        );
    }

    if info.protocol_minor != PROTOCOL_MINOR {
        bail!(
            "repo compatibility mismatch: protocol_minor expected {PROTOCOL_MINOR} but server advertised {}",
            info.protocol_minor
        );
    }

    if info.backend_name != EXPECTED_BACKEND_NAME {
        bail!(
            "repo compatibility mismatch: backend_name expected {:?} but server advertised {:?}",
            EXPECTED_BACKEND_NAME,
            info.backend_name
        );
    }

    if info.op_store_name != EXPECTED_OP_STORE_NAME {
        bail!(
            "repo compatibility mismatch: op_store_name expected {:?} but server advertised {:?}",
            EXPECTED_OP_STORE_NAME,
            info.op_store_name
        );
    }

    if info.commit_id_length == 0 {
        bail!("repo compatibility mismatch: commit_id_length must be > 0");
    }

    if info.change_id_length == 0 {
        bail!("repo compatibility mismatch: change_id_length must be > 0");
    }

    if info.root_commit_id.len() != info.commit_id_length {
        bail!(
            "repo compatibility mismatch: root_commit_id length {} does not match commit_id_length {}",
            info.root_commit_id.len(),
            info.commit_id_length
        );
    }

    if info.root_change_id.len() != info.change_id_length {
        bail!(
            "repo compatibility mismatch: root_change_id length {} does not match change_id_length {}",
            info.root_change_id.len(),
            info.change_id_length
        );
    }

    if info.empty_tree_id.len() != info.commit_id_length {
        bail!(
            "repo compatibility mismatch: empty_tree_id length {} does not match commit_id_length {}",
            info.empty_tree_id.len(),
            info.commit_id_length
        );
    }

    if info.root_operation_id.len() != ROOT_OPERATION_ID_LENGTH {
        bail!(
            "repo compatibility mismatch: root_operation_id length {} does not match expected {}",
            info.root_operation_id.len(),
            ROOT_OPERATION_ID_LENGTH
        );
    }

    for capability in required_capabilities {
        if !info.capabilities.contains(capability) {
            bail!(
                "repo compatibility mismatch: missing required capability {}",
                capability.as_str()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_target_parses_raw_host_port_as_http() {
        let parsed = ConnectorTarget::parse("127.0.0.1:12345").expect("parse endpoint");
        assert_eq!(parsed.base_url(), "http://127.0.0.1:12345");
        assert_eq!(parsed.display_addr(), "127.0.0.1:12345");
    }

    #[test]
    fn connector_target_accepts_an_explicit_http_url() {
        let parsed = ConnectorTarget::parse("http://example.com:8080/").expect("parse endpoint");
        assert_eq!(parsed.base_url(), "http://example.com:8080");
        assert_eq!(parsed.display_addr(), "example.com:8080");
    }

    #[test]
    fn connector_target_rejects_unknown_transport_scheme() {
        let err = ConnectorTarget::parse("wss://example.com:443").expect_err("must reject wss");
        assert!(
            err.to_string()
                .contains("unsupported tandem transport scheme"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn etags_round_trip_a_version() {
        assert_eq!(etag_for_version(0), "\"0\"");
        assert_eq!(version_from_etag(&etag_for_version(42)), Some(42));
        assert_eq!(version_from_etag("not-a-version"), None);
    }
}
