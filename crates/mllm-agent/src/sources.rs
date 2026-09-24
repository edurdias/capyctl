//! ADR 0008: materializing a declared remote model source into the host's
//! model store.
//!
//! SPEC §1.2 (as amended by ADR 0008) keeps implicit download out of scope; a
//! declared `huggingface` or `http` source is explicit intent, so the host
//! fetches it when the controller asks (`MaterializeSource`) and never
//! otherwise. Engines are unchanged: they read the local directory the
//! source resolves to (`<store>/sources/...`), with the offline environment
//! they always had.
//!
//! The model store is a charged filesystem resource owner (SPEC §7). Before a
//! byte is written, the download's full size (from the Hugging Face listing or
//! the response's length) is reserved against the host's
//! `model_sources.max_bytes` ceiling and the filesystem's free space, and the
//! reservation is persisted. It stays charged while the download runs and
//! while partial files remain on disk; it is released only when the
//! temporary directory is verifiably gone (SPEC §7.3: uncertainty retains
//! accounting), or converted into the verified copy's charge on commit.
//!
//! Every file is verified before it is committed: Hugging Face LFS files by
//! the SHA-256 in their pointer, other repository files by their git blob id,
//! an `http` payload by its declared SHA-256. The verified tree is renamed
//! into place atomically; the WE3 checkpoint digest (ADR 0014 §7) is then
//! measured over it as over any local checkpoint.
//!
//! Downloads are idempotent and resumable: a second request for the same
//! source shares the running download, a request after an agent restart
//! resumes from the partial files with range requests, and a verified copy is
//! reused by every deployment that declares the same pinned source.
//!
//! Secrets: a `token_ref` is resolved from the host's secret directory at the
//! moment of use, sent only as an `Authorization` header marked sensitive, and
//! never written to a file, an error, a status or a log line.

mod tar;

use futures::StreamExt;
use mllm_config::model_source::{pattern_matches, secret_name, Archive, ModelSource};
use mllm_config::effective::ModelSourcePolicy;
use serde::{Deserialize, Serialize};
use sha1::Digest as _;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Where mllm keeps its own bookkeeping under `<store>/sources`.
const STATE_DIR: &str = ".mllm";
/// Largest listing a Hugging Face revision may return.
const MAX_LISTING_BYTES: usize = 16 << 20;
/// Most files one source may select.
const MAX_FILES: usize = 100_000;

/// The closed failure categories a materialization reports (ADR 0008).
pub use mllm_config::model_source::reason;

/// A failed materialization: a closed category, and whether its reservation
/// is still charged (partial files remain, or their removal was not proven).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceFailure {
    pub reason: &'static str,
    pub reservation_retained: bool,
}

impl SourceFailure {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            reservation_retained: false,
        }
    }
}

/// What a host can say about one source right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SourceStatus {
    /// Nothing is known or running for it.
    Pending,
    /// A download is running. `bytes_total` is zero until it is sized.
    Downloading { bytes_done: u64, bytes_total: u64 },
    /// A verified copy is in place.
    Verified { bytes: u64 },
    /// The last attempt failed.
    Failed(SourceFailure),
}

/// A secret value. Never printed, serialized or compared in the open.
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// One running download.
struct Job {
    done: AtomicU64,
    total: AtomicU64,
    finished: tokio::sync::watch::Sender<Option<Result<u64, SourceFailure>>>,
}

/// A durable reservation of store bytes for one source (SPEC §7.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    key: String,
    bytes: u64,
}

/// The durable record of a committed (or committing) source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    key: String,
    /// `committing` is written before the atomic rename, `verified` after.
    state: String,
    bytes: u64,
    files: u64,
}

/// One file a source consists of.
#[derive(Debug, Clone)]
struct PlannedFile {
    path: String,
    url: reqwest::Url,
    size: u64,
    expected: Expected,
}

#[derive(Debug, Clone)]
enum Expected {
    Sha256(String),
    /// A git blob id: SHA-1 over `blob <size>\0` and the contents.
    GitBlob(String),
}

/// Host-side materialization of declared model sources into one model store.
pub struct SourceStore {
    store: PathBuf,
    policy: ModelSourcePolicy,
    secrets_dir: Option<PathBuf>,
    client: reqwest::Client,
    /// Test seam: every HTTPS origin is served by this loopback HTTP origin.
    loopback_origin: Option<reqwest::Url>,
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    failures: Mutex<BTreeMap<String, SourceFailure>>,
    accounting: Mutex<()>,
    log: Mutex<LogSink>,
}

/// Where progress and failure lines go.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

fn io_failure(_: io::Error) -> SourceFailure {
    SourceFailure::new(reason::IO_ERROR)
}

impl SourceStore {
    /// A store for `model_store` under `policy`. Secrets are read from
    /// `secrets_dir/<name>` (owner-only files).
    pub fn new(
        model_store: &Path,
        policy: ModelSourcePolicy,
        secrets_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::build(model_store, policy, secrets_dir, None)
    }

