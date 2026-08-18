//! The HTTP API surface.
//!
//! Every handler here is a thin translation between an HTTP request and one
//! of the `*_sync` methods on `Server`, which hold the actual logic and know
//! nothing about transports. The shapes worth calling out:
//!
//! * Objects, operations and views are content-addressed, so a `GET` of one
//!   can never go stale and is answered `Cache-Control: immutable`.
//! * Heads are the one mutable resource. `GET /api/heads` carries the CAS
//!   version as an `ETag`; `POST /api/heads` requires that version back in
//!   `If-Match` and answers `412 Precondition Failed`, with the state the
//!   caller lost the race to, when it does not match.
//! * `GET /api/events` is a wake-up channel, not a data channel. It says a
//!   version happened; a watcher reads `/api/heads` to learn what.
//!
//! The `*_sync` methods block — they drive jj-lib through `pollster` and
//! touch the filesystem and the bucket — so each handler hands its work to
//! `spawn_blocking` rather than stalling a reactor thread.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use futures::StreamExt as _;
use tokio_stream::wrappers::BroadcastStream;

use super::Server;
use crate::hex::{from_hex, to_hex};
use crate::http_client::etag_for_version;
use crate::wire;

// ─── Contention observability ─────────────────────────────────────────────────
//
// `tandem logs` carries these three fields on every updateOpHeads line so a
// contention trace reads the same whichever side wrote it. Retrying is the
// client's job — `TandemOpHeadsStore` runs the CAS loop and logs the real
// counts — so the server, which sees one publish per request and queues
// nothing, reports the constants that describe its own behaviour.
const SERVER_ATTEMPT: u32 = 1;
const SERVER_CAS_RETRIES: u32 = 0;
const SERVER_QUEUE_DEPTH: u32 = 0;

// ─── Request size ─────────────────────────────────────────────────────────────
//
// axum defaults every `Bytes` body to 2 MiB, which is not a size anyone chose
// here — it is smaller than plenty of ordinary source files, and a tracked
// file that grows past it stops the workspace snapshotting at all. Cap'n Proto
// allowed 64 MiB per message (its default traversal limit of 8 Mi words), so
// that is the ceiling this transport keeps: no smaller than what it replaced,
// and still bounded, because each handler copies the body onto the heap.
//
// The batch endpoint is the one that will press against it — filling a cache
// or cloning a repo means many blobs in one request. A batch that would not
// fit is the caller's to split; the ceiling stays put.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

// ─── Router ───────────────────────────────────────────────────────────────────

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/api/info", get(get_info))
        .route("/api/objects/{kind}/{id}", get(get_object))
        .route("/api/objects/{kind}", post(put_object))
        .route("/api/objects:batch", post(put_objects_batch))
        .route("/api/ops", get(resolve_op_prefix).post(put_operation))
        .route("/api/ops/{id}", get(get_operation))
        .route("/api/views", post(put_view))
        .route("/api/views/{id}", get(get_view))
        .route("/api/heads", get(get_heads).post(update_heads))
        .route("/api/events", get(events))
        .route("/api/tokens", post(mint_token))
        .route("/api/workspaces/{id}/writer", post(claim_writer_role))
        // Every route above, the handshake included. A server that answered
        // one question to an unauthenticated caller would be telling a
        // stranger which repo it is holding.
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&server),
            require_bearer,
        ))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(server)
}

// ─── Authentication ───────────────────────────────────────────────────────────

/// Resolve the bearer once, and hand what it authorizes to the handler.
///
/// The handler gets an `Authority`, not a token: nothing downstream of here
/// has to know what a token looks like, and nothing downstream can forget to
/// check one.
async fn require_bearer(
    State(server): State<Arc<Server>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> ApiResult<Response> {
    let presented = bearer_from_headers(request.headers())?.to_string();
    let Some(authority) = server.authority_for(&presented) else {
        tracing::debug!(path = %request.uri().path(), "refused an unauthenticated request");
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "the bearer token is unknown or has expired",
        ));
    };
    request.extensions_mut().insert(authority);
    Ok(next.run(request).await)
}

/// The token out of `Authorization: Bearer …`, or the 401 that refuses it.
fn bearer_from_headers(headers: &HeaderMap) -> ApiResult<&str> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "this endpoint needs an Authorization: Bearer token",
            )
        })?
        .to_str()
        .map_err(|_| ApiError::bad_request("Authorization is not text"))?;

    raw.strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Authorization must be a Bearer token",
            )
        })
}

