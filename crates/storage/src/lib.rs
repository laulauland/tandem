//! ObjectStore — the bucket abstraction behind tandem's write-ahead log.
//!
//! The bucket is the durable source of truth (see the repository's canonical
//! reliability document):
//! immutable WAL entries plus one
//! CAS-updated index object. Two backends exist — a directory on disk (dev and
//! `cargo test`) and any S3-compatible endpoint (SeaweedFS locally, a real
//! bucket in production).
//!
//! The trait is deliberately synchronous: the server's publish path is sync and
//! already serialized behind one mutex, and jj-lib's async traits are driven
//! with `pollster`. The S3 backend therefore owns a private tokio runtime and
//! blocks the calling thread on it; blocking the caller's reactor is safe
//! because the I/O runs on threads that reactor does not own.

#[cfg(feature = "s3")]
mod http_trace;

use anyhow::{anyhow, bail, Context, Result};
#[cfg(feature = "s3")]
use object_store::ObjectStoreExt as _;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ─── Trait ────────────────────────────────────────────────────────────────────

/// Failure modes of a conditional put.
#[derive(Debug)]
pub enum CasError {
    /// The stored object did not match the expected version. Retryable.
    Conflict,
    /// Anything else — network, permissions, corruption.
    Other(anyhow::Error),
}

impl fmt::Display for CasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CasError::Conflict => write!(f, "conditional put conflict"),
            CasError::Other(err) => write!(f, "{err:#}"),
        }
    }
}

impl From<anyhow::Error> for CasError {
    fn from(err: anyhow::Error) -> Self {
        CasError::Other(err)
    }
}

/// A bucket: flat key space, immutable writes plus one compare-and-swap slot.
pub trait ObjectStore: Send + Sync + fmt::Debug {
    /// Short backend name for logs (`filesystem` / `s3`).
    fn backend_name(&self) -> &'static str;

    /// Human-readable location, safe to log (no credentials).
    fn describe(&self) -> String;

    /// Write once. Succeeds whether this call stored the bytes or an earlier
    /// one did, so retrying an interrupted WAL write is free.
    ///
    /// Returns `true` when this call stored the bytes and `false` when the key
    /// was already taken. The caller needs the difference: a WAL entry that was
    /// already there holds someone else's record list, so anything this caller
    /// meant to put in it is still undurable.
    fn put_immutable(&self, key: &str, data: &[u8]) -> Result<bool>;

    /// Whether the key is taken, without fetching the bytes.
    ///
    /// The WAL ancestry walk needs this: an in-process cache cannot tell a new
    /// process what an earlier one already made durable, and re-putting every
    /// ancestor to find out costs a full entry body per operation.
    fn exists(&self, key: &str) -> Result<bool>;

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    fn get_with_etag(&self, key: &str) -> Result<Option<(Vec<u8>, String)>>;

    /// Unconditional write. Used only when the backend has no conditional put.
    fn put_overwrite(&self, key: &str, data: &[u8]) -> Result<String>;

    /// Compare-and-swap. `expected` of `None` means "must not exist yet".
    /// Returns the new etag.
    fn compare_and_put(
        &self,
        key: &str,
        data: &[u8],
        expected: Option<&str>,
    ) -> std::result::Result<String, CasError>;
}

#[derive(Debug)]
struct MeasuredObjectStore {
    inner: Arc<dyn ObjectStore>,
    repository: Option<String>,
}

impl MeasuredObjectStore {
    fn emit(
        &self,
        operation: &'static str,
        read_bytes: usize,
        write_bytes: usize,
        key: &str,
        started: std::time::Instant,
    ) {
        tracing::debug!(
            bucket_operation = operation,
            bucket_key_class = key.split('/').next().unwrap_or(""),
            bucket_elapsed_us = started.elapsed().as_micros() as u64,
            bucket_calls = 1_u64,
            bucket_read_bytes = read_bytes as u64,
            bucket_write_bytes = write_bytes as u64,
            bucket_backend = self.inner.backend_name(),
            repository = self.repository.as_deref().unwrap_or(""),
            "bucket operation"
        );
    }
}