    /// Test seam: serve every HTTPS origin from `origin`, which must be a
    /// loopback `http://127.0.0.1:<port>` address. A non-loopback origin is
    /// ignored, so this cannot downgrade a real fetch to plain HTTP.
    #[doc(hidden)]
    pub fn with_loopback_origin(
        model_store: &Path,
        policy: ModelSourcePolicy,
        secrets_dir: Option<PathBuf>,
        origin: &str,
    ) -> Arc<Self> {
        let origin = reqwest::Url::parse(origin).ok().filter(|url| {
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
        });
        Self::build(model_store, policy, secrets_dir, origin)
    }

    fn build(
        model_store: &Path,
        policy: ModelSourcePolicy,
        secrets_dir: Option<PathBuf>,
        loopback_origin: Option<reqwest::Url>,
    ) -> Arc<Self> {
        let loopback = loopback_origin.is_some();
        // SPEC §13.3: redirects stay on HTTPS (a CDN host is fine; the
        // content is verified against its pin either way). reqwest drops the
        // Authorization header when a redirect leaves the origin host.
        let redirect = reqwest::redirect::Policy::custom(move |attempt| {
            let url = attempt.url();
            let loopback_url = loopback
                && url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if url.scheme() == "https" || loopback_url {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        let client = reqwest::Client::builder()
            .redirect(redirect)
            .connect_timeout(std::time::Duration::from_secs(30))
            .read_timeout(std::time::Duration::from_secs(120))
            .https_only(!loopback)
            .build()
            .expect("the HTTP client configuration is static");
        Arc::new(Self {
            store: model_store.to_path_buf(),
            policy,
            secrets_dir,
            client,
            loopback_origin,
            jobs: Mutex::default(),
            failures: Mutex::default(),
            accounting: Mutex::default(),
            log: Mutex::new(Arc::new(|line| eprintln!("{line}"))),
        })
    }

    /// Replace where progress and failure lines go (tests capture them).
    pub fn set_log(&self, log: LogSink) {
        if let Ok(mut current) = self.log.lock() {
            *current = log;
        }
    }

    fn log(&self, line: &str) {
        if let Ok(log) = self.log.lock() {
            log(line);
        }
    }

    fn sources_root(&self) -> PathBuf {
        self.store.join(mllm_config::model_source::SOURCES_DIR)
    }
    fn state_dir(&self) -> PathBuf {
        self.sources_root().join(STATE_DIR)
    }
    fn partial_dir(&self, id: &str) -> PathBuf {
        self.state_dir().join("partial").join(id)
    }

    /// The directory a source materializes into.
    pub fn directory(&self, source: &ModelSource) -> Option<PathBuf> {
        source.store_key().map(|key| self.store.join(key))
    }

    /// What is known about `source` now, without starting anything.
    pub fn status(&self, source: &ModelSource) -> SourceStatus {
        let Some(key) = source.store_key() else {
            return SourceStatus::Failed(SourceFailure::new(reason::NOT_REMOTE));
        };
        if let Some(bytes) = self.verified(&key) {
            return SourceStatus::Verified { bytes };
        }
        if let Some(job) = self.jobs.lock().ok().and_then(|jobs| jobs.get(&key).cloned()) {
            return SourceStatus::Downloading {
                bytes_done: job.done.load(Ordering::Relaxed),
                bytes_total: job.total.load(Ordering::Relaxed),
            };
        }
        match self.failures.lock().ok().and_then(|f| f.get(&key).cloned()) {
            Some(failure) => SourceStatus::Failed(failure),
            None => SourceStatus::Pending,
        }
    }

    /// ADR 0008 (`MaterializeSource`): report on `source`, starting its
    /// download when nothing is running and no verified copy exists. A
    /// failure is reported once; the next request starts a fresh attempt
    /// (resuming any partial files). Must run inside a Tokio runtime.
    pub fn request(self: &Arc<Self>, source: &ModelSource) -> SourceStatus {
        let Some(key) = source.store_key() else {
            return SourceStatus::Failed(SourceFailure::new(reason::NOT_REMOTE));
        };
        if self.policy.permits(source).is_err() {
            return SourceStatus::Failed(SourceFailure::new(reason::DENIED));
        }
        if let Some(bytes) = self.verified(&key) {
            return SourceStatus::Verified { bytes };
        }
        let Ok(mut jobs) = self.jobs.lock() else {
            return SourceStatus::Failed(SourceFailure::new(reason::IO_ERROR));
        };
        if let Some(job) = jobs.get(&key) {
            return SourceStatus::Downloading {
                bytes_done: job.done.load(Ordering::Relaxed),
                bytes_total: job.total.load(Ordering::Relaxed),
            };
        }
        if let Some(failure) = self.failures.lock().ok().and_then(|mut f| f.remove(&key)) {
            return SourceStatus::Failed(failure);
        }
        let (finished, _) = tokio::sync::watch::channel(None);
        let job = Arc::new(Job {
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            finished,
        });
        jobs.insert(key.clone(), job.clone());
        drop(jobs);
        let this = self.clone();
        let source = source.clone();
        tokio::spawn(async move {
            let outcome = this.run(&key, &source, &job).await;
            if let Err(failure) = &outcome {
                this.log(&format!(
                    "model source {key}: materialization failed: {} (reservation {})",
                    failure.reason,
                    if failure.reservation_retained { "retained" } else { "released" }
                ));
                if let Ok(mut failures) = this.failures.lock() {
                    failures.insert(key.clone(), failure.clone());
                }
            }
            if let Ok(mut jobs) = this.jobs.lock() {
                jobs.remove(&key);
            }
            job.finished.send_replace(Some(outcome));
        });
        SourceStatus::Downloading {
            bytes_done: 0,
            bytes_total: 0,
        }
    }

    /// Materialize `source` and wait for the outcome: the verified bytes, or
    /// the failure (consumed, like a request that observed it).
    pub async fn materialize(self: &Arc<Self>, source: &ModelSource) -> Result<u64, SourceFailure> {
        loop {
            match self.request(source) {
                SourceStatus::Verified { bytes } => return Ok(bytes),
                SourceStatus::Failed(failure) => return Err(failure),
                SourceStatus::Pending | SourceStatus::Downloading { .. } => {}
            }
            let key = source.store_key().unwrap_or_default();
            let job = self.jobs.lock().ok().and_then(|jobs| jobs.get(&key).cloned());
            let Some(job) = job else {
                continue;
            };
            let mut finished = job.finished.subscribe();
            let outcome = finished
                .wait_for(Option::is_some)
                .await
                .map(|outcome| outcome.clone());
            if let Ok(Some(outcome)) = outcome {
                if let Err(failure) = &outcome {
                    if let Ok(mut failures) = self.failures.lock() {
                        failures.remove(&key);
                    }
                    return Err(failure.clone());
                }
                return outcome;
            }
        }
    }

    fn id(key: &str) -> String {
        hex::encode(&sha2::Sha256::digest(key.as_bytes())[..12])
    }

    fn read_marker(&self, id: &str) -> Option<Marker> {
        let text = fs::read(self.state_dir().join(format!("{id}.verified"))).ok()?;
        serde_json::from_slice(&text).ok()
    }

    /// The verified copy's bytes, when one is in place.
    fn verified(&self, key: &str) -> Option<u64> {
        let marker = self.read_marker(&Self::id(key))?;
        (marker.key == key && marker.state == "verified" && self.store.join(key).is_dir())
            .then_some(marker.bytes)
    }

    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        let temporary = path.with_extension("tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(self.state_dir())?.sync_all()
    }

    async fn run(&self, key: &str, source: &ModelSource, job: &Job) -> Result<u64, SourceFailure> {
        let id = Self::id(key);
        fs::create_dir_all(self.state_dir().join("partial")).map_err(io_failure)?;
        // One download per source, across processes too (prune takes it).
        let lock = lock_exclusive(&self.state_dir().join(format!("{id}.lock")))
            .map_err(io_failure)?
            .ok_or(SourceFailure::new(reason::BUSY))?;
        let target = self.store.join(key);
        // A commit interrupted between the rename and the marker update: the
        // rename happens only after every file was verified.
        if let Some(marker) = self.read_marker(&id) {
            if marker.key == key && target.is_dir() {
                if marker.state != "verified" {
                    self.write_json(
                        &self.state_dir().join(format!("{id}.verified")),
                        &Marker {
                            state: "verified".into(),
                            ..marker.clone()
                        },
                    )
                    .map_err(io_failure)?;
                }
                self.release(&id).map_err(io_failure)?;
                drop(lock);
                return Ok(marker.bytes);
            }
        }
        if target.exists() {
            return Err(SourceFailure::new(reason::STORE_CONFLICT));
        }
        let outcome = self.download(key, &id, source, job).await;
        let outcome = match outcome {
            Ok(bytes) => Ok(bytes),
            Err(mut failure) => {
                // Terminal failures discard their partial files; transient ones
                // keep them for a resume. Either way the reservation is released
                // only on evidence that nothing it covered remains.
                if reason::terminal(failure.reason) {
                    let partial = self.partial_dir(&id);
                    let _ = fs::remove_dir_all(&partial);
                    if partial.exists() || self.release(&id).is_err() {
                        failure.reservation_retained = true;
                    }
                } else {
                    failure.reservation_retained =
                        self.state_dir().join(format!("{id}.reservation")).exists();
                }
                Err(failure)
            }
        };
        drop(lock);
        outcome
    }

    /// Delete a reservation record (the bytes it covered are gone or now
    /// charged to a verified copy).
    fn release(&self, id: &str) -> io::Result<()> {
        match fs::remove_file(self.state_dir().join(format!("{id}.reservation"))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// SPEC §7.3: charge `bytes` for source `id` before anything is written.
    fn reserve(&self, id: &str, key: &str, bytes: u64) -> Result<(), SourceFailure> {
        let _guard = self
            .accounting
            .lock()
            .map_err(|_| SourceFailure::new(reason::IO_ERROR))?;
        let (charged, outstanding) = self.charges(Some(id)).map_err(io_failure)?;
        if let Some(limit) = self.policy.max_bytes {
            let limit = u64::try_from(limit).unwrap_or(0);
            if charged.saturating_add(bytes) > limit {
                return Err(SourceFailure::new(reason::TOO_LARGE));
            }
        }
        let own = dir_bytes(&self.partial_dir(id));
        let needed = outstanding.saturating_add(bytes.saturating_sub(own));
        if free_bytes(&self.store).is_some_and(|free| free < needed) {
            return Err(SourceFailure::new(reason::INSUFFICIENT_SPACE));
        }
        self.write_json(
            &self.state_dir().join(format!("{id}.reservation")),
            &Reservation {
                key: key.into(),
                bytes,
            },
        )
        .map_err(io_failure)
    }

    /// The store bytes charged to sources (verified copies and reservations,
    /// excluding `except`), and the part of the reservations not yet on disk.
    fn charges(&self, except: Option<&str>) -> io::Result<(u64, u64)> {
        let mut charged = 0_u64;
        let mut outstanding = 0_u64;
        let entries = match fs::read_dir(self.state_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".verified") {
                if Some(id) == except {
                    continue;
                }
                // A committing copy is still charged by its reservation.
                if let Some(marker) = self.read_marker(id).filter(|m| m.state == "verified") {
                    charged = charged.saturating_add(marker.bytes);
                }
            } else if let Some(id) = name.strip_suffix(".reservation") {
                if Some(id) == except {
                    continue;
                }
                let reservation: Reservation = serde_json::from_slice(&fs::read(entry.path())?)
                    .map_err(io::Error::other)?;
                // A reservation whose copy was already committed is charged by
                // its marker instead.
                if self.read_marker(id).is_some_and(|m| m.state == "verified") {
                    continue;
                }
                charged = charged.saturating_add(reservation.bytes);
                outstanding = outstanding
                    .saturating_add(reservation.bytes.saturating_sub(dir_bytes(&self.partial_dir(id))));
            }
        }
        Ok((charged, outstanding))
    }

    fn origin(&self, url: &str) -> Result<reqwest::Url, SourceFailure> {
        let mut parsed =
            reqwest::Url::parse(url).map_err(|_| SourceFailure::new(reason::INVALID_LISTING))?;
        if let Some(origin) = &self.loopback_origin {
            let _ = parsed.set_scheme(origin.scheme());
            let _ = parsed.set_host(origin.host_str());
            let _ = parsed.set_port(origin.port());
        }
        Ok(parsed)
    }

    fn token(&self, source: &ModelSource) -> Result<Option<Secret>, SourceFailure> {
        let ModelSource::HuggingFace {
            token_ref: Some(reference),
            ..
        } = source
        else {
            return Ok(None);
        };
        let unavailable = || SourceFailure::new(reason::SECRET_UNAVAILABLE);
        let name = secret_name(reference).ok_or_else(unavailable)?;
        let path = self.secrets_dir.as_ref().ok_or_else(unavailable)?.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|_| unavailable())?;
        // SPEC §13.3: a credential is private state; no group or other access,
        // owned by the account this agent runs as.
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        if !metadata.is_file() || metadata.mode() & 0o077 != 0 || metadata.uid() != euid {
            return Err(unavailable());
        }
        let mut text = String::new();
        fs::File::open(&path)
            .and_then(|file| file.take(8192).read_to_string(&mut text))
            .map_err(|_| unavailable())?;
        let token = text.trim().to_owned();
        if token.is_empty() || token.chars().any(|c| c.is_control() || c == ' ') {
            return Err(unavailable());
        }
        Ok(Some(Secret(token)))
    }

    fn get(&self, url: reqwest::Url, token: Option<&Secret>, offset: u64) -> reqwest::RequestBuilder {
        let mut request = self.client.get(url);
        if let Some(token) = token {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.0))
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""));
            value.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        if offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
        }
        request
    }

    async fn download(
        &self,
        key: &str,
        id: &str,
        source: &ModelSource,
        job: &Job,
    ) -> Result<u64, SourceFailure> {
        let token = self.token(source)?;
        let partial = self.partial_dir(id);
        let tree = partial.join("tree");
        fs::create_dir_all(&tree).map_err(io_failure)?;
        let (bytes, files) = match source {
            ModelSource::HuggingFace {
                repo,
                revision,
                files,
                ..
            } => {
                let plan = self.plan(repo, revision, files, token.as_ref()).await?;
                let total = plan.iter().map(|f| f.size).sum::<u64>();
                job.total.store(total, Ordering::Relaxed);
                // SPEC §7.3: reserved before any file byte is written.
                self.reserve(id, key, total)?;
                for file in &plan {
                    self.fetch_file(file, &tree, token.as_ref(), job).await?;
                }
                (total, plan.len() as u64)
            }
            ModelSource::Http {
                url,
                sha256,
                archive,
            } => self.fetch_http(key, id, url, sha256, *archive, &partial, job).await?,
            ModelSource::Local { .. } => return Err(SourceFailure::new(reason::NOT_REMOTE)),
        };
        self.commit(key, id, &tree, bytes, files)?;
        self.log(&format!("model source {key}: verified ({bytes} bytes, {files} files)"));
        Ok(bytes)
    }

    /// The Hugging Face files of `repo` at `revision` that `patterns` select,
    /// with their sizes and pins, from the hub's listing.
    async fn plan(
        &self,
        repo: &str,
        revision: &str,
        patterns: &[String],
        token: Option<&Secret>,
    ) -> Result<Vec<PlannedFile>, SourceFailure> {
        #[derive(Deserialize)]
        struct Lfs {
            sha256: String,
            size: u64,
        }
        #[derive(Deserialize)]
        struct Sibling {
            rfilename: String,
            #[serde(default)]
            size: Option<u64>,
            #[serde(default, rename = "blobId")]
            blob_id: Option<String>,
            #[serde(default)]
            lfs: Option<Lfs>,
        }
        #[derive(Deserialize)]
        struct Listing {
            sha: String,
            siblings: Vec<Sibling>,
        }
        let endpoint = self.policy.huggingface_endpoint();
        let listing_url = self.origin(&format!(
            "{endpoint}/api/models/{repo}/revision/{revision}?blobs=true"
        ))?;
        let response = self
            .get(listing_url, token, 0)
            .send()
            .await
            .map_err(|_| SourceFailure::new(reason::NETWORK))?;
        check_status(response.status())?;
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| SourceFailure::new(reason::NETWORK))?;
            body.extend_from_slice(&chunk);
            if body.len() > MAX_LISTING_BYTES {
                return Err(SourceFailure::new(reason::INVALID_LISTING));
            }
        }
        let listing: Listing =
            serde_json::from_slice(&body).map_err(|_| SourceFailure::new(reason::INVALID_LISTING))?;
        // The hub must answer for the pinned commit, not whatever a name moved to.
        if listing.sha != revision {
            return Err(SourceFailure::new(reason::INVALID_LISTING));
        }
        let mut plan = Vec::new();
        let mut seen = BTreeSet::new();
        for sibling in listing.siblings {
            if !patterns.is_empty() && !patterns.iter().any(|p| pattern_matches(p, &sibling.rfilename)) {
                continue;
            }
            if !safe_repository_path(&sibling.rfilename) || !seen.insert(sibling.rfilename.clone()) {
                return Err(SourceFailure::new(reason::INVALID_LISTING));
            }
            let (size, expected) = match (sibling.lfs, sibling.blob_id, sibling.size) {
                (Some(lfs), _, _) if mllm_config::model_source::is_sha256_hex(&lfs.sha256) => {
                    (lfs.size, Expected::Sha256(lfs.sha256))
                }
                (None, Some(blob), Some(size))
                    if blob.len() == 40 && blob.bytes().all(|b| b.is_ascii_hexdigit()) =>
                {
                    (size, Expected::GitBlob(blob.to_ascii_lowercase()))
                }
                _ => return Err(SourceFailure::new(reason::INVALID_LISTING)),
            };
            let mut url = self.origin(&format!("{endpoint}/"))?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| SourceFailure::new(reason::INVALID_LISTING))?;
                segments.pop_if_empty();
                segments.extend(repo.split('/'));
                segments.push("resolve");
                segments.push(revision);
                segments.extend(sibling.rfilename.split('/'));
            }
            plan.push(PlannedFile {
                path: sibling.rfilename,
                url,
                size,
                expected,
            });
            if plan.len() > MAX_FILES {
                return Err(SourceFailure::new(reason::INVALID_LISTING));
            }
        }
        if plan.is_empty() {
            return Err(SourceFailure::new(reason::NOT_FOUND));
        }
        Ok(plan)
    }

    /// Download one repository file into `tree`, resuming a `.part` file and
    /// verifying its pin before it takes its name.
    async fn fetch_file(
        &self,
        file: &PlannedFile,
        tree: &Path,
        token: Option<&Secret>,
        job: &Job,
    ) -> Result<(), SourceFailure> {
        let destination = tree.join(&file.path);
        if destination.is_file() {
            // Verified by an earlier attempt before it was renamed.
            job.done.fetch_add(file.size, Ordering::Relaxed);
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(io_failure)?;
        }
        let part = part_path(&destination);
        let mut hasher = Hasher::new(&file.expected, file.size);
        let offset = self.resume(&part, &mut hasher, file.size)?;
        job.done.fetch_add(offset, Ordering::Relaxed);
        self.stream_into(None, file.url.clone(), token, &part, offset, file.size, &mut hasher, job)
            .await?;
        if !hasher.matches(&file.expected) {
            return Err(SourceFailure::new(reason::HASH_MISMATCH));
        }
        fs::rename(&part, &destination).map_err(io_failure)
    }

    /// Feed an existing partial file into `hasher`; its length is the offset
    /// to resume from. A partial file longer than the whole is discarded.
    fn resume(&self, part: &Path, hasher: &mut Hasher, size: u64) -> Result<u64, SourceFailure> {
        let Ok(mut existing) = fs::File::open(part) else {
            return Ok(0);
        };
        let length = existing.metadata().map_err(io_failure)?.len();
        if length > size {
            drop(existing);
            fs::remove_file(part).map_err(io_failure)?;
            return Ok(0);
        }
        let mut buffer = vec![0_u8; 1 << 20];
        loop {
            let read = existing.read(&mut buffer).map_err(io_failure)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(length)
    }

    /// Stream `url` from `offset` into `part`, never beyond `size` bytes.
    /// `opened` is a response already requested from `offset`, if any.
    #[allow(clippy::too_many_arguments)]
    async fn stream_into(
        &self,
        opened: Option<reqwest::Response>,
        url: reqwest::Url,
        token: Option<&Secret>,
        part: &Path,
        mut offset: u64,
        size: u64,
        hasher: &mut Hasher,
        job: &Job,
    ) -> Result<(), SourceFailure> {
        if offset == size {
            return Ok(());
        }
        let response = match opened {
            Some(response) => response,
            None => self
                .get(url, token, offset)
                .send()
                .await
                .map_err(|_| SourceFailure::new(reason::NETWORK))?,
        };
        check_status(response.status())?;
        if offset > 0 && response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            // The origin ignored the range: start over.
            job.done.fetch_sub(offset, Ordering::Relaxed);
            offset = 0;
            *hasher = hasher.restart(size);
        }
        let mut output = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(offset == 0)
            .append(offset > 0)
            .open(part)
            .map_err(io_failure)?;
        let mut stream = response.bytes_stream();
        let mut written = offset;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| SourceFailure::new(reason::NETWORK))?;
            written = written.saturating_add(chunk.len() as u64);
            if written > size {
                return Err(SourceFailure::new(reason::SIZE_MISMATCH));
            }
            output.write_all(&chunk).map_err(io_failure)?;
            hasher.update(&chunk);
            job.done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        output.sync_all().map_err(io_failure)?;
        if written != size {
            // A short body is a dropped connection until proven otherwise;
            // the partial file resumes on the next attempt.
            return Err(SourceFailure::new(reason::NETWORK));
        }
        Ok(())
    }

    /// An `http` source: size from the response, reserve, stream, verify the
    /// declared digest, then place the file (or extract the archive).
    #[allow(clippy::too_many_arguments)]
    async fn fetch_http(
        &self,
        key: &str,
        id: &str,
        url: &str,
        sha256: &str,
        archive: Archive,
        partial: &Path,
        job: &Job,
    ) -> Result<(u64, u64), SourceFailure> {
        let tree = partial.join("tree");
        let name = match archive {
            Archive::None => file_name(url),
            Archive::Tar => "archive.tar".to_string(),
        };
        let payload = partial.join(&name);
        let part = part_path(&payload);
        let expected = Expected::Sha256(sha256.into());
        let origin = self.origin(url)?;
        if !payload.is_file() {
            let existing = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            let response = self
                .get(origin.clone(), None, existing)
                .send()
                .await
                .map_err(|_| SourceFailure::new(reason::NETWORK))?;
            check_status(response.status())?;
            let total = match response.status() {
                reqwest::StatusCode::PARTIAL_CONTENT => response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit('/').next())
                    .and_then(|v| v.parse::<u64>().ok()),
                _ => response.content_length(),
            }
            .ok_or(SourceFailure::new(reason::SIZE_UNKNOWN))?;
            job.total.store(total, Ordering::Relaxed);
            // An archive and its extracted tree coexist until the archive is
            // removed; uncompressed tar content never exceeds the archive.
            let reserved = match archive {
                Archive::None => total,
                Archive::Tar => total.saturating_mul(2),
            };
            // SPEC §7.3: reserved before any body byte is written.
            self.reserve(id, key, reserved)?;
            let mut hasher = Hasher::new(&expected, total);
            let offset = self.resume(&part, &mut hasher, total)?;
            job.done.store(offset, Ordering::Relaxed);
            // The same response continues: the body is read only now, after
            // the reservation (it was requested from `existing`, which
            // `resume` just hashed).
            let opened = (offset == existing).then_some(response);
            self.stream_into(opened, origin, None, &part, offset, total, &mut hasher, job)
                .await?;
            if !hasher.matches(&expected) {
                return Err(SourceFailure::new(reason::HASH_MISMATCH));
            }
            fs::rename(&part, &payload).map_err(io_failure)?;
        }
        match archive {
            Archive::None => {
                let bytes = fs::metadata(&payload).map_err(io_failure)?.len();
                fs::rename(&payload, tree.join(&name)).map_err(io_failure)?;
                Ok((bytes, 1))
            }
            Archive::Tar => {
                let limit = fs::metadata(&payload).map_err(io_failure)?.len();
                let _ = fs::remove_dir_all(&tree);
                let bytes = tar::extract(&payload, &tree, limit).map_err(|error| match error {
                    tar::TarError::Unsafe => SourceFailure::new(reason::UNSAFE_ARCHIVE),
                    tar::TarError::Io(_) => SourceFailure::new(reason::IO_ERROR),
                })?;
                fs::remove_file(&payload).map_err(io_failure)?;
                Ok((bytes, count_files(&tree)))
            }
        }
    }

    /// Put a verified tree in place: record `committing`, rename atomically,
    /// record `verified`, then release the reservation and the partial dir.
    fn commit(&self, key: &str, id: &str, tree: &Path, bytes: u64, files: u64) -> Result<(), SourceFailure> {
        let target = self.store.join(key);
        let parent = target.parent().ok_or(SourceFailure::new(reason::IO_ERROR))?;
        fs::create_dir_all(parent).map_err(io_failure)?;
        let marker_path = self.state_dir().join(format!("{id}.verified"));
        let marker = Marker {
            key: key.into(),
            state: "committing".into(),
            bytes,
            files,
        };
        self.write_json(&marker_path, &marker).map_err(io_failure)?;
        fs::rename(tree, &target).map_err(io_failure)?;
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(io_failure)?;
        self.write_json(
            &marker_path,
            &Marker {
                state: "verified".into(),
                ..marker
            },
        )
        .map_err(io_failure)?;
        let partial = self.partial_dir(id);
        let _ = fs::remove_dir_all(&partial);
        // The copy is now charged by its marker; the reservation is released.
        self.release(id).map_err(io_failure)
    }
}

