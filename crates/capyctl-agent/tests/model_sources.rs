//! ADR 0008: materializing declared model sources, against a local fake
//! Hugging Face hub and HTTPS origin (served as loopback HTTP through the
//! store's test seam). No test reaches a real network.
//!
//! These are CPU tests of the fetch, verification and accounting logic. They
//! are not qualification: a live Hugging Face download on a Spark is pending.

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use capyctl_agent::checkpoint::CheckpointVerifier;
use capyctl_agent::sources::{prune, reason, SourceStatus, SourceStore, FREE_SPACE_RESERVE};
use capyctl_config::effective::{DigestProvenance, ModelSourcePolicy, SourceSwitch};
use capyctl_config::model_source::{Archive, ModelSource};
use sha1::Digest as _;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const REV: &str = "0123456789abcdef0123456789abcdef01234567";
const TOKEN: &str = "hf_TOPSECRETtokenvalue0123456789";

#[derive(Clone)]
struct Payload {
    bytes: Vec<u8>,
    /// Serve this many bytes on the first full request, then drop the connection.
    cut_after_once: Option<usize>,
    /// Omit Content-Length.
    chunked: bool,
    /// Delay between 4 KiB chunks.
    delay: Option<Duration>,
}

/// A repository file: `(path, bytes, lfs)`.
type RepoFile = (String, Vec<u8>, bool);

#[derive(Default)]
struct Hub {
    /// `repo -> (sha, files)`.
    repos: BTreeMap<String, (String, Vec<RepoFile>)>,
    payloads: BTreeMap<String, Payload>,
    require_token: bool,
    /// Every request: `(path, range header, authorization present)`.
    log: Vec<(String, Option<String>, Option<String>)>,
    cut_done: BTreeSet<String>,
    /// Serve LFS content with one byte flipped.
    tamper_cdn: bool,
}

type Shared = Arc<Mutex<Hub>>;

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

fn git_blob(bytes: &[u8]) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn range_start(headers: &HeaderMap) -> Option<usize> {
    headers
        .get("range")?
        .to_str()
        .ok()?
        .strip_prefix("bytes=")?
        .strip_suffix('-')?
        .parse()
        .ok()
}

fn serve(hub: &Shared, key: &str, payload: Payload, headers: &HeaderMap) -> Response<Body> {
    let start = range_start(headers).unwrap_or(0).min(payload.bytes.len());
    let total = payload.bytes.len();
    let body_bytes = payload.bytes[start..].to_vec();
    let mut builder = Response::builder();
    if start > 0 {
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            "content-range",
            format!("bytes {start}-{}/{total}", total - 1),
        );
    }
    let cut = if start == 0 {
        let mut hub = hub.lock().unwrap();
        payload
            .cut_after_once
            .filter(|_| hub.cut_done.insert(key.to_string()))
    } else {
        None
    };
    if !payload.chunked {
        builder = builder.header("content-length", body_bytes.len());
    }
    let delay = payload.delay;
    let stream = async_stream(body_bytes, cut, delay);
    builder.body(Body::from_stream(stream)).unwrap()
}

fn async_stream(
    bytes: Vec<u8>,
    cut: Option<usize>,
    delay: Option<Duration>,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    let limit = cut.unwrap_or(bytes.len());
    let chunks: Vec<Vec<u8>> = bytes[..limit].chunks(4096).map(<[u8]>::to_vec).collect();
    let fail = cut.is_some();
    futures::stream::unfold(
        (chunks.into_iter(), fail),
        move |(mut chunks, fail)| async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            match chunks.next() {
                Some(chunk) => Some((Ok(chunk), (chunks, fail))),
                None if fail => Some((
                    Err(std::io::Error::other("connection cut")),
                    (chunks, false),
                )),
                None => None,
            }
        },
    )
}

