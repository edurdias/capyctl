//! ADR 0008: materializing declared model sources, against a local fake
//! Hugging Face hub and HTTPS origin (served as loopback HTTP through the
//! store's test seam). No test reaches a real network.
//!
//! These are CPU tests of the fetch, verification and accounting logic. They
//! are not qualification: a live Hugging Face download on a Spark is pending.

use axum::body::Body;
use axum::http::{HeaderMap, Request, Response, StatusCode};
use mllm_agent::sources::{prune, reason, SourceStatus, SourceStore};
use mllm_config::effective::{ModelSourcePolicy, SourceSwitch};
use mllm_config::model_source::{Archive, ModelSource};
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
        url: format!("https://weights.example.test/files/{name}"),
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
        SourceStore::with_loopback_origin(
            &self.store,
            policy(max_bytes),
            Some(self.secrets.clone()),
            &self.origin,
        )
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
        let state = self.store.join("sources/.mllm");
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
        std::fs::read_dir(self.store.join("sources/.mllm/partial"))
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
        url: "https://weights.example.test/files/w.gguf".into(),
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

// T14 (ADR 0008): remote sources are denied unless the host opts in; a
// denied request reaches no origin.
#[tokio::test]
async fn host_policy_denies_remote_sources() {
    let f = fixture().await;
    let store = SourceStore::with_loopback_origin(
        &f.store,
        ModelSourcePolicy::default(),
        Some(f.secrets.clone()),
        &f.origin,
    );
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
    assert_eq!(store.status(&unreferenced), SourceStatus::Pending);
    // Something outside `sources/` is never touched.
    std::fs::create_dir_all(f.store.join("toy")).unwrap();
    prune(&f.store, &BTreeSet::new(), true).unwrap();
    assert!(f.store.join("toy").is_dir());
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
