//! A content-addressed disk cache for the immutable half of the HTTP API.
//!
//! Objects, operations and views are named by a hash of what they contain, so
//! an id can only ever mean one sequence of bytes. That is the whole reason
//! this file can be as short as it is: there is no invalidation, no expiry and
//! no coherence protocol, because a key never comes to mean something else.
//! Fetching an id once per machine is therefore always correct.
//!
//! Two rules keep the cache from being able to hurt anything.
//!
//! First, it is never load-bearing. Every `get` returns `Option`, never
//! `Result`: a missing entry, an unreadable directory and a corrupt file are
//! all the same answer — "ask the server" — and none of them reaches the
//! caller as an error. A cache that can fail a read is a second source of
//! truth, and the design doc has exactly one.
//!
//! Second, an entry proves itself before it is believed. The id cannot do that
//! job: object ids come out of git's own hashing and operation ids out of
//! `blake2b` over a *decoded* struct, so re-deriving either from raw bytes
//! would mean carrying a copy of both hashing schemes here. Instead each entry
//! carries a checksum this file computed over the bytes it wrote, the same
//! independent-checksum idiom `object_store.rs` uses for the bucket. That is
//! what catches the failure a local cache actually suffers: a truncated file,
//! a half-written entry, bit rot on the disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::env::flag_enabled;
use jj_tandem_protocol::hex::to_hex;

/// Where the cache lives. Set it to bake a warm cache into an image, or to
/// give one machine's agents a cache directory of their own.
pub const CACHE_DIR_ENV: &str = "TANDEM_CACHE_DIR";

/// Turns the cache off entirely. The in-process test suites set it: they hold
/// many clients of many servers in one address space, and an oracle that
/// asserts a byte came back *over the wire* must not be answered from disk.
pub const CACHE_DISABLE_ENV: &str = "TANDEM_DISABLE_CACHE";

/// Namespace for cached operations. Objects use `wire::kind_name`, which never
/// produces this or `NAMESPACE_VIEW`.
pub const NAMESPACE_OPERATION: &str = "op";
/// Namespace for cached views.
pub const NAMESPACE_VIEW: &str = "view";

const MAGIC: &[u8; 8] = b"tdmcache";
const FORMAT_VERSION: u8 = 1;
const CHECKSUM_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + 1 + CHECKSUM_LEN;

/// A directory of content-addressed entries.
///
/// Cheap to construct — it is a path and nothing else — so every client can
/// hold one without any process-wide handle to share.
#[derive(Debug, Clone)]
pub struct DiskCache {
    root: PathBuf,
}

impl DiskCache {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The cache this process should use, or `None` if it has none: either it
    /// was switched off, or there is nowhere on this machine to put one.
    pub fn from_environment() -> Option<Arc<Self>> {
        Self::from_lookup(env_lookup)
    }

    /// The same decision, over a supplied environment.
    ///
    /// Tests take this path instead of setting variables: the suites run many
    /// threads, and `setenv` racing `getenv` is a data race whatever the value
    /// is.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Arc<Self>> {
        if flag_enabled(lookup(CACHE_DISABLE_ENV).as_deref()) {
            return None;
        }
        resolve_root(lookup).map(|root| Arc::new(Self::open(root)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The bytes stored under `(namespace, id)`, if they are there and intact.
    ///
    /// A corrupt entry is deleted on the way out, so the refetch that follows
    /// also repairs the cache rather than tripping over the same file forever.
    pub fn get(&self, namespace: &str, id: &[u8]) -> Option<Vec<u8>> {
        let path = self.entry_path(namespace, id)?;
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "cache read failed");
                return None;
            }
        };

