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
use tracing::Instrument as _;

use super::Server;
use jj_tandem_protocol::{
    hex::{from_hex, to_hex},
    http::{etag_for_version, version_from_etag},
    wire,
};

// ─── Contention observability ─────────────────────────────────────────────────
//
// `tandem logs` carries these three fields on every updateOpHeads line so a
// contention trace reads the same whichever side wrote it. Retrying is the
// client's job — `TandemOpHeadsStore` runs the CAS loop and logs the real
// counts — so the server, which sees one publish per request and queues
// nothing, reports the constants that describe its own behaviour.
const SERVER_ATTEMPT: u32 = 1;
const SERVER_CAS_RETRIES: u32 = 0;

// ─── Request size ─────────────────────────────────────────────────────────────
//
// axum defaults every `Bytes` body to 2 MiB, which is not a size anyone chose
// here — it is smaller than plenty of ordinary source files, and a tracked
// file that grows past it stops the workspace snapshotting at all. The 64 MiB
// ceiling preserves the established compatibility limit while remaining
// bounded, because each handler copies the body onto the heap.
//
// The batch endpoint is the one that will press against it — filling a cache
// or cloning a repo means many blobs in one request. A batch that would not
// fit is the caller's to split; the ceiling stays put.

// ─── Router ───────────────────────────────────────────────────────────────────

pub fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/api/info", get(get_info))
        .route("/api/objects/{kind}/{id}", get(get_object))
        .route("/api/objects/{kind}", post(put_object))
        .route("/api/objects:batch", post(put_objects_batch))
        .route("/api/ops", get(resolve_op_prefix).post(put_operation))
        .route("/api/ops/{id}", get(get_operation))
        .route("/api/ops:upload", post(put_operation_with_view))
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
        .layer(DefaultBodyLimit::max(wire::MAX_REQUEST_BODY_BYTES))
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
    const CONTROL_BODY_MAX_BYTES: usize = 16 * 1024;
    let presented = bearer_from_headers(request.headers())?.to_string();
    let Some(authority) = server.authority_for(&presented) else {
        tracing::debug!(path = %request.uri().path(), "refused an unauthenticated request");
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "the bearer token is unknown or has expired",
        ));
    };
    request.extensions_mut().insert(authority);
    let is_publish =
        request.method() == axum::http::Method::POST && request.uri().path() == "/api/heads";
    if is_publish {
        let permit = server
            .acquire_publish()
            .await
            .map_err(publish_admission_error)?;
        request
            .extensions_mut()
            .insert(Arc::new(super::PublishPermitCell(std::sync::Mutex::new(
                Some(permit),
            ))));
    }
    // Authenticate the headers first, then acquire before an extractor polls
    // the body. GET includes the long-lived event stream and therefore never
    // consumes a decoded-body permit.
    let is_writer_control =
        request.method() == axum::http::Method::POST && request.uri().path().ends_with("/writer");
    let body_permit = if requires_body_permit(request.method()) {
        Some(
            server
                .acquire_body(is_writer_control)
                .await
                .map_err(ApiError::internal)?,
        )
    } else {
        None
    };
    if let Some(body_permit) = body_permit {
        if is_writer_control {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, CONTROL_BODY_MAX_BYTES)
                .await
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "writer claim body exceeds 16 KiB",
                    )
                })?;
            request = axum::http::Request::from_parts(parts, axum::body::Body::from(bytes));
        }
        // A disconnected caller cancels this middleware future, but Tokio
        // detaches the task. Its decoded body and admission charge therefore
        // live together until all downstream work has actually finished.
        let span = tracing::Span::current();
        tokio::spawn(
            async move {
                let _body_permit = body_permit;
                next.run(request).await
            }
            .instrument(span),
        )
        .await
        .map_err(|error| ApiError::internal(anyhow::anyhow!("request task failed: {error}")))
    } else {
        Ok(next.run(request).await)
    }
}