/// The authority the middleware resolved for this request.
///
/// It put one there for every route on this router, so a missing extension is
/// a wiring mistake rather than an unauthenticated caller, and it is answered
/// 401 rather than trusted.
fn authority_of(extensions: &axum::http::Extensions) -> ApiResult<&crate::auth::Authority> {
    extensions.get::<crate::auth::Authority>().ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "this request carries no resolved authority",
        )
    })
}

// ─── Error shape ──────────────────────────────────────────────────────────────

/// A failure, rendered the way every endpoint renders one: a status plus a
/// JSON body the client turns straight back into an error message.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// A read failure is only a 404 when the object really is absent.
    ///
    /// Answering every read failure with 404 makes a disk or permission fault
    /// look exactly like a missing object, and a client that caches the
    /// conclusion — which the immutable-cache model invites it to — remembers
    /// a fault as a fact. So the server says which one it was: `NotFound` is a
    /// 404, a malformed id is a 400, and anything else is a 500 the caller may
    /// retry.
    fn from_read(error: anyhow::Error) -> Self {
        if error.downcast_ref::<super::NotFound>().is_some() {
            Self::new(StatusCode::NOT_FOUND, format!("{error:#}"))
        } else if error.downcast_ref::<super::MalformedId>().is_some() {
            Self::new(StatusCode::BAD_REQUEST, format!("{error:#}"))
        } else {
            Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
        }
    }

    /// A publish that the token was not allowed to make is the client's
    /// answer to keep, not a fault to retry: 403, with the reason the scope
    /// check gave. Everything else on that path is still a 500.
    fn from_publish(error: anyhow::Error) -> Self {
        if error.downcast_ref::<super::ScopeDenied>().is_some() {
            Self::new(StatusCode::FORBIDDEN, format!("{error:#}"))
        } else {
            Self::internal(error)
        }
    }

    fn from_write(error: anyhow::Error) -> Self {
        Self::new(StatusCode::BAD_REQUEST, format!("{error:#}"))
    }

    fn internal(error: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(wire::ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

/// Run a blocking `Server` method off the reactor.
///
/// The failure comes back as it was raised, so that each handler can decide
/// what its own failure means in HTTP terms.
async fn blocking<T, F>(server: &Arc<Server>, work: F) -> Result<T>
where
    F: FnOnce(&Server) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let server = Arc::clone(server);
    tokio::task::spawn_blocking(move || work(&server))
        .await
        .map_err(|e| anyhow::anyhow!("worker task failed: {e}"))?
}

// ─── Handlers: repo info ──────────────────────────────────────────────────────

async fn get_info(State(server): State<Arc<Server>>) -> ApiResult<Json<wire::RepoInfoBody>> {
    tracing::trace!(rpc_method = "getInfo", "rpc request");
    let body = blocking(&server, |server| Ok(server.repo_info_body()))
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(body))
}

// ─── Handlers: objects ────────────────────────────────────────────────────────

fn immutable_bytes(data: Vec<u8>, etag: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(wire::CONTENT_TYPE_OCTETS),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(wire::IMMUTABLE_CACHE_CONTROL),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("\"{etag}\"")) {
        headers.insert(header::ETAG, value);
    }
    (headers, data).into_response()
}

async fn get_object(
    State(server): State<Arc<Server>>,
    Path((kind, id_hex)): Path<(String, String)>,
) -> ApiResult<Response> {
    let kind = validated_kind(&kind)?;
    let id = parse_hex(&id_hex, "object id")?;

    tracing::debug!(rpc_method = "getObject", kind = %kind, object_id = %id_hex, "rpc request");

    let data = blocking(&server, move |server| server.get_object_sync(kind, &id))
        .await
        .map_err(ApiError::from_read)?;

    tracing::debug!(rpc_method = "getObject", kind = %kind, object_id = %id_hex, bytes = data.len(), "rpc response");
    Ok(immutable_bytes(data, &id_hex))
}

async fn put_object(
    State(server): State<Arc<Server>>,
    Path(kind): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let kind = validated_kind(&kind)?;
    tracing::info!(rpc_method = "putObject", kind = %kind, bytes = body.len(), "rpc request");

    let data = body.to_vec();
    let (id, normalized) = blocking(&server, move |server| server.put_object_sync(kind, &data))
        .await
        .map_err(ApiError::from_write)?;

    tracing::info!(
        rpc_method = "putObject",
        kind = %kind,
        object_id = %to_hex(&id),
        bytes = body.len(),
        normalized_bytes = normalized.len(),
        "rpc response"
    );
    Ok(id_and_bytes(wire::HEADER_OBJECT_ID, &id, normalized))
}