async fn handle(hub: Shared, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_string();
    let headers = request.headers().clone();
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    {
        let mut locked = hub.lock().unwrap();
        locked.log.push((
            path.clone(),
            headers
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            auth.clone(),
        ));
    }
    let not_found = || Response::builder().status(404).body(Body::empty()).unwrap();
    if let Some(payload) = path
        .strip_prefix("/files/")
        .and_then(|name| hub.lock().unwrap().payloads.get(name).cloned())
    {
        return serve(&hub, &path, payload, &headers);
    }
    let (require_token, repos, tamper) = {
        let locked = hub.lock().unwrap();
        (
            locked.require_token,
            locked.repos.clone(),
            locked.tamper_cdn,
        )
    };
    if require_token
        && auth.as_deref() != Some(&format!("Bearer {TOKEN}"))
        && !path.starts_with("/cdn/")
    {
        return Response::builder().status(401).body(Body::empty()).unwrap();
    }
    if let Some(rest) = path.strip_prefix("/api/models/") {
        let Some((repo, rev)) = rest.split_once("/revision/") else {
            return not_found();
        };
        let Some((sha, files)) = repos.get(repo) else {
            return not_found();
        };
        if rev != sha {
            return not_found();
        }
        let siblings: Vec<_> = files
            .iter()
            .map(|(name, bytes, lfs)| {
                if *lfs {
                    serde_json::json!({"rfilename": name, "size": bytes.len(), "blobId": "f".repeat(40),
                        "lfs": {"sha256": sha256_hex(bytes), "size": bytes.len(), "pointerSize": 134}})
                } else {
                    serde_json::json!({"rfilename": name, "size": bytes.len(), "blobId": git_blob(bytes)})
                }
            })
            .collect();
        let body = serde_json::json!({"id": repo, "sha": sha, "siblings": siblings});
        return Response::new(Body::from(body.to_string()));
    }
    if let Some(rest) = path.strip_prefix("/cdn/") {
        for (_, files) in repos.values() {
            for (_, bytes, _) in files {
                if sha256_hex(bytes) == rest {
                    let mut bytes = bytes.clone();
                    if tamper {
                        bytes[0] ^= 0xff;
                    }
                    let payload = Payload {
                        bytes,
                        cut_after_once: None,
                        chunked: false,
                        delay: None,
                    };
                    return serve(&hub, &path, payload, &headers);
                }
            }
        }
        return not_found();
    }
    // `/<owner>/<name>/resolve/<rev>/<path>`
    let Some((repo, tail)) = path.trim_start_matches('/').split_once("/resolve/") else {
        return not_found();
    };
    let Some((rev, file)) = tail.split_once('/') else {
        return not_found();
    };
    let Some((sha, files)) = repos.get(repo) else {
        return not_found();
    };
    if rev != sha {
        return not_found();
    }
    let Some((_, bytes, lfs)) = files.iter().find(|(name, _, _)| name == file) else {
        return not_found();
    };
    if *lfs {
        // LFS content lives on a CDN host; the hub redirects there.
        return Response::builder()
            .status(302)
            .header("location", format!("/cdn/{}", sha256_hex(bytes)))
            .body(Body::empty())
            .unwrap();
    }
    let payload = Payload {
        bytes: bytes.clone(),
        cut_after_once: None,
        chunked: false,
        delay: None,
    };
    serve(&hub, &path, payload, &headers)
}