fn requires_body_permit(method: &axum::http::Method) -> bool {
    matches!(
        *method,
        axum::http::Method::POST | axum::http::Method::PUT | axum::http::Method::PATCH
    )
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
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        work(&server)
    })
    .await
    .map_err(|e| anyhow::anyhow!("worker task failed: {e}"))?
}

// ─── Handlers: repo info ──────────────────────────────────────────────────────

async fn get_info(State(server): State<Arc<Server>>) -> ApiResult<Json<wire::RepoInfoBody>> {
    tracing::debug!(rpc_method = "getInfo", "rpc request");
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

    let data = blocking(&server, move |server| {
        server.repository.get_object_sync(kind, &id)
    })
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
    let body_len = body.len();
    tracing::info!(rpc_method = "putObject", kind = %kind, bytes = body_len, "rpc request");

    let data = body.to_vec();
    drop(body);
    let (id, normalized) = blocking(&server, move |server| {
        server.repository.put_object_sync(kind, &data)
    })
    .await
    .map_err(ApiError::from_write)?;

    tracing::info!(
        rpc_method = "putObject",
        kind = %kind,
        object_id = %to_hex(&id),
        bytes = body_len,
        normalized_bytes = normalized.len(),
        "rpc response"
    );
    Ok(id_and_bytes(wire::HEADER_OBJECT_ID, &id, normalized))
}