async fn put_objects_batch(State(server): State<Arc<Server>>, body: Bytes) -> ApiResult<Response> {
    let items = wire::decode_batch_request(&body)
        .map_err(|e| ApiError::bad_request(format!("bad batch frame: {e}")))?;
    tracing::info!(
        rpc_method = "putObjectsBatch",
        items = items.len(),
        "rpc request"
    );

    let outcomes = blocking(&server, move |server| {
        Ok(items
            .into_iter()
            .map(|item| {
                let Some(kind) = wire::kind_name(item.kind) else {
                    return wire::BatchOutcome::Failed {
                        message: format!("unknown object kind: {}", item.kind),
                    };
                };
                match server.put_object_sync(kind, &item.data) {
                    Ok((id, normalized)) => wire::BatchOutcome::Written { id, normalized },
                    Err(error) => wire::BatchOutcome::Failed {
                        message: format!("{error:#}"),
                    },
                }
            })
            .collect::<Vec<_>>())
    })
    .await
    .map_err(ApiError::internal)?;

    tracing::info!(
        rpc_method = "putObjectsBatch",
        items = outcomes.len(),
        "rpc response"
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(wire::CONTENT_TYPE_BATCH),
    );
    Ok((headers, wire::encode_batch_response(&outcomes)).into_response())
}

// ─── Handlers: operations and views ───────────────────────────────────────────

#[derive(serde::Deserialize)]
struct PrefixQuery {
    prefix: Option<String>,
}

async fn get_operation(
    State(server): State<Arc<Server>>,
    Path(id_hex): Path<String>,
) -> ApiResult<Response> {
    let id = parse_hex(&id_hex, "operation id")?;
    tracing::debug!(rpc_method = "getOperation", operation_id = %id_hex, "rpc request");

    let data = blocking(&server, move |server| server.get_operation_sync(&id))
        .await
        .map_err(ApiError::from_read)?;

    tracing::debug!(rpc_method = "getOperation", operation_id = %id_hex, bytes = data.len(), "rpc response");
    Ok(immutable_bytes(data, &id_hex))
}

async fn put_operation(State(server): State<Arc<Server>>, body: Bytes) -> ApiResult<Response> {
    tracing::info!(
        rpc_method = "putOperation",
        bytes = body.len(),
        "rpc request"
    );
    let data = body.to_vec();
    let id = blocking(&server, move |server| server.put_operation_sync(&data))
        .await
        .map_err(ApiError::from_write)?;

    tracing::info!(rpc_method = "putOperation", operation_id = %to_hex(&id), bytes = body.len(), "rpc response");
    Ok(id_and_bytes(wire::HEADER_OPERATION_ID, &id, Vec::new()))
}

async fn get_view(
    State(server): State<Arc<Server>>,
    Path(id_hex): Path<String>,
) -> ApiResult<Response> {
    let id = parse_hex(&id_hex, "view id")?;
    tracing::debug!(rpc_method = "getView", view_id = %id_hex, "rpc request");

    let data = blocking(&server, move |server| server.get_view_sync(&id))
        .await
        .map_err(ApiError::from_read)?;

    tracing::debug!(rpc_method = "getView", view_id = %id_hex, bytes = data.len(), "rpc response");
    Ok(immutable_bytes(data, &id_hex))
}

async fn put_view(State(server): State<Arc<Server>>, body: Bytes) -> ApiResult<Response> {
    tracing::info!(rpc_method = "putView", bytes = body.len(), "rpc request");
    let data = body.to_vec();
    let id = blocking(&server, move |server| server.put_view_sync(&data))
        .await
        .map_err(ApiError::from_write)?;

    tracing::info!(rpc_method = "putView", view_id = %to_hex(&id), bytes = body.len(), "rpc response");
    Ok(id_and_bytes(wire::HEADER_VIEW_ID, &id, Vec::new()))
}

async fn resolve_op_prefix(
    State(server): State<Arc<Server>>,
    Query(query): Query<PrefixQuery>,
) -> ApiResult<Json<wire::PrefixBody>> {
    let prefix = query
        .prefix
        .ok_or_else(|| ApiError::bad_request("GET /api/ops needs a ?prefix= to resolve"))?;
    if !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request("prefix must be hex"));
    }
    tracing::debug!(rpc_method = "resolveOperationIdPrefix", prefix = %prefix, "rpc request");

    let lookup = prefix.clone();
    let (resolution, matched) = blocking(&server, move |server| {
        server.resolve_operation_id_prefix_sync(&lookup)
    })
    .await
    .map_err(ApiError::internal)?;

    tracing::debug!(
        rpc_method = "resolveOperationIdPrefix",
        prefix = %prefix,
        resolution = %resolution,
        "rpc response"
    );
    Ok(Json(wire::PrefixBody {
        resolution,
        id: matched.map(|id| to_hex(&id)),
    }))
}