/// Which pin a file is checked against, computed while it streams.
enum Hasher {
    Sha256(sha2::Sha256),
    GitBlob(sha1::Sha1),
}

impl Hasher {
    fn new(expected: &Expected, size: u64) -> Self {
        match expected {
            Expected::Sha256(_) => Self::Sha256(sha2::Sha256::new()),
            Expected::GitBlob(_) => {
                let mut hasher = sha1::Sha1::new();
                hasher.update(format!("blob {size}\0").as_bytes());
                Self::GitBlob(hasher)
            }
        }
    }
    /// A fresh hasher for the same pin (an origin ignored a range request).
    fn restart(&self, size: u64) -> Self {
        match self {
            Self::Sha256(_) => Self::Sha256(sha2::Sha256::new()),
            Self::GitBlob(_) => {
                let mut hasher = sha1::Sha1::new();
                hasher.update(format!("blob {size}\0").as_bytes());
                Self::GitBlob(hasher)
            }
        }
    }
    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha256(hasher) => hasher.update(bytes),
            Self::GitBlob(hasher) => hasher.update(bytes),
        }
    }
    fn matches(&self, expected: &Expected) -> bool {
        match (self, expected) {
            (Self::Sha256(hasher), Expected::Sha256(want)) => {
                hex::encode(hasher.clone().finalize()) == *want
            }
            (Self::GitBlob(hasher), Expected::GitBlob(want)) => {
                hex::encode(hasher.clone().finalize()) == *want
            }
            _ => false,
        }
    }
}