async fn start(hub: Shared) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new().fallback(move |request: Request<Body>| {
        let hub = hub.clone();
        async move { handle(hub, request).await }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

fn policy(max_bytes: i64) -> ModelSourcePolicy {
    ModelSourcePolicy {
        huggingface: SourceSwitch::Allowed,
        http: SourceSwitch::Allowed,
        max_bytes: Some(max_bytes),
        allowed_hosts: vec![],
        huggingface_endpoint: Some("https://hub.example.test".into()),
        path: None,
        huggingface_token_file: None,
        plain_http: SourceSwitch::Denied,
    }
}

fn hf(files: Vec<&str>, token: bool) -> ModelSource {
    ModelSource::HuggingFace {
        repo: "org/model".into(),
        revision: REV.into(),
        files: files.into_iter().map(Into::into).collect(),
        token_ref: token.then(|| "secret://hf-token".to_string()),
    }
}

fn http(name: &str, bytes: &[u8], archive: Archive) -> ModelSource {
    ModelSource::Http {
        url: Some(format!("https://weights.example.test/files/{name}")),
        url_ref: None,
        sha256: sha256_hex(bytes),
        archive,
    }
}

fn weights() -> Vec<RepoFile> {
    vec![
        (
            "config.json".into(),
            br#"{"architectures":["Toy"]}"#.to_vec(),
            false,
        ),
        ("tokenizer/vocab.json".into(), b"{\"a\":1}".to_vec(), false),
        (
            "model.safetensors".into(),
            (0..200_000u32).map(|i| (i % 251) as u8).collect(),
            true,
        ),
    ]
}

struct Fixture {
    _root: tempfile::TempDir,
    store: PathBuf,
    secrets: PathBuf,
    hub: Shared,
    origin: String,
}

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("models");
    let secrets = root.path().join("secrets");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::create_dir_all(&secrets).unwrap();
    let token = secrets.join("hf-token");
    std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let hub: Shared = Arc::default();
    hub.lock()
        .unwrap()
        .repos
        .insert("org/model".into(), (REV.into(), weights()));
    let origin = start(hub.clone()).await;
    Fixture {
        store,
        secrets,
        hub,
        origin,
        _root: root,
    }
}

impl Fixture {
    fn source_store(&self, max_bytes: i64) -> Arc<SourceStore> {
        self.source_store_with(policy(max_bytes), &[])
    }

    /// A store under `policy` whose default-token variables are exactly `env`
    /// (never the test process's own).
    fn source_store_with(
        &self,
        policy: ModelSourcePolicy,
        env: &[(&'static str, &'static str)],
    ) -> Arc<SourceStore> {
        let store = SourceStore::with_loopback_origin(
            &self.store,
            policy,
            Some(self.secrets.clone()),
            &self.origin,
        );
        let env = env.to_vec();
        store.set_environment(Arc::new(move |key| {
            env.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }));
        store
    }
    fn requests(&self, needle: &str) -> Vec<(String, Option<String>, Option<String>)> {
        self.hub
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(path, _, _)| path.contains(needle))
            .cloned()
            .collect()
    }
    fn state_files(&self, suffix: &str) -> Vec<PathBuf> {
        let state = self.store.join("sources/.capyctl");
        std::fs::read_dir(state)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.to_string_lossy().ends_with(suffix))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn partial_empty(&self) -> bool {
        std::fs::read_dir(self.store.join("sources/.capyctl/partial"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    }
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
}

// T14 (ADR 0008): a pinned Hugging Face source materializes into its store
// directory with every file verified, then is reused without a second fetch.
#[tokio::test]
async fn huggingface_source_materializes_verified_into_the_store() {
    let f = fixture().await;
    let store = f.source_store(1 << 30);
    let source = hf(vec![], false);
    let bytes = store.materialize(&source).await.expect("materialized");
    let expected: u64 = weights().iter().map(|(_, b, _)| b.len() as u64).sum();
    assert_eq!(bytes, expected);
    let dir = f
        .store
        .join(format!("sources/huggingface/org--model@{REV}"));
    for (name, contents, _) in weights() {
        assert_eq!(std::fs::read(dir.join(&name)).unwrap(), contents, "{name}");
    }
    assert_eq!(store.status(&source), SourceStatus::Verified { bytes });
    // The reservation became the copy's charge; nothing temporary remains.
    assert!(f.state_files(".reservation").is_empty());
    assert!(f.partial_empty());
    // A second request (another deployment, the same pin) fetches nothing.
    let listings = f.requests("/api/models/").len();
    assert_eq!(store.request(&source), SourceStatus::Verified { bytes });
    assert_eq!(f.requests("/api/models/").len(), listings);
    // The LFS weight came through the hub's redirect to its CDN.
    assert_eq!(f.requests("/cdn/").len(), 1);

    // Allow patterns narrow the files, into a directory of their own.
    let narrowed = hf(vec!["*.json"], false);
    store.materialize(&narrowed).await.unwrap();
    let narrow_dir = f.store.join(narrowed.store_key().unwrap());
    assert!(narrow_dir.join("config.json").is_file());
    assert!(narrow_dir.join("tokenizer/vocab.json").is_file());
    assert!(!narrow_dir.join("model.safetensors").exists());
}

// T14 (ADR 0008): an http payload with the wrong digest is refused, its
// temporary files removed, and its reservation released on that evidence.
#[tokio::test]
async fn hash_mismatch_is_refused_and_temp_removed() {
    let f = fixture().await;
    let bytes = vec![5_u8; 50_000];
    f.hub.lock().unwrap().payloads.insert(
        "w.gguf".into(),
        Payload {
            bytes: bytes.clone(),
            cut_after_once: None,
            chunked: false,
            delay: None,
        },
    );
    let store = f.source_store(1 << 30);
    let wrong = ModelSource::Http {
        url: Some("https://weights.example.test/files/w.gguf".into()),
        url_ref: None,
        sha256: sha256_hex(b"something else"),
        archive: Archive::None,
    };
    let failure = store.materialize(&wrong).await.unwrap_err();
    assert_eq!(failure.reason, reason::HASH_MISMATCH);
    assert!(!failure.reservation_retained);
    assert!(f.partial_empty(), "temporary files removed");
    assert!(
        f.state_files(".reservation").is_empty(),
        "reservation released"
    );
    assert!(!f.store.join(wrong.store_key().unwrap()).exists());

    // A hub whose CDN serves bytes that do not match the LFS pointer's
    // SHA-256 is refused the same way.
    f.hub.lock().unwrap().tamper_cdn = true;
    let failure = store.materialize(&hf(vec![], false)).await.unwrap_err();
    assert_eq!(failure.reason, reason::HASH_MISMATCH);
    assert!(!failure.reservation_retained);
    assert!(f.partial_empty(), "temporary files removed");
    assert!(
        f.state_files(".reservation").is_empty(),
        "reservation released"
    );
    assert!(!f
        .store
        .join(hf(vec![], false).store_key().unwrap())
        .exists());
}

// T14 (ADR 0008, SPEC §7.3): a source over the store's max_bytes is refused
// before any file is downloaded, with nothing reserved.
#[tokio::test]
async fn size_over_limit_is_refused_before_download() {
    let f = fixture().await;
    let store = f.source_store(1_000);
    let failure = store.materialize(&hf(vec![], false)).await.unwrap_err();
    assert_eq!(failure.reason, reason::TOO_LARGE);
    assert_eq!(f.requests("/api/models/").len(), 1, "the listing sized it");
    assert!(f.requests("/resolve/").is_empty(), "no file was fetched");
    assert!(f.requests("/cdn/").is_empty(), "no file was fetched");
    assert!(f.state_files(".reservation").is_empty());

    // The ceiling counts copies already in the store (the store is a charged
    // resource owner): two sources that fit alone do not fit together.
    let first = vec![1_u8; 6_000];
    let second = vec![2_u8; 6_000];
    for (name, bytes) in [("a.bin", &first), ("b.bin", &second)] {
        f.hub.lock().unwrap().payloads.insert(
            name.into(),
            Payload {
                bytes: bytes.clone(),
                cut_after_once: None,
                chunked: false,
                delay: None,
            },
        );
    }
    let store = f.source_store(10_000);
    store
        .materialize(&http("a.bin", &first, Archive::None))
        .await
        .unwrap();
    let failure = store
        .materialize(&http("b.bin", &second, Archive::None))
        .await
        .unwrap_err();
    assert_eq!(failure.reason, reason::TOO_LARGE);

    // An origin that does not state the size is refused rather than guessed.
    let third = vec![3_u8; 6_000];
    f.hub.lock().unwrap().payloads.insert(
        "chunked.bin".into(),
        Payload {
            bytes: third.clone(),
            cut_after_once: None,
            chunked: true,
            delay: None,
        },
    );
    let store = f.source_store(1 << 30);
    let failure = store
        .materialize(&http("chunked.bin", &third, Archive::None))
        .await
        .unwrap_err();
    assert_eq!(failure.reason, reason::SIZE_UNKNOWN);
}

// T14 (ADR 0008, owner decision 2026-09-25): a host that turns remote
// sources off keeps them off, and a denied request reaches no origin.
#[tokio::test]
async fn host_policy_denies_remote_sources_it_turned_off() {
    let f = fixture().await;
    let mut off = policy(1 << 30);
    off.huggingface = SourceSwitch::Denied;
    off.http = SourceSwitch::Denied;
    let store =
        SourceStore::with_loopback_origin(&f.store, off, Some(f.secrets.clone()), &f.origin);
    for source in [hf(vec![], false), http("x.bin", b"x", Archive::None)] {
        match store.request(&source) {
            SourceStatus::Failed(failure) => assert_eq!(failure.reason, reason::DENIED),
            other => panic!("{other:?}"),
        }
    }
    let mut only_http = policy(1 << 30);
    only_http.huggingface = SourceSwitch::Denied;
    let store = SourceStore::with_loopback_origin(&f.store, only_http, None, &f.origin);
    assert!(matches!(
        store.request(&hf(vec![], false)),
        SourceStatus::Failed(ref failure) if failure.reason == reason::DENIED
    ));
    assert!(
        f.hub.lock().unwrap().log.is_empty(),
        "no origin was contacted"
    );
}

// T14 (SPEC §7.3, owner decision 2026-09-25): a download that would leave
// less than the free-space reserve on the filesystem is refused
// `insufficient_space` before any byte is written; the default policy (500
// GiB ceiling) materializes one that fits.
#[tokio::test]
async fn a_download_that_would_fill_the_disk_is_refused() {
    let f = fixture().await;
    let payload = vec![7_u8; 4_000];
    f.hub.lock().unwrap().payloads.insert(
        "w.bin".into(),
        Payload {
            bytes: payload.clone(),
            cut_after_once: None,
            chunked: false,
            delay: None,
        },
    );
    let source = http("w.bin", &payload, Archive::None);
    let store =
        SourceStore::with_loopback_origin(&f.store, ModelSourcePolicy::default(), None, &f.origin);
    let needed = payload.len() as u64 + FREE_SPACE_RESERVE;
    store.set_free_bytes_for_test(Some(needed - 1));
    let failure = store.materialize(&source).await.unwrap_err();
    assert_eq!(failure.reason, reason::INSUFFICIENT_SPACE);
    assert!(f.state_files(".reservation").is_empty());
    assert!(f.requests("/files/").len() <= 1, "sized, never streamed");
    store.set_free_bytes_for_test(Some(needed));
    assert_eq!(
        store.materialize(&source).await.unwrap(),
        payload.len() as u64
    );
}

// T14 (ADR 0008): concurrent requests for one source share one download.
#[tokio::test]
async fn concurrent_requests_share_one_download() {
    let f = fixture().await;
    let bytes: Vec<u8> = (0..120_000u32).map(|i| (i % 13) as u8).collect();
    f.hub.lock().unwrap().payloads.insert(
        "slow.bin".into(),
        Payload {
            bytes: bytes.clone(),
            cut_after_once: None,
            chunked: false,
            delay: Some(Duration::from_millis(2)),
        },
    );
    let store = f.source_store(1 << 30);
    let source = http("slow.bin", &bytes, Archive::None);
    let status = store.request(&source);
    assert!(
        matches!(status, SourceStatus::Downloading { .. }),
        "{status:?}"
    );
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            let source = source.clone();
            tokio::spawn(async move { store.materialize(&source).await })
        })
        .collect();
    for waiter in waiters {
        assert_eq!(waiter.await.unwrap().unwrap(), bytes.len() as u64);
    }
    assert_eq!(f.requests("/files/slow.bin").len(), 1, "one download");
    let dir = f.store.join(source.store_key().unwrap());
    assert_eq!(std::fs::read(dir.join("slow.bin")).unwrap(), bytes);
}