// ─── Handlers: heads ──────────────────────────────────────────────────────────

fn heads_response(status: StatusCode, body: wire::HeadsBody) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&etag_for_version(body.version)) {
        headers.insert(header::ETAG, value);
    }
    // Heads move. A cache that answered this from a store would hand a client
    // a version it must then lose a CAS race to discover is stale.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (status, headers, Json(body)).into_response()
}

async fn get_heads(State(server): State<Arc<Server>>) -> ApiResult<Response> {
    tracing::debug!(rpc_method = "getHeads", "rpc request");
    let state = blocking(&server, |server| server.get_heads_sync())
        .await
        .map_err(ApiError::internal)?;
    tracing::debug!(
        rpc_method = "getHeads",
        version = state.version,
        heads = state.heads.len(),
        workspace_heads = state.workspace_heads.len(),
        "rpc response"
    );
    Ok(heads_response(
        StatusCode::OK,
        wire::HeadsBody {
            version: state.version,
            heads: state.heads,
            workspace_heads: state.workspace_heads,
        },
    ))
}

async fn update_heads(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    extensions: axum::http::Extensions,
    Json(request): Json<wire::UpdateHeadsBody>,
) -> ApiResult<Response> {
    let expected_version = expected_version_from_if_match(&headers)?;
    let authority = authority_of(&extensions)?.clone();

    // The workspace a publish attributes itself to has to be the one the token
    // speaks for. A publish that names no workspace attributes nothing, which
    // any token may do — the view-diff check below is what actually decides
    // what it is allowed to change.
    if !request.workspace_id.is_empty() && !authority.may_act_for(&request.workspace_id) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!(
                "this token does not speak for workspace {}",
                request.workspace_id
            ),
        ));
    }

    let mut old_ids = Vec::with_capacity(request.old_ids.len());
    for hex in &request.old_ids {
        old_ids.push(parse_hex(hex, "old operation id")?);
    }
    let new_id = parse_hex(&request.new_id, "new operation id")?;
    let workspace_id = if request.workspace_id.is_empty() {
        None
    } else {
        Some(request.workspace_id.clone())
    };

    let started = Instant::now();
    tracing::debug!(
        rpc_method = "updateOpHeads",
        expected_version,
        old_ids = old_ids.len(),
        new_id = %request.new_id,
        workspace_id = request.workspace_id.as_str(),
        attempt = SERVER_ATTEMPT,
        cas_retries = SERVER_CAS_RETRIES,
        queue_depth = SERVER_QUEUE_DEPTH,
        "rpc request"
    );

    let workspace_for_call = workspace_id.clone();
    let new_for_call = new_id.clone();
    let result = blocking(&server, move |server| {
        server.update_op_heads_sync(
            old_ids,
            new_for_call,
            expected_version,
            workspace_for_call,
            &authority,
        )
    })
    .await
    .map_err(ApiError::from_publish)?;

    let status = if result.ok {
        StatusCode::OK
    } else {
        StatusCode::PRECONDITION_FAILED
    };
    tracing::debug!(
        rpc_method = "updateOpHeads",
        ok = result.ok,
        status = status.as_u16(),
        version = result.version,
        heads = result.heads.len(),
        workspace_heads = result.workspace_heads.len(),
        attempt = SERVER_ATTEMPT,
        cas_retries = SERVER_CAS_RETRIES,
        queue_depth = SERVER_QUEUE_DEPTH,
        latency_ms = started.elapsed().as_millis() as u64,
        "rpc response"
    );

    Ok(heads_response(
        status,
        wire::HeadsBody {
            version: result.version,
            heads: result.heads.iter().map(|id| to_hex(id)).collect(),
            workspace_heads: result.workspace_heads,
        },
    ))
}

/// The CAS version a client is publishing against, taken from `If-Match`.
///
/// The header is required: a publish without one is a client that thinks it
/// can overwrite whatever is there, which is the race the CAS exists to stop.
fn expected_version_from_if_match(headers: &HeaderMap) -> ApiResult<u64> {
    let raw = headers
        .get(header::IF_MATCH)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::PRECONDITION_REQUIRED,
                "POST /api/heads needs an If-Match with the version it read",
            )
        })?
        .to_str()
        .map_err(|_| ApiError::bad_request("If-Match is not text"))?;

    crate::http_client::version_from_etag(raw)
        .ok_or_else(|| ApiError::bad_request(format!("If-Match {raw:?} is not a head version")))
}