fn check_status(status: reqwest::StatusCode) -> Result<(), SourceFailure> {
    match status.as_u16() {
        200 | 206 => Ok(()),
        401 | 403 => Err(SourceFailure::new(reason::UNAUTHORIZED)),
        404 | 410 => Err(SourceFailure::new(reason::NOT_FOUND)),
        _ => Err(SourceFailure::new(reason::NETWORK)),
    }
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// A repository path mllm will write: relative, no `..`, no hidden
/// bookkeeping names, printable.
fn safe_repository_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != ".." && !segment.ends_with(".part"))
}

/// The file name an `http` payload is stored under: the URL's last path
/// segment when it is a plain name, otherwise `download`.
fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let last = path.rsplit('/').next().unwrap_or_default();
    if !last.is_empty()
        && last != "."
        && last != ".."
        && !last.starts_with('.')
        && !last.ends_with(".part")
        && last.len() <= 255
        && last
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        last.to_string()
    } else {
        "download".to_string()
    }
}

fn count_files(root: &Path) -> u64 {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(kind) if kind.is_file() => count += 1,
                _ => {}
            }
        }
    }
    count
}

/// Bytes of regular files under `root` (0 when absent).
fn dir_bytes(root: &Path) -> u64 {
    let mut total = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(metadata) if metadata.is_dir() => stack.push(entry.path()),
                Ok(metadata) if metadata.is_file() => total = total.saturating_add(metadata.len()),
                _ => {}
            }
        }
    }
    total
}