impl ObjectStore for MeasuredObjectStore {
    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put_immutable(&self, key: &str, data: &[u8]) -> Result<bool> {
        let started = std::time::Instant::now();
        let result = self.inner.put_immutable(key, data);
        self.emit("put_immutable", 0, data.len(), key, started);
        result
    }
    fn exists(&self, key: &str) -> Result<bool> {
        let started = std::time::Instant::now();
        let result = self.inner.exists(key);
        self.emit("exists", 0, 0, key, started);
        result
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let started = std::time::Instant::now();
        let result = self.inner.get(key);
        self.emit(
            "get",
            result
                .as_ref()
                .ok()
                .and_then(|v| v.as_ref())
                .map_or(0, Vec::len),
            0,
            key,
            started,
        );
        result
    }
    fn get_with_etag(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        let started = std::time::Instant::now();
        let result = self.inner.get_with_etag(key);
        self.emit(
            "get_with_etag",
            result
                .as_ref()
                .ok()
                .and_then(|v| v.as_ref())
                .map_or(0, |(v, _)| v.len()),
            0,
            key,
            started,
        );
        result
    }
    fn put_overwrite(&self, key: &str, data: &[u8]) -> Result<String> {
        let started = std::time::Instant::now();
        let result = self.inner.put_overwrite(key, data);
        self.emit("put_overwrite", 0, data.len(), key, started);
        result
    }
    fn compare_and_put(
        &self,
        key: &str,
        data: &[u8],
        expected: Option<&str>,
    ) -> std::result::Result<String, CasError> {
        let started = std::time::Instant::now();
        let result = self.inner.compare_and_put(key, data, expected);
        self.emit("compare_and_put", 0, data.len(), key, started);
        result
    }
}

// ─── Backend selection ────────────────────────────────────────────────────────

/// Open a bucket from a `--bucket` argument.
///
/// Accepted forms:
///   * `/path/to/dir` or `file:///path/to/dir` — a directory as a bucket
///   * `s3://<bucket>[/<prefix>][?endpoint=..&region=..&anonymous=true]`
pub fn open(spec: &str) -> Result<Arc<dyn ObjectStore>> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty bucket specification");
    }
    let inner = if let Some(rest) = spec.strip_prefix("s3://") {
        open_s3(rest)
    } else if let Some(rest) = spec.strip_prefix("file://") {
        open_filesystem(Path::new(rest))
    } else if spec.contains("://") {
        bail!("unsupported bucket scheme: {spec}");
    } else {
        open_filesystem(Path::new(spec))
    }?;
    let repository = spec
        .split('?')
        .next()
        .and_then(|path| path.split_once("/repositories/"))
        .map(|(_, name)| name.trim_matches('/').to_string())
        .filter(|name| !name.is_empty());
    Ok(Arc::new(MeasuredObjectStore { inner, repository }))
}

#[cfg(feature = "s3")]
fn open_s3(rest: &str) -> Result<Arc<dyn ObjectStore>> {
    Ok(Arc::new(S3ObjectStore::open(rest)?))
}

#[cfg(not(feature = "s3"))]
fn open_s3(_rest: &str) -> Result<Arc<dyn ObjectStore>> {
    bail!("S3 bucket support is not enabled in this build")
}

pub fn open_filesystem(root: &Path) -> Result<Arc<dyn ObjectStore>> {
    Ok(Arc::new(FsObjectStore::open(root)?))
}

/// Probe whether the backend really enforces conditional puts.
///
/// Config is not evidence: a store that silently ignores `If-Match` would let
/// a lost update through unnoticed, so the probe writes a canary, then asserts
/// that a stale etag is rejected and a fresh one is accepted.
pub fn probe_conditional_put(store: &dyn ObjectStore) -> Result<bool> {
    const KEY: &str = "_tandem/conditional-put-probe";
    const STALE_ETAG: &str = "\"tandem-deliberately-stale-etag\"";

    let etag = store
        .put_overwrite(KEY, b"tandem conditional put probe\n")
        .context("probe: write canary")?;

    match store.compare_and_put(KEY, b"tandem probe: stale write\n", Some(STALE_ETAG)) {
        Err(CasError::Conflict) => {}
        Ok(_) => return Ok(false),
        Err(CasError::Other(err)) => return Err(err.context("probe: stale conditional put")),
    }

    match store.compare_and_put(KEY, b"tandem probe: fresh write\n", Some(&etag)) {
        Ok(_) => Ok(true),
        Err(CasError::Conflict) => Ok(false),
        Err(CasError::Other(err)) => Err(err.context("probe: fresh conditional put")),
    }
}