        match decode_entry(&raw) {
            Some(data) => Some(data),
            None => {
                tracing::warn!(
                    path = %path.display(),
                    "discarding a corrupt cache entry; refetching from the server"
                );
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    /// Store bytes under `(namespace, id)`. Best effort: a cache that cannot
    /// be written is a slower machine, not a broken one.
    pub fn put(&self, namespace: &str, id: &[u8], data: &[u8]) {
        let Some(path) = self.entry_path(namespace, id) else {
            return;
        };
        // Content-addressed, so an entry that is already there is already
        // right. Skipping the rewrite keeps a hot read path off the disk.
        if path.exists() {
            return;
        }
        if let Err(err) = write_atomic(&path, &encode_entry(data)) {
            tracing::debug!(path = %path.display(), error = %err, "cache write failed");
        }
    }

    /// `<root>/<namespace>/<first two hex digits>/<rest>`.
    ///
    /// The two-digit fanout is git's, for the same reason: a flat directory of
    /// every blob a repo has ever had is slow to walk on every filesystem that
    /// has an opinion about it.
    fn entry_path(&self, namespace: &str, id: &[u8]) -> Option<PathBuf> {
        if !is_safe_namespace(namespace) || id.is_empty() {
            tracing::debug!(namespace, "refusing an unsafe cache key");
            return None;
        }
        let hex = to_hex(id);
        let dir = self.root.join(namespace);
        Some(if hex.len() > 2 {
            dir.join(&hex[..2]).join(&hex[2..])
        } else {
            dir.join(&hex)
        })
    }
}

// ─── Entry format ─────────────────────────────────────────────────────────────

fn encode_entry(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&checksum(data));
    out.extend_from_slice(data);
    out
}

/// The payload of a well-formed entry whose checksum still matches, or `None`
/// for anything else — a truncated file, a foreign file, a future format, a
/// flipped bit.
fn decode_entry(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < HEADER_LEN {
        return None;
    }
    if &raw[..MAGIC.len()] != MAGIC || raw[MAGIC.len()] != FORMAT_VERSION {
        return None;
    }
    let stored = &raw[MAGIC.len() + 1..HEADER_LEN];
    let data = &raw[HEADER_LEN..];
    if stored != checksum(data) {
        return None;
    }
    Some(data.to_vec())
}

fn checksum(data: &[u8]) -> [u8; CHECKSUM_LEN] {
    use blake2::Digest as _;
    let mut hasher = blake2::Blake2b512::new();
    hasher.update(data);
    let digest = hasher.finalize();
    let mut out = [0u8; CHECKSUM_LEN];
    out.copy_from_slice(&digest[..CHECKSUM_LEN]);
    out
}

// ─── Disk ─────────────────────────────────────────────────────────────────────

/// Write through a unique temp name and rename into place.
///
/// No lock, unlike the bucket store: keys are content hashes, so two writers
/// of one key write identical bytes, and the rename means a reader sees either
/// nothing or the whole entry. The temp name appends to the file name rather
/// than replacing an extension, and carries a per-process counter, so two
/// writers never share one temp path.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache entry path has no parent"))?;
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("cache entry path has no file name"))?
        .to_string_lossy()
        .into_owned();
    let serial = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{file_name}.tmp-{}-{serial}", std::process::id()));

    match fs::write(&tmp, data).and_then(|()| fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

// ─── Location ─────────────────────────────────────────────────────────────────

/// Where the cache goes: the override first, then the XDG cache home, then the
/// conventional place inside a home directory. A machine with none of the
/// three has nowhere durable to write, so it gets no cache at all rather than
/// a directory somebody has to find later.
pub fn resolve_root(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = lookup(CACHE_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = lookup("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg).join("tandem"));
    }
    if let Some(home) = lookup("HOME") {
        return Some(PathBuf::from(home).join(".cache").join("tandem"));
    }
    None
}

/// An environment variable, treating "set but empty" as unset — an empty
/// `TANDEM_CACHE_DIR` means "I did not set this", not "cache at the root".
fn env_lookup(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// A namespace becomes one path segment, and the kind half of a cache key is
/// whatever the caller passed. Nothing in the tree passes anything but a fixed
/// string, and this is why it can stay that way.
fn is_safe_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn an_entry_reads_back_byte_identical() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());

        cache.put("file", b"\x01\x02\x03", b"hello");
        assert_eq!(
            cache.get("file", b"\x01\x02\x03").as_deref(),
            Some(&b"hello"[..])
        );
    }

    #[test]
    fn a_missing_entry_is_a_miss_and_not_an_error() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        assert_eq!(cache.get("file", b"\xaa\xbb"), None);
    }

    #[test]
    fn the_kind_is_part_of_the_key() {
        // A file and a symlink can hash to the same git object id, because both
        // are stored as plain blobs. Serving one for the other would be silent
        // corruption, so the namespace has to separate them.
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());

        cache.put("file", b"\x01", b"contents");
        cache.put("symlink", b"\x01", b"target");

        assert_eq!(
            cache.get("file", b"\x01").as_deref(),
            Some(&b"contents"[..])
        );
        assert_eq!(
            cache.get("symlink", b"\x01").as_deref(),
            Some(&b"target"[..])
        );
    }

    #[test]
    fn a_corrupt_entry_is_detected_and_dropped() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        cache.put("tree", b"\x10\x20", b"the original bytes");

        let path = cache.entry_path("tree", b"\x10\x20").expect("entry path");
        let mut raw = fs::read(&path).expect("read the entry");
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        fs::write(&path, &raw).expect("corrupt the entry");

        assert_eq!(cache.get("tree", b"\x10\x20"), None, "corruption must miss");
        assert!(!path.exists(), "a corrupt entry must be removed, not kept");

        // And the cache repairs itself: the refetch that follows a miss writes
        // the entry again.
        cache.put("tree", b"\x10\x20", b"the original bytes");
        assert_eq!(
            cache.get("tree", b"\x10\x20").as_deref(),
            Some(&b"the original bytes"[..])
        );
    }

    #[test]
    fn a_truncated_entry_is_a_miss() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        cache.put("commit", b"\x33", b"0123456789");

        let path = cache.entry_path("commit", b"\x33").expect("entry path");
        let raw = fs::read(&path).expect("read the entry");
        fs::write(&path, &raw[..raw.len() - 4]).expect("truncate the entry");

        assert_eq!(cache.get("commit", b"\x33"), None);
    }

    #[test]
    fn a_foreign_file_in_the_cache_is_a_miss() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        let path = cache.entry_path("op", b"\x44\x55").expect("entry path");
        fs::create_dir_all(path.parent().unwrap()).expect("create the entry directory");
        fs::write(&path, b"something else entirely").expect("write a foreign file");

        assert_eq!(cache.get("op", b"\x44\x55"), None);
    }

    #[test]
    fn an_empty_payload_round_trips() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        cache.put("file", b"\x99", b"");
        assert_eq!(cache.get("file", b"\x99").as_deref(), Some(&b""[..]));
    }

    #[test]
    fn an_unsafe_namespace_is_refused_rather_than_joined() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        for namespace in ["", "..", "a/b", "../escape", "a\\b"] {
            assert!(
                cache.entry_path(namespace, b"\x01").is_none(),
                "namespace {namespace:?} must be refused"
            );
            cache.put(namespace, b"\x01", b"payload");
            assert_eq!(cache.get(namespace, b"\x01"), None);
        }
    }

    #[test]
    fn two_writers_of_one_key_leave_one_good_entry() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = cache.clone();
                scope.spawn(move || cache.put("file", b"\x07", b"identical bytes"));
            }
        });
        assert_eq!(
            cache.get("file", b"\x07").as_deref(),
            Some(&b"identical bytes"[..])
        );
    }

    #[test]
    fn no_temp_files_are_left_behind() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let cache = DiskCache::open(tmp.path());
        cache.put("file", b"\x01\x02", b"payload");
        let dir = cache
            .entry_path("file", b"\x01\x02")
            .expect("entry path")
            .parent()
            .expect("entry directory")
            .to_path_buf();
        let names: Vec<String> = fs::read_dir(&dir)
            .expect("read the entry directory")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "unexpected files in the cache: {names:?}");
        assert!(
            !names[0].contains(".tmp-"),
            "temp file left behind: {names:?}"
        );
    }

    #[test]
    fn the_location_override_wins_over_every_default() {
        let root = resolve_root(lookup_from(&[
            (CACHE_DIR_ENV, "/baked/into/the/image"),
            ("XDG_CACHE_HOME", "/home/someone/.cache"),
            ("HOME", "/home/someone"),
        ]));
        assert_eq!(root, Some(PathBuf::from("/baked/into/the/image")));
    }

    #[test]
    fn without_an_override_the_cache_home_is_used() {
        let root = resolve_root(lookup_from(&[
            ("XDG_CACHE_HOME", "/home/someone/.cache"),
            ("HOME", "/home/someone"),
        ]));
        assert_eq!(root, Some(PathBuf::from("/home/someone/.cache/tandem")));
    }

    #[test]
    fn without_a_cache_home_the_home_directory_is_used() {
        let root = resolve_root(lookup_from(&[("HOME", "/home/someone")]));
        assert_eq!(root, Some(PathBuf::from("/home/someone/.cache/tandem")));
    }

    #[test]
    fn a_machine_with_no_home_gets_no_cache() {
        assert_eq!(resolve_root(lookup_from(&[])), None);
        assert!(DiskCache::from_lookup(lookup_from(&[])).is_none());
    }

    #[test]
    fn the_kill_switch_wins_over_the_location() {
        let cache = DiskCache::from_lookup(lookup_from(&[
            (CACHE_DIR_ENV, "/somewhere"),
            (CACHE_DISABLE_ENV, "1"),
        ]));
        assert!(cache.is_none());

        let still_on = DiskCache::from_lookup(lookup_from(&[
            (CACHE_DIR_ENV, "/somewhere"),
            (CACHE_DISABLE_ENV, "0"),
        ]));
        assert_eq!(
            still_on.map(|c| c.root().to_path_buf()),
            Some(PathBuf::from("/somewhere"))
        );
    }
}