// T14 (ADR 0008): an interrupted download keeps its partial file and its
// reservation; a new store (an agent restart) resumes with a range request.
#[tokio::test]
async fn download_resumes_after_agent_restart() {
    let f = fixture().await;
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 256) as u8).collect();
    f.hub.lock().unwrap().payloads.insert(
        "big.bin".into(),
        Payload {
            bytes: bytes.clone(),
            cut_after_once: Some(40_960),
            chunked: false,
            // Paced, so the response head is on the wire before the cut.
            delay: Some(Duration::from_millis(1)),
        },
    );
    let source = http("big.bin", &bytes, Archive::None);
    {
        let store = f.source_store(1 << 30);
        let failure = store.materialize(&source).await.unwrap_err();
        assert_eq!(failure.reason, reason::NETWORK);
        assert!(failure.reservation_retained, "partial bytes stay charged");
        assert_eq!(f.state_files(".reservation").len(), 1);
        assert!(!f.partial_empty(), "the partial file is kept for a resume");
    }
    // Restart: a fresh store over the same directory.
    let store = f.source_store(1 << 30);
    assert_eq!(
        store.materialize(&source).await.unwrap(),
        bytes.len() as u64
    );
    let requests = f.requests("/files/big.bin");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].1.as_deref(), Some("bytes=40960-"), "resumed");
    let dir = f.store.join(source.store_key().unwrap());
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), bytes);
    assert!(f.state_files(".reservation").is_empty());
}