// ─── Filesystem backend ───────────────────────────────────────────────────────

/// A directory as a bucket.
///
/// Etags are content hashes. That is enough for compare-and-swap here because
/// every index write carries a strictly increasing version number, so two
/// distinct index states never hash alike. Writers serialize on an `flock` over
/// a sentinel file, so the read-compare-write is atomic across processes too.
#[derive(Debug)]
pub struct FsObjectStore {
    root: PathBuf,
}

impl FsObjectStore {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("create bucket directory {}", root.display()))?;
        let root = dunce::canonicalize(root)
            .with_context(|| format!("canonicalize bucket directory {}", root.display()))?;
        Ok(Self { root })
    }

    fn object_path(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join(".tandem-bucket-lock")
    }

    fn write_atomic(&self, path: &Path, data: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        // The temp name appends to the full file name instead of replacing the
        // extension: `with_extension` would map both `heads.json` and
        // `heads.bak` onto the same `heads.tmp-<pid>`, and two writers of
        // different keys would then interleave through one temp file. The
        // counter keeps two threads of one process apart as well.
        static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let file_name = path
            .file_name()
            .ok_or_else(|| anyhow!("bucket key has no file name: {}", path.display()))?
            .to_string_lossy()
            .into_owned();
        let serial = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_file_name(format!("{file_name}.tmp-{}-{serial}", std::process::id()));
        fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
        Ok(())
    }
}

impl ObjectStore for FsObjectStore {
    fn backend_name(&self) -> &'static str {
        "filesystem"
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn put_immutable(&self, key: &str, data: &[u8]) -> Result<bool> {
        let path = self.object_path(key)?;
        // Under the bucket lock, so "does it exist" and "write it" cannot be
        // split by another writer: two servers sharing a directory bucket must
        // not both believe they created the same WAL entry.
        let _lock = FileLock::acquire(&self.lock_path())?;
        if path.exists() {
            return Ok(false);
        }
        self.write_atomic(&path, data)?;
        Ok(true)
    }

    fn exists(&self, key: &str) -> Result<bool> {
        // No lock: a key that exists never stops existing, and a key being
        // written is only ever written once, so the answer is stable in the one
        // direction the caller acts on.
        Ok(self.object_path(key)?.exists())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.object_path(key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(anyhow!("read {}: {err}", path.display())),
        }
    }

    fn get_with_etag(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        Ok(self.get(key)?.map(|bytes| {
            let etag = content_etag(&bytes);
            (bytes, etag)
        }))
    }

    fn put_overwrite(&self, key: &str, data: &[u8]) -> Result<String> {
        let path = self.object_path(key)?;
        let _lock = FileLock::acquire(&self.lock_path())?;
        self.write_atomic(&path, data)?;
        Ok(content_etag(data))
    }

    fn compare_and_put(
        &self,
        key: &str,
        data: &[u8],
        expected: Option<&str>,
    ) -> std::result::Result<String, CasError> {
        let path = self.object_path(key)?;
        let _lock = FileLock::acquire(&self.lock_path())?;

        let current = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(anyhow!("read {}: {err}", path.display()).into()),
        };

        match (expected, current.as_deref()) {
            (None, None) => {}
            (None, Some(_)) => return Err(CasError::Conflict),
            (Some(_), None) => return Err(CasError::Conflict),
            (Some(expected), Some(bytes)) => {
                if content_etag(bytes) != expected {
                    return Err(CasError::Conflict);
                }
            }
        }

        self.write_atomic(&path, data)?;
        Ok(content_etag(data))
    }
}

fn content_etag(data: &[u8]) -> String {
    use blake2::Digest as _;
    let mut hasher = blake2::Blake2b512::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut etag = String::with_capacity(34);
    etag.push('"');
    for byte in &digest[..16] {
        write!(&mut etag, "{byte:02x}").expect("writing to a String cannot fail");
    }
    etag.push('"');
    etag
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("empty bucket key");
    }
    if key.starts_with('/') || key.contains("..") || key.contains('\\') {
        bail!("unsafe bucket key: {key}");
    }
    Ok(())
}