/// Free bytes available to this account on the filesystem holding `path`.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is NUL-terminated and `stat` is a valid out pointer.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

/// An exclusive advisory lock on `path`, or `None` when another holder has it.
fn lock_exclusive(path: &Path) -> io::Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: the descriptor is owned by `file` for the duration of the call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(Some(file))
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

/// What `prune` did with one materialized source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrunedSource {
    pub key: String,
    pub bytes: u64,
}

/// SPEC §6.3, ADR 0008: the outcome of `mllm prune sources`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    /// Unreferenced copies removed (or, without `apply`, that would be).
    pub removed: Vec<PrunedSource>,
    /// Copies a deployment still references.
    pub kept: Vec<PrunedSource>,
    /// Unreferenced copies left alone: a download holds their lock, or their
    /// removal could not be proven.
    pub skipped: Vec<PrunedSource>,
    pub applied: bool,
}

/// SPEC §6.3, ADR 0008: remove materialized sources no deployment references.
///
/// Deleting a deployment never deletes its source (the copy may serve another
/// deployment, or the next revision). This is the explicit, host-side reclaim:
/// only verified copies under `<store>/sources`, only keys absent from
/// `referenced`, never one whose download lock is held. Without `apply` it
/// only reports. A marker is removed only after its directory is verifiably
/// gone, so the store's charge never drops below what is on disk.
pub fn prune(
    model_store: &Path,
    referenced: &BTreeSet<String>,
    apply: bool,
) -> io::Result<PruneReport> {
    let state = model_store
        .join(mllm_config::model_source::SOURCES_DIR)
        .join(STATE_DIR);
    let mut report = PruneReport {
        applied: apply,
        ..Default::default()
    };
    let entries = match fs::read_dir(&state) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };
    let mut markers = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".verified") {
            let marker: Marker = match serde_json::from_slice(&fs::read(entry.path())?) {
                Ok(marker) => marker,
                Err(_) => continue,
            };
            markers.push((id.to_owned(), marker));
        }
    }
    markers.sort_by(|a, b| a.1.key.cmp(&b.1.key));
    for (id, marker) in markers {
        let entry = PrunedSource {
            key: marker.key.clone(),
            bytes: marker.bytes,
        };
        // Only mllm's own keys, and only inside `sources/`.
        let key_ok = marker.key.starts_with("sources/")
            && !marker.key.split('/').any(|s| s == ".." || s == "." || s.is_empty() || s == STATE_DIR)
            && SourceStore::id(&marker.key) == id;
        if !key_ok {
            report.skipped.push(entry);
            continue;
        }
        if referenced.contains(&marker.key) {
            report.kept.push(entry);
            continue;
        }
        if !apply {
            report.removed.push(entry);
            continue;
        }
        let Some(lock) = lock_exclusive(&state.join(format!("{id}.lock")))? else {
            report.skipped.push(entry);
            continue;
        };
        let target = model_store.join(&marker.key);
        let _ = fs::remove_dir_all(&target);
        if target.exists() {
            report.skipped.push(entry);
            continue;
        }
        fs::remove_file(state.join(format!("{id}.verified")))?;
        let _ = fs::remove_file(state.join(format!("{id}.reservation")));
        drop(lock);
        let _ = fs::remove_file(state.join(format!("{id}.lock")));
        report.removed.push(entry);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_paths_and_names_are_confined() {
        assert!(safe_repository_path("model-00001-of-00002.safetensors"));
        assert!(safe_repository_path("tokenizer/vocab.json"));
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a\\b", "x.part"] {
            assert!(!safe_repository_path(bad), "{bad}");
        }
        assert_eq!(file_name("https://h/p/model.gguf?x=1"), "model.gguf");
        assert_eq!(file_name("https://h/p/"), "download");
        assert_eq!(file_name("https://h/p/.hidden"), "download");
    }

    #[test]
    fn git_blob_ids_match_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        let expected = Expected::GitBlob("ce013625030ba8dba906f756967f9e9ca394464a".into());
        let mut hasher = Hasher::new(&expected, 6);
        hasher.update(b"hello\n");
        assert!(hasher.matches(&expected));
    }
}