// T14 / T37 (ADR 0008, SPEC §13.3): the token is sent only as an
// Authorization header and appears in no file, status, failure or log line.
#[tokio::test]
async fn secret_token_is_never_logged_or_persisted() {
    let f = fixture().await;
    f.hub.lock().unwrap().require_token = true;
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let store = f.source_store(1 << 30);
    let captured = lines.clone();
    store.set_log(Arc::new(move |line| {
        captured.lock().unwrap().push(line.to_string())
    }));

    // Without the token the hub refuses; with a reference it is used.
    let failure = store.materialize(&hf(vec![], false)).await.unwrap_err();
    assert_eq!(failure.reason, reason::UNAUTHORIZED);
    let source = hf(vec![], true);
    let bytes = store.materialize(&source).await.expect("authorized");
    let used = f
        .requests("/api/models/")
        .into_iter()
        .filter(|(_, _, auth)| auth.as_deref() == Some(&format!("Bearer {TOKEN}")))
        .count();
    assert_eq!(used, 1, "the listing carried the token");
    // The CDN redirect target is on the same loopback origin here, so the
    // header may follow; on a real CDN host reqwest drops it.

    let mut files = Vec::new();
    walk(&f.store, &mut files);
    for file in files {
        let contents = std::fs::read(&file).unwrap();
        assert!(
            !contents.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()),
            "{} holds the token",
            file.display()
        );
    }
    let status = format!(
        "{:?} {:?}",
        store.status(&source),
        SourceStatus::Verified { bytes }
    );
    assert!(!status.contains(TOKEN));
    let log = lines.lock().unwrap().join("\n");
    assert!(!log.is_empty(), "the store logged its outcomes");
    assert!(!log.contains(TOKEN), "{log}");
    assert!(!format!("{failure:?}").contains(TOKEN));

    // A secret readable by others is refused, not used.
    let token = f.secrets.join("hf-token");
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let other = ModelSource::HuggingFace {
        repo: "org/model".into(),
        revision: REV.into(),
        files: vec!["config.json".into()],
        token_ref: Some("secret://hf-token".into()),
    };
    let failure = store.materialize(&other).await.unwrap_err();
    assert_eq!(failure.reason, reason::SECRET_UNAVAILABLE);
    assert!(!lines.lock().unwrap().join("\n").contains(TOKEN));
}

/// The query of a presigned URL: the credential it carries.
const SIGNATURE: &str = "X-Amz-Signature=0b5e55ed5ec2e7a1&X-Amz-Expires=300";

fn presigned(name: &str) -> String {
    format!("https://bucket.example.test/files/{name}?{SIGNATURE}")
}

impl Fixture {
    fn payload(&self, name: &str, bytes: &[u8]) {
        self.hub.lock().unwrap().payloads.insert(
            name.into(),
            Payload {
                bytes: bytes.to_vec(),
                cut_after_once: None,
                chunked: false,
                delay: None,
            },
        );
    }