async fn put_objects_batch(State(server): State<Arc<Server>>, body: Bytes) -> ApiResult<Response> {
    let items = wire::decode_batch_request(&body)
        .map_err(|e| ApiError::bad_request(format!("bad batch frame: {e}")))?;
    drop(body);
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
                match server.repository.put_object_sync(kind, &item.data) {
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

    let data = blocking(&server, move |server| {
        server.repository.get_operation_sync(&id)
    })
    .await
    .map_err(ApiError::from_read)?;

    tracing::debug!(rpc_method = "getOperation", operation_id = %id_hex, bytes = data.len(), "rpc response");
    Ok(immutable_bytes(data, &id_hex))
}

async fn put_operation(State(server): State<Arc<Server>>, body: Bytes) -> ApiResult<Response> {
    let body_len = body.len();
    tracing::info!(rpc_method = "putOperation", bytes = body_len, "rpc request");
    let data = body.to_vec();
    drop(body);
    let id = blocking(&server, move |server| {
        server.repository.put_operation_sync(&data)
    })
    .await
    .map_err(ApiError::from_write)?;

    tracing::info!(rpc_method = "putOperation", operation_id = %to_hex(&id), bytes = body_len, "rpc response");
    Ok(id_and_bytes(wire::HEADER_OPERATION_ID, &id, Vec::new()))
}

async fn put_operation_with_view(
    State(server): State<Arc<Server>>,
    body: Bytes,
) -> ApiResult<Response> {
    let body_len = body.len();
    tracing::info!(
        rpc_method = "putOperationWithView",
        bytes = body_len,
        "rpc request"
    );
    let (view, operation) = wire::decode_operation_upload(&body).map_err(ApiError::bad_request)?;
    let view = view.to_vec();
    let operation = operation.to_vec();
    drop(body);
    let (view_id, id) = blocking(&server, move |server| {
        server
            .repository
            .put_operation_with_view_sync(&view, &operation)
    })
    .await
    .map_err(ApiError::from_write)?;
    tracing::info!(rpc_method = "putOperationWithView", operation_id = %to_hex(&id), bytes = body_len, "rpc response");
    let mut response = id_and_bytes(wire::HEADER_OPERATION_ID, &id, Vec::new());
    response.headers_mut().insert(
        wire::HEADER_VIEW_ID,
        HeaderValue::from_str(&to_hex(&view_id)).expect("hex ID"),
    );
    Ok(response)
}

async fn get_view(
    State(server): State<Arc<Server>>,
    Path(id_hex): Path<String>,
) -> ApiResult<Response> {
    let id = parse_hex(&id_hex, "view id")?;
    tracing::debug!(rpc_method = "getView", view_id = %id_hex, "rpc request");

    let data = blocking(&server, move |server| server.repository.get_view_sync(&id))
        .await
        .map_err(ApiError::from_read)?;

    tracing::debug!(rpc_method = "getView", view_id = %id_hex, bytes = data.len(), "rpc response");
    Ok(immutable_bytes(data, &id_hex))
}

async fn put_view(State(server): State<Arc<Server>>, body: Bytes) -> ApiResult<Response> {
    let body_len = body.len();
    tracing::info!(rpc_method = "putView", bytes = body_len, "rpc request");
    let data = body.to_vec();
    drop(body);
    let id = blocking(&server, move |server| {
        server.repository.put_view_sync(&data)
    })
    .await
    .map_err(ApiError::from_write)?;

    tracing::info!(rpc_method = "putView", view_id = %to_hex(&id), bytes = body_len, "rpc response");
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
    let resolved = blocking(&server, move |server| {
        server.repository.resolve_operation_id_prefix_sync(&lookup)
    })
    .await
    .map_err(ApiError::internal)?;

    let (resolution, matched) = match resolved {
        jj_tandem_repository::PrefixResolution::NoMatch => ("noMatch", None),
        jj_tandem_repository::PrefixResolution::SingleMatch(id) => ("singleMatch", Some(id)),
        jj_tandem_repository::PrefixResolution::Ambiguous => ("ambiguous", None),
    };

    tracing::debug!(
        rpc_method = "resolveOperationIdPrefix",
        prefix = %prefix,
        resolution = %resolution,
        "rpc response"
    );
    Ok(Json(wire::PrefixBody {
        resolution: resolution.to_string(),
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
    let state = blocking(&server, |server| server.repository.get_heads_sync())
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
    let new_id_hex = request.new_id.clone();
    let workspace_id = if request.workspace_id.is_empty() {
        None
    } else {
        Some(request.workspace_id.clone())
    };
    let cell = extensions
        .get::<Arc<super::PublishPermitCell>>()
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("publish admission missing")))?;
    let publish_permit = cell
        .0
        .lock()
        .map_err(|error| ApiError::internal(anyhow::anyhow!("publish admission lock: {error}")))?
        .take()
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("publish admission already used")))?;
    let queue_depth = publish_permit.queue_depth;
    let admission_wait_ms = publish_permit.wait_ms;

    let started = Instant::now();
    tracing::debug!(
        rpc_method = "updateOpHeads",
        expected_version,
        old_ids = old_ids.len(),
        new_id = %request.new_id,
        workspace_id = request.workspace_id.as_str(),
        attempt = SERVER_ATTEMPT,
        cas_retries = SERVER_CAS_RETRIES,
        queue_depth,
        admission_wait_ms,
        "rpc request"
    );

    let workspace_for_call = workspace_id.clone();
    let new_for_call = new_id.clone();
    let result = blocking(&server, move |server| {
        let _publish_permit = publish_permit;
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
        new_id = %new_id_hex,
        attempt = SERVER_ATTEMPT,
        cas_retries = SERVER_CAS_RETRIES,
        queue_depth,
        admission_wait_ms,
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

fn publish_admission_error(error: anyhow::Error) -> ApiError {
    if error.downcast_ref::<super::PublishQueueFull>().is_some() {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
    } else {
        ApiError::internal(error)
    }
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

    version_from_etag(raw)
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
    let stream = BroadcastStream::new(server.repository.subscribe_heads()).filter_map(
        |version| async move {
            let version = version.ok()?;
            let body = wire::HeadsEventBody { version };
            let data = serde_json::to_string(&body).ok()?;
            Some(Ok(Event::default().event("heads").data(data)))
        },
    );

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

#[cfg(test)]
mod admission_tests {
    use super::*;
    use prost::Message as _;
    use std::time::Duration;
    use tower::ServiceExt as _;

    fn gated_request(
        uri: &'static str,
    ) -> (
        axum::http::Request<axum::body::Body>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let stream = futures::stream::once(async move {
            let _ = entered_tx.send(());
            let _ = release_rx.await;
            Ok::<_, Infallible>(Bytes::from_static(b"{}"))
        });
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer router-admission-secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from_stream(stream))
            .unwrap();
        (request, entered_rx, release_tx)
    }

    fn test_router(
        bodies: Arc<tokio::sync::Semaphore>,
        publishes: Arc<tokio::sync::Semaphore>,
    ) -> (tempfile::TempDir, Router) {
        let directory = tempfile::tempdir().unwrap();
        let server = super::super::Server::new_with_faults_and_budget(
            directory.path().to_path_buf(),
            None,
            "router-admission-secret",
            jj_tandem_repository::FaultPoints::inert(),
            Arc::new(jj_tandem_repository::StagingBudget::default()),
            bodies,
            Arc::new(tokio::sync::Semaphore::new(1)),
            publishes,
        )
        .unwrap();
        (directory, router(Arc::new(server)))
    }

    async fn publish_child(router: Router, description: &str) -> StatusCode {
        let request = |method, uri: &str, body: axum::body::Body| {
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header(header::AUTHORIZATION, "Bearer router-admission-secret")
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .unwrap()
        };
        let heads_response = router
            .clone()
            .oneshot(request(
                axum::http::Method::GET,
                "/api/heads",
                axum::body::Body::empty(),
            ))
            .await
            .unwrap();
        let etag = heads_response.headers()[header::ETAG].clone();
        let heads_bytes = axum::body::to_bytes(heads_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let heads: serde_json::Value = serde_json::from_slice(&heads_bytes).unwrap();
        let old = heads["heads"][0].as_str().unwrap();
        let operation_response = router
            .clone()
            .oneshot(request(
                axum::http::Method::GET,
                &format!("/api/ops/{old}"),
                axum::body::Body::empty(),
            ))
            .await
            .unwrap();
        let operation_bytes = axum::body::to_bytes(operation_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let mut operation =
            jj_lib::protos::simple_op_store::Operation::decode(operation_bytes).unwrap();
        operation.parents = vec![from_hex(old).unwrap()];
        operation.metadata.get_or_insert_default().description = description.to_owned();
        let upload = router
            .clone()
            .oneshot(request(
                axum::http::Method::POST,
                "/api/ops",
                axum::body::Body::from(operation.encode_to_vec()),
            ))
            .await
            .unwrap();
        assert_eq!(upload.status(), StatusCode::OK);
        let new_id = upload.headers()[wire::HEADER_OPERATION_ID]
            .to_str()
            .unwrap()
            .to_owned();
        let body = serde_json::to_vec(&serde_json::json!({
            "oldIds": [old], "newId": new_id, "workspaceId": ""
        }))
        .unwrap();
        let mut publish = request(
            axum::http::Method::POST,
            "/api/heads",
            axum::body::Body::from(body),
        );
        publish.headers_mut().insert(header::IF_MATCH, etag);
        router.oneshot(publish).await.unwrap().status()
    }

    async fn claim_writer(router: Router, holder: &str, ttl_seconds: u64) -> StatusCode {
        let body = serde_json::to_vec(&serde_json::json!({
            "holder": holder,
            "ttlSeconds": ttl_seconds,
        }))
        .unwrap();
        let request = axum::http::Request::builder()
            .method(axum::http::Method::POST)
            .uri("/api/workspaces/agent-a/writer")
            .header(header::AUTHORIZATION, "Bearer router-admission-secret")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .unwrap();
        router.oneshot(request).await.unwrap().status()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_blocked_wal_publish_does_not_block_another_router() {
        struct ReleaseWal(Arc<jj_tandem_repository::FaultPoints>);
        impl Drop for ReleaseWal {
            fn drop(&mut self) {
                self.0.release_wal_write();
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let bodies = Arc::new(tokio::sync::Semaphore::new(4));
        let publishes = Arc::new(tokio::sync::Semaphore::new(4));
        let faults = jj_tandem_repository::FaultPoints::inert();
        let make = |name: &str| {
            let cache = temporary.path().join(format!("{name}-cache"));
            let bucket = temporary.path().join(format!("{name}-bucket"));
            let server = super::super::Server::new_with_faults_and_budget(
                cache,
                Some(bucket.to_str().unwrap()),
                "router-admission-secret",
                faults.clone(),
                Arc::new(jj_tandem_repository::StagingBudget::default()),
                bodies.clone(),
                Arc::new(tokio::sync::Semaphore::new(1)),
                publishes.clone(),
            )
            .unwrap();
            server.durably_initialize().unwrap();
            router(Arc::new(server))
        };
        let first = make("first");
        let second = make("second");
        assert_eq!(
            claim_writer(first.clone(), "healthy-daemon", 0).await,
            StatusCode::OK
        );
        faults.hold_next_wal_write();
        let release = ReleaseWal(faults.clone());
        let blocked = tokio::spawn(publish_child(first.clone(), "blocked"));
        tokio::task::spawn_blocking({
            let faults = faults.clone();
            move || faults.wait_for_held_wal_write()
        })
        .await
        .unwrap();
        assert_eq!(
            claim_writer(first.clone(), "healthy-daemon", 30).await,
            StatusCode::OK,
            "writer renewal must not wait for repository WAL work"
        );
        assert_eq!(
            claim_writer(first.clone(), "competing-daemon", 30).await,
            StatusCode::CONFLICT,
            "renewal must keep a competing daemon out while publish is gated"
        );
        let unrelated =
            tokio::time::timeout(Duration::from_secs(10), publish_child(second, "unrelated"))
                .await
                .expect("another repository did not durably acknowledge before gate release");
        assert_eq!(unrelated, StatusCode::OK);
        drop(release);
        assert_eq!(blocked.await.unwrap(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writer_renewal_has_a_reserved_bounded_body_slot() {
        let bodies = Arc::new(tokio::sync::Semaphore::new(3));
        let publishes = Arc::new(tokio::sync::Semaphore::new(4));
        let (_directory, router) = test_router(bodies, publishes);
        let mut active = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..3 {
            let (request, entered, release) = gated_request("/api/objects/file");
            let service = router.clone();
            active.push(tokio::spawn(async move { service.oneshot(request).await }));
            releases.push(release);
            entered.await.unwrap();
        }
        let (fourth, mut fourth_entered, fourth_release) = gated_request("/api/objects/file");
        let service = router.clone();
        let queued = tokio::spawn(async move { service.oneshot(fourth).await });
        tokio::task::yield_now().await;
        assert!(matches!(
            fourth_entered.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                claim_writer(router.clone(), "healthy-daemon", 30),
            )
            .await
            .expect("writer renewal waited behind general request bodies"),
            StatusCode::OK
        );

        for release in releases {
            let _ = release.send(());
        }
        for task in active {
            let _ = task.await;
        }
        tokio::time::timeout(Duration::from_secs(1), &mut fourth_entered)
            .await
            .unwrap()
            .unwrap();
        let _ = fourth_release.send(());
        let _ = queued.await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn three_wal_blocked_publishes_and_a_fourth_waiter_leave_renewal_available() {
        let temporary = tempfile::tempdir().unwrap();
        let bodies = Arc::new(tokio::sync::Semaphore::new(3));
        let controls = Arc::new(tokio::sync::Semaphore::new(1));
        let publishes = Arc::new(tokio::sync::Semaphore::new(4));
        let mut faults = Vec::new();
        let mut routers = Vec::new();
        for index in 0..4 {
            let fault = jj_tandem_repository::FaultPoints::inert();
            let cache = temporary.path().join(format!("cache-{index}"));
            let bucket = temporary.path().join(format!("bucket-{index}"));
            let server = super::super::Server::new_with_faults_and_budget(
                cache,
                Some(bucket.to_str().unwrap()),
                "router-admission-secret",
                fault.clone(),
                Arc::new(jj_tandem_repository::StagingBudget::default()),
                bodies.clone(),
                controls.clone(),
                publishes.clone(),
            )
            .unwrap();
            server.durably_initialize().unwrap();
            faults.push(fault);
            routers.push(router(Arc::new(server)));
        }
        assert_eq!(
            claim_writer(routers[0].clone(), "healthy", 0).await,
            StatusCode::OK
        );
        let mut blocked = Vec::new();
        for (index, fault) in faults.iter().take(3).enumerate() {
            fault.hold_next_wal_write();
            blocked.push(tokio::spawn(publish_child(
                routers[index].clone(),
                "blocked",
            )));
        }
        struct ReleaseWals(Vec<Arc<jj_tandem_repository::FaultPoints>>);
        impl Drop for ReleaseWals {
            fn drop(&mut self) {
                for fault in &self.0 {
                    fault.release_wal_write();
                }
            }
        }
        let release = ReleaseWals(faults[..3].to_vec());
        for fault in faults.iter().take(3) {
            tokio::task::spawn_blocking({
                let fault = fault.clone();
                move || fault.wait_for_held_wal_write()
            })
            .await
            .unwrap();
        }
        let fourth = tokio::spawn(publish_child(routers[3].clone(), "waiting"));
        tokio::task::yield_now().await;
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                claim_writer(routers[0].clone(), "healthy", 30),
            )
            .await
            .expect("renewal waited behind saturated general body admission"),
            StatusCode::OK
        );
        assert_eq!(
            claim_writer(routers[0].clone(), "competitor", 30).await,
            StatusCode::CONFLICT
        );
        drop(release);
        for task in blocked {
            assert_eq!(task.await.unwrap(), StatusCode::OK);
        }
        assert_eq!(fourth.await.unwrap(), StatusCode::OK);
    }

    #[test]
    fn only_methods_with_decoded_request_bodies_need_a_permit() {
        assert!(requires_body_permit(&axum::http::Method::POST));
        assert!(requires_body_permit(&axum::http::Method::PUT));
        assert!(requires_body_permit(&axum::http::Method::PATCH));
        assert!(!requires_body_permit(&axum::http::Method::GET));
        assert!(!requires_body_permit(&axum::http::Method::HEAD));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_router_retains_body_permits_through_cancel_and_excludes_sse() {
        let bodies = Arc::new(tokio::sync::Semaphore::new(4));
        let publishes = Arc::new(tokio::sync::Semaphore::new(4));
        let mut directories = Vec::new();
        let mut faults = Vec::new();
        let mut routers = Vec::new();
        for _ in 0..5 {
            let directory = tempfile::tempdir().unwrap();
            let fault = jj_tandem_repository::FaultPoints::inert();
            let server = super::super::Server::new_with_faults_and_budget(
                directory.path().to_path_buf(),
                None,
                "router-admission-secret",
                fault.clone(),
                Arc::new(jj_tandem_repository::StagingBudget::default()),
                bodies.clone(),
                Arc::new(tokio::sync::Semaphore::new(1)),
                publishes.clone(),
            )
            .unwrap();
            directories.push(directory);
            faults.push(fault);
            routers.push(router(Arc::new(server)));
        }
        let request = |entered: tokio::sync::oneshot::Sender<()>,
                       release: tokio::sync::oneshot::Receiver<()>| {
            let stream = futures::stream::once(async move {
                let _ = entered.send(());
                let _ = release.await;
                Ok::<_, Infallible>(Bytes::from_static(b"body"))
            });
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/objects/file")
                .header(header::AUTHORIZATION, "Bearer router-admission-secret")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        };

        let mut releases = Vec::new();
        let mut active = Vec::new();
        for index in 0..4 {
            faults[index].hold_next_object_write();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            releases.push(release_tx);
            let router = routers[index].clone();
            active.push(tokio::spawn(async move {
                router.oneshot(request(entered_tx, release_rx)).await
            }));
            entered_rx.await.unwrap();
        }

        let sse = axum::http::Request::builder()
            .uri("/api/events")
            .header(header::AUTHORIZATION, "Bearer router-admission-secret")
            .body(axum::body::Body::empty())
            .unwrap();
        let response =
            tokio::time::timeout(Duration::from_secs(1), routers[4].clone().oneshot(sse))
                .await
                .expect("SSE must not wait for body admission")
                .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let (fifth_entered_tx, mut fifth_entered_rx) = tokio::sync::oneshot::channel();
        let (fifth_release_tx, fifth_release_rx) = tokio::sync::oneshot::channel();
        let fifth_router = routers[4].clone();
        let fifth = tokio::spawn(async move {
            fifth_router
                .oneshot(request(fifth_entered_tx, fifth_release_rx))
                .await
        });
        tokio::task::yield_now().await;
        assert!(matches!(
            fifth_entered_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        active[0].abort();
        let _ = (&mut active[0]).await;
        assert!(
            matches!(
                fifth_entered_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "client cancellation released a live body charge"
        );
        releases.remove(0).send(()).unwrap();
        faults[0].release_object_write();
        tokio::time::timeout(Duration::from_secs(1), &mut fifth_entered_rx)
            .await
            .expect("fifth body should be polled after actual work releases capacity")
            .unwrap();
        let _ = fifth_release_tx.send(());
        for (index, release) in releases.into_iter().enumerate() {
            let _ = release.send(());
            faults[index + 1].release_object_write();
        }
        let _ = fifth.await;
        for task in active.into_iter().skip(1) {
            let _ = task.await;
        }
        drop(directories);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_publishes_do_not_monopolize_body_permits_across_repositories() {
        let bodies = Arc::new(tokio::sync::Semaphore::new(4));
        let publishes = Arc::new(tokio::sync::Semaphore::new(4));
        let (_first_dir, first) = test_router(bodies.clone(), publishes.clone());
        let (_other_dir, other) = test_router(bodies, publishes);

        let (active_request, active_entered, active_release) = gated_request("/api/heads");
        let active_router = first.clone();
        let active = tokio::spawn(async move { active_router.oneshot(active_request).await });
        active_entered.await.unwrap();

        let mut queued = Vec::new();
        let mut queued_entered = Vec::new();
        let mut queued_releases = Vec::new();
        for _ in 0..3 {
            let (request, entered, release) = gated_request("/api/heads");
            let router = first.clone();
            queued.push(tokio::spawn(async move { router.oneshot(request).await }));
            queued_entered.push(entered);
            queued_releases.push(release);
        }
        tokio::task::yield_now().await;
        for entered in &mut queued_entered {
            assert!(matches!(
                entered.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
        }

        let mut unrelated = Vec::new();
        let mut unrelated_releases = Vec::new();
        for _ in 0..3 {
            let (request, entered, release) = gated_request("/api/objects/file");
            let router = other.clone();
            unrelated.push(tokio::spawn(async move { router.oneshot(request).await }));
            unrelated_releases.push(release);
            tokio::time::timeout(Duration::from_secs(1), entered)
                .await
                .expect("another repository should retain the three free body permits")
                .unwrap();
        }

        for release in unrelated_releases {
            let _ = release.send(());
        }
        for task in unrelated {
            let _ = task.await;
        }
        let _ = active_release.send(());
        let _ = active.await;
        for release in queued_releases {
            let _ = release.send(());
        }
        for task in queued {
            let _ = task.await;
        }
    }
}