/// Advisory `flock` held for the lifetime of the value. Released by the kernel
/// if the process dies, so a crash mid-CAS cannot wedge the bucket.
struct FileLock {
    file: fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        use std::os::unix::io::AsRawFd as _;
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open bucket lock {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            bail!(
                "lock bucket {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd as _;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

// ─── S3 backend ───────────────────────────────────────────────────────────────

/// Any S3-compatible endpoint, driven through the `object_store` crate.
#[cfg(feature = "s3")]
pub struct S3ObjectStore {
    inner: Arc<dyn object_store::ObjectStore>,
    /// `Some` for the whole life of the store; `None` only inside `drop`.
    /// See the `Drop` impl below for why it has to be takeable.
    runtime: Option<tokio::runtime::Runtime>,
    prefix: String,
    description: String,
}

/// Shut the private runtime down without waiting for it.
///
/// Dropping a `Runtime` the ordinary way blocks until its worker threads stop,
/// and tokio panics rather than block when the drop happens on a reactor
/// thread. The server builds this store inside `run_serve`, which is such a
/// thread, so an error anywhere after the store is open would end as
/// "Cannot drop a runtime in a context where blocking is not allowed" — a
/// panic with none of the information the real error carried. Handing the
/// threads to `shutdown_background` lets the drop return at once, so the
/// error that started it survives to be reported.
#[cfg(feature = "s3")]
impl Drop for S3ObjectStore {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(feature = "s3")]
impl fmt::Debug for S3ObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3ObjectStore")
            .field("location", &self.description)
            .finish()
    }
}

/// Parsed `s3://…` bucket specification.
#[derive(Debug, PartialEq, Eq)]
#[cfg(feature = "s3")]
struct S3Spec {
    bucket: String,
    prefix: String,
    endpoint: Option<String>,
    region: Option<String>,
    anonymous: bool,
    allow_http: Option<bool>,
    virtual_hosted: bool,
}

#[cfg(feature = "s3")]
fn parse_s3_spec(rest: &str) -> Result<S3Spec> {
    let (location, query) = match rest.split_once('?') {
        Some((location, query)) => (location, Some(query)),
        None => (rest, None),
    };
    let (bucket, prefix) = match location.split_once('/') {
        Some((bucket, prefix)) => (bucket, prefix.trim_matches('/')),
        None => (location, ""),
    };
    if bucket.is_empty() {
        bail!("s3 bucket name is missing (expected s3://<bucket>[/<prefix>])");
    }

    let mut spec = S3Spec {
        bucket: bucket.to_string(),
        prefix: if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        },
        endpoint: std::env::var("AWS_ENDPOINT").ok(),
        region: std::env::var("AWS_REGION").ok(),
        anonymous: false,
        allow_http: None,
        virtual_hosted: false,
    };

    for pair in query.into_iter().flat_map(|q| q.split('&')) {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow!("bad bucket query parameter: {pair}"))?;
        match name {
            "endpoint" => spec.endpoint = Some(value.to_string()),
            "region" => spec.region = Some(value.to_string()),
            "anonymous" => spec.anonymous = parse_bool(value)?,
            "allow_http" => spec.allow_http = Some(parse_bool(value)?),
            "virtual_hosted" => spec.virtual_hosted = parse_bool(value)?,
            other => bail!("unknown bucket query parameter: {other}"),
        }
    }
    Ok(spec)
}

#[cfg(feature = "s3")]
fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => bail!("expected a boolean, got {other}"),
    }
}

#[cfg(feature = "s3")]
impl S3ObjectStore {
    fn open(rest: &str) -> Result<Self> {
        let spec = parse_s3_spec(rest)?;

        let mut builder = object_store::aws::AmazonS3Builder::from_env()
            .with_http_connector(http_trace::ObservedConnector)
            .with_bucket_name(&spec.bucket)
            .with_region(spec.region.clone().unwrap_or_else(|| "us-east-1".into()))
            .with_virtual_hosted_style_request(spec.virtual_hosted)
            .with_conditional_put(object_store::aws::S3ConditionalPut::ETagMatch);

        if let Some(endpoint) = &spec.endpoint {
            builder = builder.with_endpoint(endpoint.clone());
        }
        let allow_http = spec.allow_http.unwrap_or_else(|| {
            spec.endpoint
                .as_deref()
                .map(|e| e.starts_with("http://"))
                .unwrap_or(false)
        });
        builder = builder.with_allow_http(allow_http);
        if spec.anonymous {
            builder = builder.with_skip_signature(true);
        }

        let inner = builder.build().context("configure S3 bucket")?;

        // A private runtime: the caller's reactor is blocked while these
        // futures run, so they must not need it to make progress.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("tandem-bucket")
            .build()
            .context("build bucket runtime")?;

        let description = match &spec.endpoint {
            Some(endpoint) => format!("s3://{}/{} @ {endpoint}", spec.bucket, spec.prefix),
            None => format!("s3://{}/{}", spec.bucket, spec.prefix),
        };

        Ok(Self {
            inner: Arc::new(inner),
            runtime: Some(runtime),
            prefix: spec.prefix,
            description,
        })
    }