    fn secret(&self, name: &str, value: &str, mode: u32) {
        let path = self.secrets.join(name);
        std::fs::write(&path, format!("{value}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

fn by_ref(reference: &str, bytes: &[u8]) -> ModelSource {
    serde_json::from_value(serde_json::json!({"http": {
        "url_ref": reference, "sha256": sha256_hex(bytes)}}))
    .unwrap()
}

fn capture(store: &SourceStore) -> Arc<Mutex<Vec<String>>> {
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let captured = lines.clone();
    store.set_log(Arc::new(move |line| {
        captured.lock().unwrap().push(line.to_string())
    }));
    lines
}

// T14 T37 (ADR 0008 amendment 2026-10-08, SPEC §13.3): an http source whose
// URL is a host secret materializes verified against its digest. The URL (a
// presigned one, whose query is its credential) is read at the moment of the
// fetch and appears in no stored file, status, failure or log line; the
// host checks its origin against its own policy before any request.
#[tokio::test]
async fn a_secret_url_is_fetched_verified_and_never_persisted_or_logged() {
    let f = fixture().await;
    let bytes = vec![7_u8; 40_000];
    f.payload("w.gguf", &bytes);
    f.payload("x.gguf", b"other");
    f.secret("weights-url", &presigned("w.gguf"), 0o600);
    let store = f.source_store(1 << 30);
    let lines = capture(&store);
    let source = by_ref("secret://weights-url", &bytes);
    let verified = store.materialize(&source).await.expect("verified");
    assert_eq!(verified, bytes.len() as u64);
    let directory = f.store.join(source.store_key().unwrap());
    assert_eq!(std::fs::read(directory.join("w.gguf")).unwrap(), bytes);
    assert_eq!(f.requests("/files/w.gguf").len(), 1);
    let mut failures = Vec::new();
    // The same secret behind a different pin: the digest still decides.
    let wrong = by_ref("secret://weights-url", b"something else");
    let failure = store.materialize(&wrong).await.unwrap_err();
    assert_eq!(failure.reason, reason::HASH_MISMATCH);
    failures.push(failure);
    // A missing secret, one others may read, and one that holds no URL.
    f.secret("loose-url", &presigned("x.gguf"), 0o644);
    f.secret("not-a-url", "ftp://bucket.example.test/x.gguf", 0o600);
    for reference in [
        "secret://absent",
        "secret://loose-url",
        "secret://not-a-url",
    ] {
        let failure = store
            .materialize(&by_ref(reference, b"other"))
            .await
            .unwrap_err();
        assert_eq!(failure.reason, reason::SECRET_UNAVAILABLE, "{reference}");
        failures.push(failure);
    }
    // A host that lists origins checks the resolved URL's before fetching.
    f.secret("x-url", &presigned("x.gguf"), 0o600);
    let mut listed = policy(1 << 30);
    listed.allowed_hosts = vec!["weights.example.test".into()];
    let listing = f.source_store_with(listed, &[]);
    let listing_lines = capture(&listing);
    let failure = listing
        .materialize(&by_ref("secret://x-url", b"other"))
        .await
        .unwrap_err();
    assert_eq!(failure.reason, reason::DENIED);
    failures.push(failure);
    assert!(
        f.requests("/files/x.gguf").is_empty(),
        "nothing was fetched"
    );

    let credential = "0b5e55ed5ec2e7a1";
    let mut files = Vec::new();
    walk(&f.store, &mut files);
    assert!(!files.is_empty());
    for file in files {
        let contents = std::fs::read(&file).unwrap();
        assert!(
            !contents
                .windows(credential.len())
                .any(|w| w == credential.as_bytes())
                && !contents.windows(9).any(|w| w == b"X-Amz-Sig"),
            "{} holds the URL",
            file.display()
        );
    }
    let status = serde_json::to_string(&store.status(&source)).unwrap();
    let reported = format!("{status} {failures:?} {:?}", store.status(&wrong));
    assert!(!reported.contains(credential), "{reported}");
    assert!(!reported.contains("bucket.example.test"), "{reported}");
    let mut log = lines.lock().unwrap().join("\n");
    log.push_str(&listing_lines.lock().unwrap().join("\n"));
    assert!(!log.is_empty(), "the store logged its outcomes");
    assert!(!log.contains(credential), "{log}");
    assert!(!log.contains("bucket.example.test"), "{log}");
}

// T14 T37 (ADR 0008 amendment 2026-10-08): a plain http:// source is fetched
// only by a host whose policy approves plain HTTP, and is still verified
// against its digest; the same holds for a plain URL named by a secret.
#[tokio::test]
async fn plain_http_sources_are_fetched_only_where_the_host_approves() {
    let f = fixture().await;
    let bytes = vec![9_u8; 30_000];
    f.payload("p.bin", &bytes);
    let written: ModelSource = serde_json::from_value(serde_json::json!({"http": {
        "url": "http://mirror.lan:8080/files/p.bin", "sha256": sha256_hex(&bytes)}}))
    .unwrap();
    f.secret("plain-url", "http://mirror.lan:8080/files/p.bin", 0o600);
    let named = by_ref("secret://plain-url", &bytes);
    let refusing = f.source_store(1 << 30);
    for source in [&written, &named] {
        let failure = refusing.materialize(source).await.unwrap_err();
        assert_eq!(failure.reason, reason::DENIED);
    }
    assert!(f.requests("/files/p.bin").is_empty(), "nothing was fetched");
    let mut approved = policy(1 << 30);
    approved.plain_http = SourceSwitch::Allowed;
    let approving = f.source_store_with(approved, &[]);
    assert_eq!(
        approving.materialize(&written).await.unwrap(),
        bytes.len() as u64
    );
    assert_eq!(
        approving.materialize(&named).await.unwrap(),
        bytes.len() as u64
    );
    let wrong: ModelSource = serde_json::from_value(serde_json::json!({"http": {
        "url": "http://mirror.lan:8080/files/p.bin", "sha256": sha256_hex(b"tampered")}}))
    .unwrap();
    assert_eq!(
        approving.materialize(&wrong).await.unwrap_err().reason,
        reason::HASH_MISMATCH
    );
}

// T14 T37 (owner rule 2026-09-25: a secret is a variable or a protected file,
// never a CLI flag): a source that names no token uses the host's default one,
// `CAPYCTL_HF_TOKEN` over `HF_TOKEN` over `model_sources.huggingface_token_file`;
// a token file others can read is refused, and with none the fetch carries
// no token.
#[tokio::test]
async fn a_default_token_comes_from_the_environment_or_the_token_file() {
    let token_file = |f: &Fixture| {
        let mut policy = policy(1 << 30);
        policy.huggingface_token_file = Some(f.secrets.join("hf-token"));
        policy
    };
    type Case = (
        &'static str,
        &'static [(&'static str, &'static str)],
        bool,
        Result<(), &'static str>,
    );
    let cases: Vec<Case> = vec![
        (
            "capyctl variable",
            &[("CAPYCTL_HF_TOKEN", TOKEN), ("HF_TOKEN", "wrong")],
            true,
            Ok(()),
        ),
        ("tools variable", &[("HF_TOKEN", TOKEN)], false, Ok(())),
        (
            "variable over file",
            &[("CAPYCTL_HF_TOKEN", "wrong")],
            true,
            Err(reason::UNAUTHORIZED),
        ),
        ("file", &[], true, Ok(())),
        ("nothing", &[], false, Err(reason::UNAUTHORIZED)),
    ];
    for (name, env, file, expected) in cases {
        let f = fixture().await;
        f.hub.lock().unwrap().require_token = true;
        let policy = if file {
            token_file(&f)
        } else {
            policy(1 << 30)
        };
        let store = f.source_store_with(policy, env);
        let outcome = store
            .materialize(&hf(vec![], false))
            .await
            .map(|_| ())
            .map_err(|failure| failure.reason);
        assert_eq!(outcome, expected, "{name}");
    }
    // A token file others can read is refused, not used.
    let f = fixture().await;
    f.hub.lock().unwrap().require_token = true;
    std::fs::set_permissions(
        f.secrets.join("hf-token"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let failure = f
        .source_store_with(token_file(&f), &[])
        .materialize(&hf(vec![], false))
        .await
        .unwrap_err();
    assert_eq!(failure.reason, reason::SECRET_UNAVAILABLE);
}

// T14 (ADR 0008): a tar payload is verified as a whole, then extracted.
#[tokio::test]
async fn tar_archive_is_verified_then_extracted() {
    let f = fixture().await;
    let archive = ustar(&[("config.json", b"{}"), ("weights/w.bin", &[9_u8; 3000])]);
    f.hub.lock().unwrap().payloads.insert(
        "model.tar".into(),
        Payload {
            bytes: archive.clone(),
            cut_after_once: None,
            chunked: false,
            delay: None,
        },
    );
    let store = f.source_store(1 << 30);
    let source = http("model.tar", &archive, Archive::Tar);
    assert_eq!(store.materialize(&source).await.unwrap(), 3002);
    let dir = f.store.join(source.store_key().unwrap());
    assert_eq!(
        std::fs::read(dir.join("weights/w.bin")).unwrap(),
        vec![9_u8; 3000]
    );
    assert!(!dir.join("archive.tar").exists());
}

// SPEC §6.3 (ADR 0008): prune removes only unreferenced verified copies, only
// when asked, and never one whose download lock is held.
#[tokio::test]
async fn prune_removes_only_unreferenced_sources() {
    let f = fixture().await;
    let a = vec![1_u8; 1000];
    let b = vec![2_u8; 1000];
    for (name, bytes) in [("a.bin", &a), ("b.bin", &b)] {
        f.hub.lock().unwrap().payloads.insert(
            name.into(),
            Payload {
                bytes: bytes.clone(),
                cut_after_once: None,
                chunked: false,
                delay: None,
            },
        );
    }
    let store = f.source_store(1 << 30);
    let kept = http("a.bin", &a, Archive::None);
    let unreferenced = http("b.bin", &b, Archive::None);
    store.materialize(&kept).await.unwrap();
    store.materialize(&unreferenced).await.unwrap();
    let referenced = BTreeSet::from([kept.store_key().unwrap()]);

    let dry = prune(&f.store, &referenced, false).unwrap();
    assert_eq!(dry.removed.len(), 1);
    assert!(
        f.store.join(unreferenced.store_key().unwrap()).is_dir(),
        "dry run"
    );

    let applied = prune(&f.store, &referenced, true).unwrap();
    assert_eq!(applied.removed[0].key, unreferenced.store_key().unwrap());
    assert_eq!(applied.kept[0].key, kept.store_key().unwrap());
    assert!(!f.store.join(unreferenced.store_key().unwrap()).exists());
    assert!(f.store.join(kept.store_key().unwrap()).is_dir());
    // Its fetched manifest goes with it; the kept copy keeps its own.
    assert_eq!(f.state_files(".manifest").len(), 1);
    assert_eq!(store.status(&unreferenced), SourceStatus::Pending);
    // Something outside `sources/` is never touched.
    std::fs::create_dir_all(f.store.join("toy")).unwrap();
    prune(&f.store, &BTreeSet::new(), true).unwrap();
    assert!(f.store.join("toy").is_dir());
}

/// A copy of `from` at `to`, made by reading every file: a checkpoint no
/// download describes, which a verifier must measure in full.
fn copy_tree(from: &Path, to: &Path) {
    let mut files = Vec::new();
    walk(from, &mut files);
    for file in files {
        let target = to.join(file.strip_prefix(from).unwrap());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(&file, &target).unwrap();
    }
}

// T14 T34 (ADR 0014 §7, amendment of 2026-10-08): a checkpoint CapyCTL
// downloaded and verified file by file gets its digest from those verified
// hashes: the first placement reads no weight file again (only the small
// files every launch rehashes), and the digest is exactly the one a full read
// of the same bytes gives. A hidden tool file in the repository is left out,
// as a walk leaves it out.
#[tokio::test]
async fn a_fetched_checkpoint_digest_needs_no_second_read() {
    let f = fixture().await;
    f.hub
        .lock()
        .unwrap()
        .repos
        .get_mut("org/model")
        .unwrap()
        .1
        .push((
            ".gitattributes".into(),
            b"*.safetensors lfs".to_vec(),
            false,
        ));
    let store = f.source_store(1 << 30);
    let source = hf(vec![], false);
    store.materialize(&source).await.unwrap();
    let dir = f.store.join(source.store_key().unwrap());
    assert_eq!(f.state_files(".manifest").len(), 1);

    // A fresh verifier, as an agent after a restart; the 200 kB weight file
    // counts as large.
    let limit = 1024;
    let small: u64 = weights()
        .iter()
        .map(|(_, bytes, _)| bytes.len() as u64)
        .filter(|size| *size <= limit)
        .sum();
    let verifier = CheckpointVerifier::in_memory().with_small_file_limit(limit);
    let fetched = verifier.measure(&f.store, &dir).unwrap();
    assert_eq!(fetched.provenance, DigestProvenance::Fetched);
    assert!(!fetched.full_rehash);
    assert_eq!(verifier.bytes_hashed(), small, "no weight file was read");

    // The same canonical manifest a full read gives.
    let copy = f.store.join("copy");
    copy_tree(&dir, &copy);
    let full = CheckpointVerifier::in_memory()
        .measure(&f.store, &copy)
        .unwrap();
    assert_eq!(full.provenance, DigestProvenance::Measured);
    assert!(full.full_rehash);
    assert_eq!(fetched.manifest, full.manifest);

    // Later launches reuse it from the stat cache.
    let again = verifier
        .verify(&f.store, &dir, &full.manifest.digest)
        .unwrap();
    assert_eq!(again.provenance, DigestProvenance::Fetched);
    assert_eq!(verifier.bytes_hashed(), 2 * small);
}

// T14 T34 (ADR 0014 §7, amendment of 2026-10-08): a downloaded file changed
// after its verification is never described by the download: the checkpoint
// is measured in full and its digest is the changed bytes'. An http payload
// gets its fetched manifest the same way; a tar archive's extracted files,
// verified only as one archive, are measured.
#[tokio::test]
async fn a_changed_or_unpinned_download_is_measured() {
    let f = fixture().await;
    let store = f.source_store(1 << 30);
    let source = hf(vec![], false);
    store.materialize(&source).await.unwrap();
    let dir = f.store.join(source.store_key().unwrap());
    let weight = dir.join("model.safetensors");
    let mut bytes = std::fs::read(&weight).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&weight, &bytes).unwrap();
    let measured = CheckpointVerifier::in_memory()
        .measure(&f.store, &dir)
        .unwrap();
    assert_eq!(measured.provenance, DigestProvenance::Measured);
    assert!(measured.full_rehash);

    let payload = vec![7_u8; 3000];
    let archive = ustar(&[("weights.bin", &payload)]);
    for (name, bytes) in [("w.bin", &payload), ("w.tar", &archive)] {
        f.hub.lock().unwrap().payloads.insert(
            name.into(),
            Payload {
                bytes: bytes.clone(),
                cut_after_once: None,
                chunked: false,
                delay: None,
            },
        );
    }
    let plain = http("w.bin", &payload, Archive::None);
    store.materialize(&plain).await.unwrap();
    let fetched = CheckpointVerifier::in_memory()
        .measure(&f.store, &f.store.join(plain.store_key().unwrap()))
        .unwrap();
    assert_eq!(fetched.provenance, DigestProvenance::Fetched);
    let tar = http("w.tar", &archive, Archive::Tar);
    store.materialize(&tar).await.unwrap();
    let extracted = CheckpointVerifier::in_memory()
        .measure(&f.store, &f.store.join(tar.store_key().unwrap()))
        .unwrap();
    assert_eq!(extracted.provenance, DigestProvenance::Measured);
}

fn ustar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, contents) in entries {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        header[108..115].copy_from_slice(b"0000000");
        header[116..123].copy_from_slice(b"0000000");
        header[124..135].copy_from_slice(format!("{:011o}", contents.len()).as_bytes());
        header[136..147].copy_from_slice(b"00000000000");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(contents);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0; 1024]);
    out
}
