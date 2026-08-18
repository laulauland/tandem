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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::cache::{DiskCache, NAMESPACE_OPERATION, NAMESPACE_VIEW};
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

/// What the server answered a writer-role claim with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriterClaim {
    /// The role is this holder's for `expires_in`, after which somebody else
    /// may take it. A holder that means to keep it asks again before then.
    Held { holder: String, expires_in: Duration },
    /// Somebody else holds it. `detail` is the server's sentence about who,
    /// and for how much longer.
    Refused { detail: String },
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
    /// The bearer every request carries. A server refuses a request without
    /// one, the handshake included, so this is set before the first byte
    /// leaves rather than being attached by whoever remembers to.
    token: String,
    repo_info: RepoInfoResponse,
    injected_rtt: Duration,
    /// Where an id that has already been fetched on this machine comes from
    /// the second time. `None` when the cache is switched off or the machine
    /// has nowhere to put one.
    ///
    /// The three jj store traits each build their own client, so nothing is
    /// shared between them in memory — the sharing is the directory, which is
    /// also what makes it survive the end of the process and reach the next
    /// command and the next workspace.
    cache: Option<Arc<DiskCache>>,
    /// How many requests have left this client. Tests assert on it: "served
    /// from cache" has to mean no request happened, not that one was fast.
    requests_sent: AtomicU64,
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
    pub fn connect(addr: &str, token: &str) -> Result<Arc<Self>> {
        Self::connect_with_requirements(addr, token, &[])
    }

    pub fn connect_with_requirements(
        addr: &str,
        token: &str,
        required_capabilities: &[RepoCapability],
    ) -> Result<Arc<Self>> {
        Self::connect_with_cache(
            addr,
            token,
            required_capabilities,
            DiskCache::from_environment(),
        )
    }

    /// The same connection with a cache the caller chose, rather than the one
    /// the environment names. Tests use it to keep a cache directory per test
    /// without writing to a process-wide environment.
    pub fn connect_with_cache(
        addr: &str,
        token: &str,
        required_capabilities: &[RepoCapability],
        cache: Option<Arc<DiskCache>>,
    ) -> Result<Arc<Self>> {
        let target = ConnectorTarget::parse(addr)?;
        let http = build_http_client(Some(REQUEST_TIMEOUT))?;

        let client = TandemClient {
            http,
            target,
            token: token.to_string(),
            // Filled in by the handshake immediately below.
            repo_info: RepoInfoResponse::default(),
            injected_rtt: bench_injected_rtt_delay(),
            cache,
            requests_sent: AtomicU64::new(0),
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
    /// so that a latency profile can be measured without a real network, and
    /// the counter is here for the same reason it is the only honest place for
    /// it: nothing reaches the server without passing through.
    fn send(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response> {
        self.requests_sent.fetch_add(1, Ordering::Relaxed);
        if !self.injected_rtt.is_zero() {
            std::thread::sleep(self.injected_rtt);
        }
        let response = request
            .bearer_auth(&self.token)
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

    /// The same read, but the disk answers first.
    ///
    /// Only a body the server actually returned is cached. A 404 and a failed
    /// read are both left uncached on purpose: the server takes care to keep
    /// "this object does not exist" and "this disk would not answer" apart,
    /// and a client that wrote either to disk would turn a fault into a fact.
    fn cached_get(&self, namespace: &str, id: &[u8], path: &str, what: &str) -> Result<Vec<u8>> {
        if let Some(cache) = &self.cache {
            if let Some(data) = cache.get(namespace, id) {
                return Ok(data);
            }
        }

        let data = self.get_bytes(path, what)?;
        self.store_in_cache(namespace, id, &data);
        Ok(data)
    }

    /// Put bytes in the cache if this client has one. Both the read that had
    /// to go to the server and the write that already knows the id fill the
    /// cache through here.
    fn store_in_cache(&self, namespace: &str, id: &[u8], data: &[u8]) {
        if let Some(cache) = &self.cache {
            cache.put(namespace, id, data);
        }
    }

    /// How many requests this client has sent, cache hits excluded by
    /// construction.
    pub fn requests_sent(&self) -> u64 {
        self.requests_sent.load(Ordering::Relaxed)
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

    /// The bearer this client presents, so that a caller holding one client
    /// can hand the same credential to the next one it builds.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Ask for a workspace-scoped bearer.
    ///
    /// `Ok(None)` means the server refused this client's own token for the
    /// job, which is what a workspace token gets: only the admin token mints.
    /// The caller reads that as "what I am holding is already the workspace
    /// token", which is what makes `tandem init --token` take either one.
    pub fn mint_workspace_token(
        &self,
        workspace_id: &str,
        ttl_seconds: Option<u64>,
    ) -> Result<Option<wire::TokenBody>> {
        let request = wire::MintTokenBody {
            workspace_id: workspace_id.to_string(),
            ttl_seconds,
        };
        let response = self.send(self.http.post(self.url("/api/tokens")).json(&request))?;
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            return Ok(None);
        }
        let response = Self::check(response, "mint token")?;
        let body: wire::TokenBody = response.json().context("decode minted token")?;
        Ok(Some(body))
    }

    /// Take the writer role for a workspace, or keep it.
    ///
    /// The same holder asking again renews; anybody else asking while the
    /// current claim stands is refused, and the refusal says who holds it.
    /// A refusal is an answer rather than an error: a daemon that lost the
    /// race has to keep running and keep asking, not fall over.
    pub fn claim_writer_role(
        &self,
        workspace_id: &str,
        holder: &str,
        ttl: Option<Duration>,
    ) -> Result<WriterClaim> {
        let request = wire::ClaimWriterBody {
            holder: holder.to_string(),
            ttl_seconds: ttl.map(|ttl| ttl.as_secs()),
        };
        let response = self.send(
            self.http
                .post(self.url(&format!("/api/workspaces/{workspace_id}/writer")))
                .json(&request),
        )?;

        if response.status() == reqwest::StatusCode::CONFLICT {
            let body = response.text().unwrap_or_default();
            let detail = serde_json::from_str::<wire::ErrorBody>(&body)
                .map(|parsed| parsed.error)
                .unwrap_or(body);
            return Ok(WriterClaim::Refused { detail });
        }

        let response = Self::check(response, "claim writer role")?;
        let body: wire::WriterRoleBody = response.json().context("decode writer role")?;
        Ok(WriterClaim::Held {
            holder: body.holder,
            expires_in: Duration::from_secs(body.expires_in_seconds),
        })
    }

    pub fn get_object(&self, kind: u16, id: &[u8]) -> Result<Vec<u8>> {
        let kind_name =
            wire::kind_name(kind).ok_or_else(|| anyhow!("unknown object kind: {kind}"))?;
        // The kind is part of the cache key, not decoration: a file and a
        // symlink holding the same bytes are the same git blob and so share an
        // id. The URL separates them for the same reason.
        self.cached_get(
            kind_name,
            id,
            &format!("/api/objects/{kind_name}/{}", to_hex(id)),
            "get object",
        )
    }

    /// Write an object and hand back its id and the bytes the server settled
    /// on.
    ///
    /// What goes into the cache is the response body, not what the caller
    /// handed in: the server normalizes some kinds on the way through, and a
    /// cache that answered with the pre-normalized form would be answering a
    /// different question than the reader asked.
    pub fn put_object(&self, kind: u16, data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let kind_name =
            wire::kind_name(kind).ok_or_else(|| anyhow!("unknown object kind: {kind}"))?;
        let response =
            self.post_octets(&format!("/api/objects/{kind_name}"), data, "put object")?;
        let id = header_id(&response, wire::HEADER_OBJECT_ID)?;
        let normalized = response.bytes()?.to_vec();
        self.store_in_cache(kind_name, &id, &normalized);
        Ok((id, normalized))
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

    // Operations and views are content-addressed too — the server hashes each
    // one and answers with `Cache-Control: immutable`, exactly as it does for
    // objects — so they are cached on the same terms. Leaving them out would
    // have left most of the chatter uncached: every command walks operation
    // ancestry before it reads a single file.
    //
    // The write path stores the bytes that were sent rather than a response
    // body, because there is none: the server keeps what it was given and
    // answers with the id alone.

    pub fn get_operation(&self, id: &[u8]) -> Result<Vec<u8>> {
        self.cached_get(
            NAMESPACE_OPERATION,
            id,
            &format!("/api/ops/{}", to_hex(id)),
            "get operation",
        )
    }

    pub fn put_operation(&self, data: &[u8]) -> Result<Vec<u8>> {
        let response = self.post_octets("/api/ops", data, "put operation")?;
        let id = header_id(&response, wire::HEADER_OPERATION_ID)?;
        self.store_in_cache(NAMESPACE_OPERATION, &id, data);
        Ok(id)
    }

    pub fn get_view(&self, id: &[u8]) -> Result<Vec<u8>> {
        self.cached_get(
            NAMESPACE_VIEW,
            id,
            &format!("/api/views/{}", to_hex(id)),
            "get view",
        )
    }

    pub fn put_view(&self, data: &[u8]) -> Result<Vec<u8>> {
        let response = self.post_octets("/api/views", data, "put view")?;
        let id = header_id(&response, wire::HEADER_VIEW_ID)?;
        self.store_in_cache(NAMESPACE_VIEW, &id, data);
        Ok(id)
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

    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    use crate::wire::KIND_FILE;

    // ─── A server that counts ─────────────────────────────────────────
    //
    // The cache's whole claim is that a hit costs nothing, and "nothing" is a
    // statement about the network, not about the clock. Counting at the far
    // end of a real socket is the only way to say it without believing the
    // client's own bookkeeping. Fifty lines of HTTP/1.1 is cheaper than a
    // mocking crate, and the tree has no mocking crate for a reason.

    /// What every request in these tests presents. The counting server does
    /// not check it — the server's own tests do that — but it records it, so
    /// that "the client sends the token it was given" is a fact a test can
    /// state at the far end of a socket.
    const TEST_TOKEN: &str = "tdmw_testtoken";

    struct CountingServer {
        addr: String,
        requests: Arc<AtomicU64>,
        authorizations: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        acceptor: Option<std::thread::JoinHandle<()>>,
    }

    impl CountingServer {
        fn start(objects: HashMap<String, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the counting server");
            let addr = listener.local_addr().expect("local addr").to_string();
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");

            let requests = Arc::new(AtomicU64::new(0));
            let authorizations = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let objects = Arc::new(objects);

            let acceptor = {
                let requests = Arc::clone(&requests);
                let authorizations = Arc::clone(&authorizations);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let requests = Arc::clone(&requests);
                                let authorizations = Arc::clone(&authorizations);
                                let objects = Arc::clone(&objects);
                                std::thread::spawn(move || {
                                    serve(stream, requests, authorizations, objects)
                                });
                            }
                            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };

            Self {
                addr,
                requests,
                authorizations,
                stop,
                acceptor: Some(acceptor),
            }
        }

        fn requests(&self) -> u64 {
            self.requests.load(Ordering::Relaxed)
        }

        fn authorizations(&self) -> Vec<String> {
            self.authorizations.lock().expect("authorizations").clone()
        }
    }

    impl Drop for CountingServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.acceptor.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve(
        stream: std::net::TcpStream,
        requests: Arc<AtomicU64>,
        authorizations: Arc<Mutex<Vec<String>>>,
        objects: Arc<HashMap<String, Vec<u8>>>,
    ) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let mut writer = stream.try_clone().expect("clone the stream");
        let mut reader = BufReader::new(stream);

        loop {
            let mut request_line = String::new();
            match reader.read_line(&mut request_line) {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => return,
            }
            let Some(path) = request_line.split_whitespace().nth(1).map(str::to_string) else {
                return;
            };

            // Headers, so the body length is known and the connection stays
            // framed for the next request on it.
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(_) => return,
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some(value) = header
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim())
                {
                    authorizations
                        .lock()
                        .expect("authorizations")
                        .push(value.to_string());
                }
                if let Some(value) = header
                    .split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim())
                {
                    content_length = value.parse().unwrap_or(0);
                }
            }
            if content_length > 0 {
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
            }

            requests.fetch_add(1, Ordering::Relaxed);

            let response = if path == "/api/info" {
                let body = serde_json::to_vec(&repo_info_body()).expect("encode repo info");
                http_response(200, "application/json", &body)
            } else if let Some(body) = objects.get(&path) {
                http_response(200, wire::CONTENT_TYPE_OCTETS, body)
            } else {
                http_response(404, "application/json", br#"{"error":"not found"}"#)
            };

            if writer.write_all(&response).is_err() || writer.flush().is_err() {
                return;
            }
        }
    }

    fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
        let reason = if status == 200 { "OK" } else { "Not Found" };
        let mut out = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn repo_info_body() -> wire::RepoInfoBody {
        wire::RepoInfoBody {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            tandem_version: "test".to_string(),
            backend_name: EXPECTED_BACKEND_NAME.to_string(),
            op_store_name: EXPECTED_OP_STORE_NAME.to_string(),
            commit_id_length: 20,
            change_id_length: 16,
            root_commit_id: to_hex(&[0u8; 20]),
            root_change_id: to_hex(&[0u8; 16]),
            empty_tree_id: to_hex(&[1u8; 20]),
            root_operation_id: to_hex(&[0u8; ROOT_OPERATION_ID_LENGTH]),
            capabilities: Vec::new(),
        }
    }

    /// Every file under a directory, so a test can reach into the cache
    /// without the cache having to expose where it put things.
    fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files_under(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    fn cache_at(dir: &std::path::Path) -> Option<Arc<DiskCache>> {
        Some(Arc::new(DiskCache::open(dir)))
    }

    // ─── Cache behaviour at the client boundary ───────────────────────

    #[test]
    fn a_cached_object_is_served_with_no_request_at_all() {
        let id = vec![0xab, 0xcd, 0xef];
        let payload = b"the bytes of one file object".to_vec();
        let server = CountingServer::start(HashMap::from([(
            format!("/api/objects/file/{}", to_hex(&id)),
            payload.clone(),
        )]));
        let tmp = tempfile::tempdir().expect("temp dir");

        let client =
            TandemClient::connect_with_cache(&server.addr, TEST_TOKEN, &[], cache_at(tmp.path()))
                .expect("connect");
        assert_eq!(client.requests_sent(), 1, "the handshake, and nothing else");

        let cold = client.get_object(KIND_FILE, &id).expect("cold read");
        assert_eq!(cold, payload);
        assert_eq!(client.requests_sent(), 2, "a cold read has to ask");

        let warm = client.get_object(KIND_FILE, &id).expect("warm read");
        assert_eq!(warm, payload);
        assert_eq!(
            client.requests_sent(),
            2,
            "a warm read must not send a request"
        );
        assert_eq!(server.requests(), 2, "and none must arrive either");
    }

    #[test]
    fn a_second_command_on_the_same_machine_reads_from_the_cache() {
        // Two clients over one directory is what two `tandem` commands are,
        // and what the three jj store traits are within one command: nothing
        // is shared in memory, only the cache directory.
        let id = vec![0x11, 0x22];
        let payload = b"shared between commands".to_vec();
        let server = CountingServer::start(HashMap::from([(
            format!("/api/objects/file/{}", to_hex(&id)),
            payload.clone(),
        )]));
        let tmp = tempfile::tempdir().expect("temp dir");

        let first =
            TandemClient::connect_with_cache(&server.addr, TEST_TOKEN, &[], cache_at(tmp.path()))
                .expect("connect the first client");
        assert_eq!(
            first.get_object(KIND_FILE, &id).expect("cold read"),
            payload
        );
        let after_warming = server.requests();

        let second =
            TandemClient::connect_with_cache(&server.addr, TEST_TOKEN, &[], cache_at(tmp.path()))
                .expect("connect the second client");
        assert_eq!(
            second.get_object(KIND_FILE, &id).expect("warm read"),
            payload
        );
        assert_eq!(
            second.requests_sent(),
            1,
            "the second client's only request must be its handshake"
        );
        assert_eq!(
            server.requests(),
            after_warming + 1,
            "the handshake arrived; the object read did not"
        );
    }

    #[test]
    fn a_corrupt_cache_entry_is_refetched_rather_than_believed() {
        let id = vec![0x5a];
        let payload = b"the true bytes".to_vec();
        let server = CountingServer::start(HashMap::from([(
            format!("/api/objects/file/{}", to_hex(&id)),
            payload.clone(),
        )]));
        let tmp = tempfile::tempdir().expect("temp dir");

        let client =
            TandemClient::connect_with_cache(&server.addr, TEST_TOKEN, &[], cache_at(tmp.path()))
                .expect("connect");
        assert_eq!(
            client.get_object(KIND_FILE, &id).expect("cold read"),
            payload
        );

        let entries = files_under(tmp.path());
        assert_eq!(entries.len(), 1, "one read, one entry: {entries:?}");
        let mut raw = std::fs::read(&entries[0]).expect("read the entry");
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        std::fs::write(&entries[0], &raw).expect("corrupt the entry");

        let before = client.requests_sent();
        let repaired = client
            .get_object(KIND_FILE, &id)
            .expect("read past the corruption");
        assert_eq!(repaired, payload, "corruption must not reach the caller");
        assert_eq!(
            client.requests_sent(),
            before + 1,
            "a corrupt entry must cost exactly one refetch"
        );

        let before = client.requests_sent();
        assert_eq!(
            client.get_object(KIND_FILE, &id).expect("warm read"),
            payload
        );
        assert_eq!(
            client.requests_sent(),
            before,
            "and the refetch must have repaired the entry"
        );
    }

    #[test]
    fn a_missing_object_is_not_remembered_as_missing() {
        // The server keeps "there is no such object" and "this disk would not
        // answer" apart on purpose. A client that wrote either to disk would
        // remember a fault as a fact, so nothing but a body gets cached.
        let id = vec![0x77];
        let server = CountingServer::start(HashMap::new());
        let tmp = tempfile::tempdir().expect("temp dir");

        let client =
            TandemClient::connect_with_cache(&server.addr, TEST_TOKEN, &[], cache_at(tmp.path()))
                .expect("connect");
        assert!(client.get_object(KIND_FILE, &id).is_err());
        assert!(
            files_under(tmp.path()).is_empty(),
            "a 404 must leave nothing on disk"
        );

        let before = client.requests_sent();
        assert!(client.get_object(KIND_FILE, &id).is_err());
        assert_eq!(
            client.requests_sent(),
            before + 1,
            "the second attempt must ask the server again"
        );
    }

    #[test]
    fn a_client_without_a_cache_still_reads() {
        let id = vec![0x01];
        let payload = b"no cache here".to_vec();
        let server = CountingServer::start(HashMap::from([(
            format!("/api/objects/file/{}", to_hex(&id)),
            payload.clone(),
        )]));

        let client =
            TandemClient::connect_with_cache(&server.addr, TEST_TOKEN, &[], None).expect("connect");
        assert_eq!(
            client.get_object(KIND_FILE, &id).expect("first read"),
            payload
        );
        assert_eq!(
            client.get_object(KIND_FILE, &id).expect("second read"),
            payload
        );
        assert_eq!(
            server.requests(),
            3,
            "every read asks when there is no cache"
        );
    }

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
    fn every_request_carries_the_token_the_client_was_given() {
        let id = vec![0x01, 0x02];
        let server = CountingServer::start(HashMap::from([(
            format!("/api/objects/file/{}", to_hex(&id)),
            b"payload".to_vec(),
        )]));

        let client =
            TandemClient::connect_with_cache(&server.addr, TEST_TOKEN, &[], None).expect("connect");
        client.get_object(KIND_FILE, &id).expect("read");

        let seen = server.authorizations();
        assert_eq!(
            seen.len(),
            2,
            "the handshake and the read, both authorized: {seen:?}"
        );
        for header in seen {
            assert_eq!(header, format!("Bearer {TEST_TOKEN}"));
        }
    }

    #[test]
    fn etags_round_trip_a_version() {
        assert_eq!(etag_for_version(0), "\"0\"");
        assert_eq!(version_from_etag(&etag_for_version(42)), Some(42));
        assert_eq!(version_from_etag("not-a-version"), None);
    }
}