// ─── Handlers: tokens and the writer role ─────────────────────────────────────

/// `POST /api/tokens` — the admin token asking for a workspace-scoped one.
async fn mint_token(
    State(server): State<Arc<Server>>,
    extensions: axum::http::Extensions,
    Json(request): Json<wire::MintTokenBody>,
) -> ApiResult<Json<wire::TokenBody>> {
    if !authority_of(&extensions)?.is_admin() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "only the admin token may mint tokens",
        ));
    }
    let workspace_id = request.workspace_id.trim().to_string();
    if workspace_id.is_empty() {
        return Err(ApiError::bad_request(
            "POST /api/tokens needs the workspaceId the token is for",
        ));
    }
    let ttl = request
        .ttl_seconds
        .map(std::time::Duration::from_secs)
        .unwrap_or(crate::auth::DEFAULT_TOKEN_TTL);

    tracing::info!(rpc_method = "mintToken", workspace_id = %workspace_id, "rpc request");
    let body = blocking(&server, move |server| {
        Ok(server.mint_token_sync(&workspace_id, ttl))
    })
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(body))
}

/// `POST /api/workspaces/{id}/writer` — claiming or renewing the writer role.
///
/// A claim by the holder that already has it is a renewal and always answers
/// 200. A claim by anybody else while the current one still stands is a 409,
/// carrying who holds it and for how much longer.
async fn claim_writer_role(
    State(server): State<Arc<Server>>,
    extensions: axum::http::Extensions,
    Path(workspace_id): Path<String>,
    Json(request): Json<wire::ClaimWriterBody>,
) -> ApiResult<Json<wire::WriterRoleBody>> {
    if !authority_of(&extensions)?.may_act_for(&workspace_id) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("this token does not speak for workspace {workspace_id}"),
        ));
    }
    let holder = request.holder.trim().to_string();
    if holder.is_empty() {
        return Err(ApiError::bad_request(
            "a writer-role claim needs a holder to name who is asking",
        ));
    }
    let ttl = request
        .ttl_seconds
        .map(std::time::Duration::from_secs)
        .unwrap_or(super::writer::DEFAULT_WRITER_TTL);

    tracing::info!(
        rpc_method = "claimWriterRole",
        workspace_id = %workspace_id,
        holder = %holder,
        "rpc request"
    );

    let claimed = blocking(&server, move |server| {
        Ok(server.claim_writer_role_sync(&workspace_id, &holder, ttl))
    })
    .await
    .map_err(ApiError::internal)?;

    match claimed {
        Ok(role) => Ok(Json(wire::WriterRoleBody {
            workspace_id: role.workspace_id,
            holder: role.holder,
            expires_in_seconds: role.expires_in.as_secs(),
        })),
        Err(conflict) => Err(ApiError::new(StatusCode::CONFLICT, conflict.to_string())),
    }
}

// ─── Handlers: events ─────────────────────────────────────────────────────────

/// Best-effort wake-ups. A slow reader misses versions rather than holding
/// the publisher up, which is safe precisely because the event carries no
/// data: the next read of `/api/heads` catches up on everything skipped.
async fn events(
    State(server): State<Arc<Server>>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    tracing::info!(rpc_method = "events", "watcher subscribed");
    let stream = BroadcastStream::new(server.subscribe_heads()).filter_map(|version| async move {
        let version = version.ok()?;
        let body = wire::HeadsEventBody { version };
        let data = serde_json::to_string(&body).ok()?;
        Some(Ok(Event::default().event("heads").data(data)))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// The path segment as a kind, or the 400 that refuses it.
///
/// The answer is the canonical `&'static str` rather than a copy of the
/// segment, so a handler can keep one for its tracing lines and hand the same
/// one to `spawn_blocking` without cloning either.
fn validated_kind(kind: &str) -> ApiResult<&'static str> {
    wire::canonical_kind_name(kind)
        .ok_or_else(|| ApiError::bad_request(format!("unknown object kind: {kind}")))
}

fn parse_hex(hex: &str, what: &str) -> ApiResult<Vec<u8>> {
    from_hex(hex).map_err(|e| ApiError::bad_request(format!("{what} is not hex: {e}")))
}

fn id_and_bytes(header_name: &'static str, id: &[u8], body: Vec<u8>) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(wire::CONTENT_TYPE_OCTETS),
    );
    if let Ok(value) = HeaderValue::from_str(&to_hex(id)) {
        headers.insert(header_name, value);
    }
    (headers, body).into_response()
}