    fn location(&self, key: &str) -> Result<object_store::path::Path> {
        validate_key(key)?;
        Ok(object_store::path::Path::from(format!(
            "{}{key}",
            self.prefix
        )))
    }

    /// Run a future on the private runtime and block until it finishes.
    ///
    /// `std::sync::mpsc::Receiver::recv` is used on purpose: tokio's blocking
    /// receivers panic when called from inside a runtime, and the server's
    /// publish path always is.
    fn block_on<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let runtime = self
            .runtime
            .as_ref()
            .expect("bucket runtime is only taken while dropping the store");
        use tracing::{instrument::WithSubscriber, Instrument};
        runtime.spawn(
            async move {
                let _ = tx.send(http_trace::observe(future).await);
            }
            .instrument(tracing::Span::current())
            .with_current_subscriber(),
        );
        rx.recv().expect("bucket runtime stopped unexpectedly")
    }

    fn put_with_mode(
        &self,
        key: &str,
        data: &[u8],
        mode: object_store::PutMode,
    ) -> std::result::Result<object_store::PutResult, object_store::Error> {
        let location = match self.location(key) {
            Ok(location) => location,
            Err(err) => {
                return Err(object_store::Error::Generic {
                    store: "tandem",
                    source: err.into(),
                })
            }
        };
        let inner = self.inner.clone();
        let payload = object_store::PutPayload::from(data.to_vec());
        let options = object_store::PutOptions::from(mode);
        self.block_on(async move { inner.put_opts(&location, payload, options).await })
    }

    fn etag_of(&self, key: &str) -> Result<String> {
        tracing::debug!(key_fingerprint = %content_etag(key.as_bytes()), "S3 ETag follow-up HEAD");
        let location = self.location(key)?;
        let inner = self.inner.clone();
        let meta = self
            .block_on(async move { inner.head(&location).await })
            .with_context(|| format!("head {key}"))?;
        meta.e_tag
            .ok_or_else(|| anyhow!("bucket did not return an ETag for {key}"))
    }
}

#[cfg(feature = "s3")]
impl ObjectStore for S3ObjectStore {
    fn backend_name(&self) -> &'static str {
        "s3"
    }

    fn describe(&self) -> String {
        self.description.clone()
    }

    fn put_immutable(&self, key: &str, data: &[u8]) -> Result<bool> {
        let _span = tracing::debug_span!("S3 adapter write", key_class = http_trace::key_class(key), key_fingerprint = %content_etag(key.as_bytes())).entered();
        match self.put_with_mode(key, data, object_store::PutMode::Create) {
            Ok(_) => Ok(true),
            // An earlier attempt, or another writer, got there first.
            Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
            Err(err) => Err(anyhow!("put {key}: {err}")),
        }
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let location = self.location(key)?;
        let inner = self.inner.clone();
        match self.block_on(async move { inner.head(&location).await }) {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(anyhow!("head {key}: {err}")),
        }
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.get_with_etag(key)?.map(|(bytes, _)| bytes))
    }

    fn get_with_etag(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        let location = self.location(key)?;
        let inner = self.inner.clone();
        let result = self.block_on(async move {
            let response = inner.get(&location).await?;
            let e_tag = response.meta.e_tag.clone();
            let bytes = response.bytes().await?;
            Ok::<_, object_store::Error>((bytes, e_tag))
        });
        match result {
            Ok((bytes, e_tag)) => {
                let etag = match e_tag {
                    Some(etag) => etag,
                    None => content_etag(&bytes),
                };
                Ok(Some((bytes.to_vec(), etag)))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(anyhow!("get {key}: {err}")),
        }
    }

    fn put_overwrite(&self, key: &str, data: &[u8]) -> Result<String> {
        let _span = tracing::debug_span!("S3 adapter write", key_class = http_trace::key_class(key), key_fingerprint = %content_etag(key.as_bytes())).entered();
        let result = self
            .put_with_mode(key, data, object_store::PutMode::Overwrite)
            .map_err(|err| anyhow!("put {key}: {err}"))?;
        match result.e_tag {
            Some(etag) => Ok(etag),
            None => self.etag_of(key),
        }
    }

    fn compare_and_put(
        &self,
        key: &str,
        data: &[u8],
        expected: Option<&str>,
    ) -> std::result::Result<String, CasError> {
        let _span = tracing::debug_span!("S3 adapter write", key_class = http_trace::key_class(key), key_fingerprint = %content_etag(key.as_bytes())).entered();
        let mode = match expected {
            Some(etag) => object_store::PutMode::Update(object_store::UpdateVersion {
                e_tag: Some(etag.to_string()),
                version: None,
            }),
            None => object_store::PutMode::Create,
        };
        match self.put_with_mode(key, data, mode) {
            Ok(result) => match result.e_tag {
                Some(etag) => Ok(etag),
                None => Ok(self.etag_of(key)?),
            },
            Err(object_store::Error::Precondition { .. })
            | Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::NotFound { .. }) => Err(CasError::Conflict),
            Err(err) => Err(CasError::Other(anyhow!("conditional put {key}: {err}"))),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "s3")]
    #[test]
    fn s3_attempt_trace_distinguishes_retry_and_missing_etag_rejection() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for (status, etag) in [(503, None), (200, Some("first")), (200, None)] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let header = String::from_utf8(request).unwrap();
                methods.push(header.split_whitespace().next().unwrap().to_string());
                let length: usize = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                socket.read_exact(&mut vec![0; length]).unwrap();
                let etag = etag
                    .map(|v| format!("ETag: \"{v}\"\r\n"))
                    .unwrap_or_default();
                write!(socket, "HTTP/1.1 {status} test\r\nContent-Length: 0\r\nLast-Modified: Tue, 15 Sep 2026 00:00:00 GMT\r\n{etag}Connection: close\r\n\r\n").unwrap();
            }
            methods
        });
        let output = http_trace::test_capture();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(output.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let store = S3ObjectStore::open(&format!(
                "test/prefix?endpoint=http://{address}&anonymous=true"
            ))
            .unwrap();
            assert_eq!(
                store.compare_and_put("index/heads", b"one", None).unwrap(),
                "\"first\""
            );
            let error = store
                .compare_and_put("index/heads", b"two", Some("\"first\""))
                .unwrap_err();
            assert!(error.to_string().contains("ETag"));
        });
        assert_eq!(server.join().unwrap(), ["PUT", "PUT", "PUT"]);
        let rows = output.events();
        let attempts: Vec<_> = rows
            .iter()
            .filter(|e| e["fields"]["message"] == "S3 HTTP attempt finished")
            .collect();
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[0]["fields"]["status"], 503);
        assert_eq!(attempts[1]["fields"]["attempt"], 2);
        assert_eq!(attempts[1]["fields"]["previous_outcome"], "http_503");
        assert!(attempts[1]["fields"]["inter_attempt_us"].as_u64().unwrap() > 0);
        assert_eq!(attempts[1]["fields"]["etag_present"], true);
        assert_eq!(attempts[2]["fields"]["etag_present"], false);
        assert_eq!(
            rows.iter()
                .filter(|e| e["fields"]["message"] == "S3 ETag follow-up HEAD")
                .count(),
            0
        );
    }

    /// Every backend must behave the same way for the WAL and index writes.
    fn assert_bucket_contract(store: &dyn ObjectStore, salt: &str) {
        let immutable = format!("wal/{salt}-entry");
        let index = format!("index/{salt}-heads.json");

        assert!(
            !store.exists(&immutable).unwrap(),
            "a key that was never written must not report as present"
        );
        assert!(
            store.put_immutable(&immutable, b"first").unwrap(),
            "the first immutable write must report that it stored the bytes"
        );
        assert!(
            store.exists(&immutable).unwrap(),
            "a key that was written must report as present"
        );
        // Immutable writes are idempotent, the first write wins, and the second
        // caller is told its bytes were not stored.
        assert!(
            !store.put_immutable(&immutable, b"second").unwrap(),
            "an immutable write over an existing key must report that it stored nothing"
        );
        assert_eq!(store.get(&immutable).unwrap().unwrap(), b"first");
        assert!(store.get("wal/definitely-absent").unwrap().is_none());

        // Creating the index requires that it does not exist yet.
        let etag = store.compare_and_put(&index, b"v1", None).unwrap();
        assert!(matches!(
            store.compare_and_put(&index, b"v1-again", None),
            Err(CasError::Conflict)
        ));

        // A stale etag is rejected; the fresh one is accepted.
        assert!(matches!(
            store.compare_and_put(&index, b"v2", Some("\"stale\"")),
            Err(CasError::Conflict)
        ));
        let etag2 = store.compare_and_put(&index, b"v2", Some(&etag)).unwrap();
        assert_ne!(etag, etag2);

        let (bytes, read_etag) = store.get_with_etag(&index).unwrap().unwrap();
        assert_eq!(bytes, b"v2");
        assert_eq!(read_etag, etag2);

        assert!(probe_conditional_put(store).unwrap());
    }

    #[test]
    fn filesystem_backend_honours_the_bucket_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(tmp.path()).unwrap();
        assert_bucket_contract(&store, "fs");
        assert_eq!(store.backend_name(), "filesystem");
    }

    /// Two keys that share a stem must not share a temp file while being
    /// written, or concurrent writers of the two keys interleave.
    #[test]
    fn filesystem_backend_keeps_keys_with_a_shared_stem_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(FsObjectStore::open(tmp.path()).unwrap());

        let threads: Vec<_> = ["index/heads.json", "index/heads.bak", "index/heads"]
            .into_iter()
            .map(|key| {
                let store = store.clone();
                std::thread::spawn(move || {
                    let payload = key.repeat(4096);
                    for _ in 0..20 {
                        store.put_overwrite(key, payload.as_bytes()).unwrap();
                        let read = store.get(key).unwrap().unwrap();
                        assert_eq!(
                            read,
                            payload.as_bytes(),
                            "key {key} was written through a shared temp file"
                        );
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn filesystem_backend_rejects_unsafe_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(tmp.path()).unwrap();
        assert!(store.get("../escape").is_err());
        assert!(store.get("/absolute").is_err());
        assert!(store.get("").is_err());
    }

    #[cfg(feature = "s3")]
    #[test]
    fn s3_spec_parsing() {
        let spec = parse_s3_spec("tandem-wal/repo-a?endpoint=http://127.0.0.1:8333&anonymous=true")
            .unwrap();
        assert_eq!(spec.bucket, "tandem-wal");
        assert_eq!(spec.prefix, "repo-a/");
        assert_eq!(spec.endpoint.as_deref(), Some("http://127.0.0.1:8333"));
        assert!(spec.anonymous);
        assert!(!spec.virtual_hosted);

        let bare = parse_s3_spec("tandem-wal").unwrap();
        assert_eq!(bare.bucket, "tandem-wal");
        assert_eq!(bare.prefix, "");

        assert!(parse_s3_spec("tandem-wal?nonsense=1").is_err());
        assert!(parse_s3_spec("").is_err());
    }

    /// Tier-2 validation (see the design doc): a real S3 API from SeaweedFS.
    /// Opt in with `TANDEM_TEST_S3_BUCKET=s3://<bucket>?endpoint=...`.
    #[cfg(feature = "s3")]
    #[test]
    fn s3_backend_honours_the_bucket_contract() {
        let Ok(spec) = std::env::var("TANDEM_TEST_S3_BUCKET") else {
            eprintln!("skipping: set TANDEM_TEST_S3_BUCKET to run the S3 backend test");
            return;
        };
        let store = open(&spec).expect("open S3 bucket");
        assert_eq!(store.backend_name(), "s3");
        let salt = format!(
            "s3-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        assert_bucket_contract(store.as_ref(), &salt);
    }

    /// The server opens its bucket from inside the HTTP runtime, so the store
    /// is also dropped there whenever startup fails afterwards. A blocking
    /// drop turns that failure into a tokio panic and throws the real error
    /// away, so the drop must not block. No endpoint is contacted here:
    /// opening the store only builds the config and the private runtime.
    #[cfg(feature = "s3")]
    #[test]
    fn an_s3_store_can_be_dropped_from_inside_an_async_context() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = open("s3://tandem-drop-probe?endpoint=http://127.0.0.1:1&anonymous=true")
                .expect("open S3 bucket");
            drop(store);
        });
    }
}
