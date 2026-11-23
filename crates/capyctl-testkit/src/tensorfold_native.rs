//! ADR 0039 (Implementation plan item 6): a fake of TensorFold's native
//! server (`tensorfold-native serve`, 1.0.0 and 1.1.5) for adapter tests.
//!
//! What it copies from the published source (`zig/src/` at v1.0.0 and
//! v1.1.5):
//!
//! - no listener until the engine has loaded (`server/main.zig` opens the
//!   engine before `serve.run` binds): a connection is refused until
//!   [`FakeTensorfoldNative::load`] (or the delay of
//!   [`FakeTensorfoldNative::load_after`]) opens it;
//! - `GET /health` (`server/status_routes.zig`): 200 `{"status": "ok",
//!   "model", "model_ids", "max_batch_size", "warming", "memory", "live"}`,
//!   plus 1.1.5's `version`, `backend` and `device`; a key-configured
//!   engine's bare `{"status": "ok"}`; and 1.1.5's 503 `{"status":
//!   "draining", "requests_in_progress": n}` and `{"status":
//!   "unavailable"}`;
//! - `GET /v1/models`, and `GET /metrics` with `tensorfold:requests_running`
//!   and `tensorfold:requests_waiting` (and their `num_requests_*` mirrors)
//!   from one status read (`server/metrics.zig`);
//! - a chat completion carrying the `tensorfold` object;
//! - the `serve` parser (`server/cli.zig`): exact names, `--name=value`
//!   split at `=`, and the engine's exact refusal of a flag the binary does
//!   not serve, for a Linux (CUDA) build.
//!
//! It is not an engine: passing a test against it qualifies nothing (SPEC
//! §18); the TF rows do.
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

/// The two releases whose source was read (ADR 0039 Evidence §2, §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeRelease {
    V1_0_0,
    V1_1_5,
}

impl NativeRelease {
    pub fn version(self) -> &'static str {
        match self {
            Self::V1_0_0 => "1.0.0",
            Self::V1_1_5 => "1.1.5",
        }
    }

    /// The tail of the refusal of a flag the binary does not serve
    /// (1.0.0 `server/cli.zig:174-177`, 1.1.5 `server/cli.zig:281-284`).
    fn unserved_tail(self) -> &'static str {
        match self {
            Self::V1_0_0 => "serve this command with --engine python",
            Self::V1_1_5 => {
                "the Python engine 0.6.6 serves it: python -m pip install \
                 git+https://github.com/ashhart/TensorFold.git@v0.6.6"
            }
        }
    }

    fn flags(self) -> &'static [Flag] {
        match self {
            Self::V1_0_0 => FLAGS_1_0_0,
            Self::V1_1_5 => FLAGS_1_1_5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Value,
    StoreTrue,
    Append,
}

/// One row of the `serve` flag table (`server/cli.zig` `Flag`).
#[derive(Debug, Clone, Copy)]
struct Flag {
    name: &'static str,
    kind: Kind,
    choices: &'static [&'static str],
    native: bool,
    native_values: Option<&'static [&'static str]>,
}

const fn served(name: &'static str) -> Flag {
    Flag {
        name,
        kind: Kind::Value,
        choices: &[],
        native: true,
        native_values: None,
    }
}

const fn unserved(name: &'static str) -> Flag {
    Flag {
        native: false,
        ..served(name)
    }
}

const fn switch(name: &'static str, native: bool) -> Flag {
    Flag {
        kind: Kind::StoreTrue,
        native,
        ..served(name)
    }
}

const fn append(name: &'static str) -> Flag {
    Flag {
        kind: Kind::Append,
        ..served(name)
    }
}

const fn choice(name: &'static str, choices: &'static [&'static str], native: bool) -> Flag {
    Flag {
        choices,
        native,
        ..served(name)
    }
}

const fn only(name: &'static str, values: &'static [&'static str]) -> Flag {
    Flag {
        native_values: Some(values),
        ..served(name)
    }
}

/// The Linux build's `--backend` values (`backend_values`).
const BACKEND_VALUES: &[&str] = &["auto", "cuda"];

/// v1.0.0 `server/cli.zig:30-92`, as the CUDA (Linux) build has it.
const FLAGS_1_0_0: &[Flag] = &[
    served("--host"),
    served("--port"),
    served("--name"),
    append("--alias"),
    append("--api-key"),
    served("--api-key-file"),
    switch("--metrics-open", true),
    switch("--dashboard", true),
    switch("--vision", false),
    switch("--vision-urls", false),
    switch("--vision-offload", false),
    unserved("--vision-max-images"),
    unserved("--vision-image-tokens"),
    served("--context"),
    served("--speed-up"),
    served("--max-tokens"),
    served("--temperature"),
    served("--top-p"),
    served("--top-k"),
    served("--min-p"),
    switch("--thinking", true),
    switch("--no-thinking", true),
    choice(
        "--reasoning-effort",
        &["low", "medium", "high", "xhigh"],
        true,
    ),
    served("--thinking-budget"),
    switch("--loop-guard", true),
    switch("--no-drafts", true),
    served("--keep-warm"),
    served("--compact-at"),
    served("--compact-keep"),
    served("--compact-memory"),
    unserved("--drafter"),
    unserved("--drafter-bits"),
    unserved("--mtp-drafts"),
    unserved("--mtp-confidence"),
    choice("--lane-kernels", &["auto", "on", "off"], false),
    served("--prompt-cache-gib"),
    switch("--prompt-cache-over-cap", true),
    unserved("--checkpoint-slots"),
    unserved("--spill-gib"),
    only("--snapshot-dir", &["none"]),
    only("--max-snapshots", &["0"]),
    served("--parallel"),
    unserved("--decode-share"),
    unserved("--prefill-pass"),
    unserved("--pass-cache-gib"),
    unserved("--mlx-cache-gib"),
    unserved("--ssd-experts"),
    switch("--ple-on-ssd", false),
    switch("--no-update-check", true),
    Flag {
        choices: &["auto", "mlx", "cuda"],
        native_values: Some(BACKEND_VALUES),
        ..served("--backend")
    },
    choice("--tp", &["1", "2"], false),
    choice("--rank", &["0", "1"], false),
    unserved("--master"),
    unserved("--master-port"),
    choice("--kv-dtype", &["bf16", "int8", "int4"], false),
    switch("--prefill-fp8", false),
    switch("--no-prefill-fp8", false),
    choice("--precision", &["checkpoint", "full"], false),
    served("--device"),
    served("--segments"),
];

/// v1.1.5 `server/cli.zig:41-153`, as the CUDA (Linux) build has it. Not
/// copied: the flags of `drama_cfg.zig` (not read). The rows the build's own
/// `serves` list decides (`--mtp-drafts`, `--mtp-confidence`,
/// `--checkpoint-slots`, `--tp`, `--rank`, `--master`, `--master-port`,
/// `--policy`) are unserved here [INFERENCE: the CUDA root's list was not
/// read].
const FLAGS_1_1_5: &[Flag] = &[
    served("--host"),
    append("--allowed-host"),
    append("--allowed-origin"),
    served("--allowed-controls"),
    served("--port"),
    served("--name"),
    served("--chat-template"),
    served("--voices"),
    served("--default-voice"),
    append("--alias"),
    append("--api-key"),
    served("--api-key-file"),
    switch("--metrics-open", true),
    switch("--dashboard", true),
    switch("--vision", true),
    switch("--vision-urls", true),
    switch("--vision-offload", false),
    served("--vision-max-images"),
    served("--vision-max-videos"),
    served("--vision-image-tokens"),
    served("--vision-video-tokens"),
    served("--context"),
    served("--max-frames"),
    unserved("--speed-up"),
    served("--max-tokens"),
    served("--temperature"),
    switch("--prompt-seed", true),
    served("--top-p"),
    served("--top-k"),
    served("--min-p"),
    switch("--thinking", true),
    switch("--no-thinking", true),
    choice(
        "--reasoning-effort",
        &["low", "medium", "high", "xhigh"],
        true,
    ),
    served("--thinking-budget"),
    served("--thinking-reserve"),
    switch("--loop-guard", true),
    switch("--no-drafts", true),
    served("--keep-warm"),
    served("--idle-unload"),
    served("--compact-at"),
    served("--compact-keep"),
    served("--compact-memory"),
    switch("--slide", true),
    served("--slide-graph"),
    served("--drafter"),
    choice("--drafter-bits", &["0", "4"], true),
    served("--record"),
    choice("--kv-cache", &["bf16", "fp8"], true),
    choice("--prompt-precision", &["bf16", "int8"], true),
    unserved("--mtp-drafts"),
    unserved("--mtp-confidence"),
    choice("--lane-kernels", &["auto", "on", "off"], false),
    served("--prompt-cache-gib"),
    switch("--prompt-cache-over-cap", true),
    unserved("--checkpoint-slots"),
    unserved("--spill-gib"),
    only("--snapshot-dir", &["none"]),
    only("--max-snapshots", &["0"]),
    served("--parallel"),
    unserved("--decode-share"),
    unserved("--prefill-pass"),
    unserved("--pass-cache-gib"),
    unserved("--mlx-cache-gib"),
    unserved("--ssd-experts"),
    switch("--ple-on-ssd", false),
    switch("--no-update-check", true),
    Flag {
        choices: &["auto", "mlx", "cuda", "rocm"],
        native_values: Some(BACKEND_VALUES),
        ..served("--backend")
    },
    choice("--tp", &["1", "2", "4", "8"], false),
    unserved("--rank"),
    unserved("--master"),
    unserved("--master-port"),
    Flag {
        kind: Kind::Append,
        native: false,
        ..served("--policy")
    },
    choice("--kv-dtype", &["bf16", "int8", "int4"], false),
    switch("--prefill-fp8", false),
    switch("--no-prefill-fp8", false),
    choice("--precision", &["checkpoint", "full"], true),
    served("--device"),
    served("--segments"),
    served("--loops"),
    switch("--learn", true),
    served("--learn-dir"),
    served("--learn-gib"),
    served("--learn-min-free-gib"),
    switch("--learn-conversations", true),
    served("--learn-daily-gib"),
    switch("--recall", true),
    served("--recall-dir"),
    served("--recall-gib"),
    switch("--recall-read-only", true),
    switch("--cortheon", true),
];

/// What a `serve` command line the fake accepted says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeArgs {
    pub model: String,
    pub name: String,
    pub port: u16,
    pub aliases: Vec<String>,
    /// `--parallel` as written (`auto` when absent).
    pub parallel: String,
    pub context: Option<i64>,
}

impl ServeArgs {
    /// The streams the engine admits: `auto` is 8, a number is clamped to
    /// 1 to 4096 (`server/cli.zig` `parallel`). The fake does not model the
    /// engine's handling of anything else, and takes 8.
    pub fn lanes(&self) -> u64 {
        let text = self.parallel.trim();
        if text.eq_ignore_ascii_case("auto") {
            return 8;
        }
        text.parse::<i64>().map_or(8, |n| n.clamp(1, 4096) as u64)
    }

    /// The names `/health` `model_ids` and `/v1/models` list.
    pub fn model_ids(&self) -> Vec<String> {
        std::iter::once(self.name.clone())
            .chain(self.aliases.iter().cloned())
            .collect()
    }
}

/// The line the engine prints after its usage line when it refuses the
/// command (`server/main.zig`: `tensorfold serve: error: <message>`).
fn usage(message: String) -> String {
    format!("tensorfold serve: error: {message}")
}

/// Parse `serve`'s arguments (after `serve`) as `release`'s `server/cli.zig`
/// `parse` does; `Err` is the engine's own error line. Only the checks the
/// fake needs are modelled: names, kinds, choices, served flags and values,
/// `--port`, and the model.
pub fn parse_serve(release: NativeRelease, args: &[String]) -> Result<ServeArgs, String> {
    let mut model: Option<String> = None;
    let mut out = ServeArgs {
        model: String::new(),
        name: String::new(),
        port: 8080,
        aliases: Vec::new(),
        parallel: "auto".into(),
        context: None,
    };
    let mut i = 0;
    while i < args.len() {
        let token = &args[i];
        i += 1;
        if !token.starts_with("--") {
            if model.is_some() {
                return Err(usage(format!("unrecognized arguments: {token}")));
            }
            model = Some(token.clone());
            continue;
        }
        let (name, inline) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (token.as_str(), None),
        };
        let Some(flag) = release.flags().iter().find(|flag| flag.name == name) else {
            return Err(usage(format!("unrecognized arguments: {token}")));
        };
        let mut value = None;
        if flag.kind != Kind::StoreTrue {
            value = match inline {
                Some(value) => Some(value),
                None => {
                    let Some(next) = args.get(i) else {
                        return Err(usage(format!("argument {name}: expected one argument")));
                    };
                    i += 1;
                    Some(next.clone())
                }
            };
            let given = value.as_deref().unwrap_or_default();
            if !flag.choices.is_empty() && !flag.choices.contains(&given) {
                return Err(usage(format!("argument {name}: invalid choice: '{given}'")));
            }
        } else if let Some(given) = inline {
            return Err(usage(format!(
                "argument {name}: ignored explicit argument '{given}'"
            )));
        }
        if !flag.native {
            return Err(usage(format!(
                "{name} is not served by the native engine yet; {}",
                release.unserved_tail()
            )));
        }
        if let (Some(allowed), Some(given)) = (flag.native_values, value.as_deref()) {
            if !allowed.contains(&given) {
                return Err(usage(format!(
                    "{name} {given} is not served by the native engine; {}",
                    release.unserved_tail()
                )));
            }
        }
        let given = value.unwrap_or_default();
        match name {
            "--name" => out.name = given,
            "--alias" => out.aliases.push(given),
            "--parallel" => out.parallel = given,
            "--port" => {
                let port: i64 = given
                    .trim()
                    .parse()
                    .map_err(|_| usage(format!("argument {name}: invalid int value: '{given}'")))?;
                out.port = u16::try_from(port)
                    .map_err(|_| usage(format!("argument --port: invalid port: '{given}'")))?;
            }
            "--context" => {
                out.context = Some(given.trim().parse().map_err(|_| {
                    usage(format!("argument {name}: invalid int value: '{given}'"))
                })?);
            }
            _ => {}
        }
    }
    out.model = model.ok_or_else(|| usage("the following arguments are required: model".into()))?;
    Ok(out)
}

/// What `/health` answers (ADR 0039 §9, Evidence §3, §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeHealthMode {
    /// 200 with the open body.
    Serving,
    /// 200 `{"status": "ok"}` alone: an engine started with a key.
    KeyConfigured,
    /// 1.1.5: 503 `{"status": "unavailable"}`.
    Unavailable,
    /// 1.1.5: 503 `{"status": "draining", "requests_in_progress": n}`.
    Draining,
}

struct Shared {
    release: NativeRelease,
    served: String,
    model_ids: Vec<String>,
    max_batch_size: AtomicU64,
    health: Mutex<NativeHealthMode>,
    running: AtomicU64,
    waiting: AtomicU64,
    metrics_held: AtomicBool,
    health_reads: AtomicUsize,
    chats: AtomicUsize,
}

/// A native TensorFold server on a loopback port that opens only once it is
/// "loaded". Cheap to clone; every clone is the same fake.
#[derive(Clone)]
pub struct FakeTensorfoldNative {
    port: u16,
    shared: Arc<Shared>,
    server: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

/// A loopback port nobody listens on: bound to learn a free one, then
/// released.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("a free loopback port")
}

impl FakeTensorfoldNative {
    /// A fake serving `served` with `max_batch_size` streams on a free port,
    /// not loaded yet.
    pub fn new(release: NativeRelease, served: &str, max_batch_size: u64) -> Self {
        Self::build(
            release,
            free_port(),
            vec![served.to_owned()],
            max_batch_size,
        )
    }

    /// The fake a rendered command would start (`argv[0]` the binary, then
    /// `serve`), on the command's port, or the engine's own refusal of it.
    pub fn from_argv(release: NativeRelease, argv: &[String]) -> Result<Self, String> {
        if argv.get(1).map(String::as_str) != Some("serve") {
            return Err(
                "usage: tensorfold-native capabilities --json | serve MODEL [flags]".into(),
            );
        }
        let args = parse_serve(release, &argv[2..])?;
        Ok(Self::build(
            release,
            args.port,
            args.model_ids(),
            args.lanes(),
        ))
    }

    fn build(release: NativeRelease, port: u16, model_ids: Vec<String>, lanes: u64) -> Self {
        Self {
            port,
            shared: Arc::new(Shared {
                release,
                served: model_ids.first().cloned().unwrap_or_default(),
                model_ids,
                max_batch_size: AtomicU64::new(lanes),
                health: Mutex::new(NativeHealthMode::Serving),
                running: AtomicU64::new(0),
                waiting: AtomicU64::new(0),
                metrics_held: AtomicBool::new(false),
                health_reads: AtomicUsize::new(0),
                chats: AtomicUsize::new(0),
            }),
            server: Arc::default(),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>`.
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The engine has loaded: the listener opens. Idempotent.
    pub async fn load(&self) {
        let mut server = self.server.lock().unwrap();
        if server.is_some() {
            return;
        }
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", self.port)).expect("the fake's port is free");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        let app = axum::Router::new()
            .route("/health", get(health))
            .route("/v1/models", get(models))
            .route("/metrics", get(metrics))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/completions", post(completion))
            .with_state(self.shared.clone());
        *server = Some(tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        }));
    }

    /// The listener opens after `delay` (a load that takes that long).
    pub fn load_after(&self, delay: Duration) {
        let fake = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            fake.load().await;
        });
    }

    /// What `/health` answers from now on. The 503 bodies are 1.1.5's.
    pub fn set_health(&self, mode: NativeHealthMode) {
        assert!(
            self.shared.release == NativeRelease::V1_1_5
                || !matches!(
                    mode,
                    NativeHealthMode::Unavailable | NativeHealthMode::Draining
                ),
            "1.0.0 has no 503 on /health"
        );
        *self.shared.health.lock().unwrap() = mode;
    }

    /// The two work gauges `/metrics` reports.
    pub fn set_work(&self, running: u64, waiting: u64) {
        self.shared.running.store(running, Ordering::SeqCst);
        self.shared.waiting.store(waiting, Ordering::SeqCst);
    }

    /// `/metrics` takes the request and never answers it.
    pub fn hold_metrics(&self, held: bool) {
        self.shared.metrics_held.store(held, Ordering::SeqCst);
    }

    /// The stream count `/health` reports (as if the engine admitted
    /// another count than the command asked for).
    pub fn set_max_batch_size(&self, lanes: u64) {
        self.shared.max_batch_size.store(lanes, Ordering::SeqCst);
    }

    /// `/health` requests answered so far.
    pub fn health_reads(&self) -> usize {
        self.shared.health_reads.load(Ordering::SeqCst)
    }

    /// Chat completions answered so far.
    pub fn chats(&self) -> usize {
        self.shared.chats.load(Ordering::SeqCst)
    }
}

fn json_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn health(State(shared): State<Arc<Shared>>) -> Response {
    shared.health_reads.fetch_add(1, Ordering::SeqCst);
    let mode = *shared.health.lock().unwrap();
    let running = shared.running.load(Ordering::SeqCst);
    let waiting = shared.waiting.load(Ordering::SeqCst);
    match mode {
        NativeHealthMode::Draining => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{{\"status\": \"draining\", \"requests_in_progress\": {running}}}"),
        ),
        NativeHealthMode::Unavailable => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "{\"status\": \"unavailable\"}".into(),
        ),
        NativeHealthMode::KeyConfigured => {
            json_response(StatusCode::OK, "{\"status\": \"ok\"}".into())
        }
        NativeHealthMode::Serving => {
            let mut body = json!({
                "status": "ok",
                "model": shared.served,
                "model_ids": shared.model_ids,
                "max_batch_size": shared.max_batch_size.load(Ordering::SeqCst),
                "warming": false,
                "memory": {"active": 19_000_000_000_u64, "cache": 0, "peak": 19_500_000_000_u64},
                "live": {
                    "connections": running + waiting,
                    "waiting": waiting,
                    "decode_tokens_per_second": 0.0,
                    "prefill_tokens_per_second": 0.0,
                },
            });
            if shared.release == NativeRelease::V1_1_5 {
                body["version"] = json!(shared.release.version());
                body["backend"] = json!("cuda");
            }
            Json(body).into_response()
        }
    }
}

async fn models(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let data: Vec<Value> = shared
        .model_ids
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 1, "owned_by": "tensorfold"}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

async fn metrics(State(shared): State<Arc<Shared>>) -> Response {
    if shared.metrics_held.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    let running = shared.running.load(Ordering::SeqCst);
    let waiting = shared.waiting.load(Ordering::SeqCst);
    let body = format!(
        "# HELP tensorfold:requests_running Requests in prefill or decode.\n\
         # TYPE tensorfold:requests_running gauge\n\
         tensorfold:requests_running {running}\n\
         # HELP tensorfold:requests_waiting Requests queued or held until a lane is free.\n\
         # TYPE tensorfold:requests_waiting gauge\n\
         tensorfold:requests_waiting {waiting}\n\
         # HELP tensorfold:num_requests_running Requests in prefill or decode. A mirror of tensorfold:requests_running.\n\
         # TYPE tensorfold:num_requests_running gauge\n\
         tensorfold:num_requests_running {running}\n\
         # HELP tensorfold:num_requests_waiting Requests queued or held until a lane is free. A mirror of tensorfold:requests_waiting.\n\
         # TYPE tensorfold:num_requests_waiting gauge\n\
         tensorfold:num_requests_waiting {waiting}\n"
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// The `tensorfold` object a reply carries (ADR 0039 Evidence §6,
/// `server/chat.zig:501-530`); the drafting counts' names are 0.6's
/// [INFERENCE: not read at 1.0].
fn tensorfold_object() -> Value {
    json!({
        "time_to_first_token": 0.05,
        "prefill_seconds": 0.02,
        "tokens_per_second": 120.0,
        "token_sha": "0000",
        "drafted": 3,
        "accepted": 2,
    })
}

async fn chat(State(shared): State<Arc<Shared>>, Json(body): Json<Value>) -> Response {
    shared.chats.fetch_add(1, Ordering::SeqCst);
    let model = body["model"].clone();
    if body["stream"] != true {
        return Json(json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Ready."},
                         "finish_reason": "length"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
            "tensorfold": tensorfold_object(),
        }))
        .into_response();
    }
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
               "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut end = chunk(json!({}), json!("length"));
    end["tensorfold"] = tensorfold_object();
    end["usage"] = json!({"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5});
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant"}), Value::Null),
        chunk(json!({"content": "Ready."}), Value::Null),
        end
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        stream,
    )
        .into_response()
}

/// `/v1/completions` answers text alone: the native server returns no
/// token ids (ADR 0039 Evidence §6).
async fn completion(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "id": "cmpl-1", "object": "text_completion", "created": 1, "model": body["model"],
        "choices": [{"index": 0, "text": " ready", "finish_reason": "length"}],
        "tensorfold": tensorfold_object(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_owned()).collect()
    }

    // T41 (ADR 0039 §4): the fake refuses an unserved flag with each
    // release's own words, by exact name.
    #[test]
    fn unserved_flags_are_refused_with_the_engines_words() {
        assert_eq!(
            parse_serve(NativeRelease::V1_0_0, &args(&["/m", "--drafter", "x"])).unwrap_err(),
            "tensorfold serve: error: --drafter is not served by the native engine yet; serve \
             this command with --engine python"
        );
        assert!(parse_serve(NativeRelease::V1_1_5, &args(&["/m", "--drafter", "x"])).is_ok());
        assert_eq!(
            parse_serve(NativeRelease::V1_1_5, &args(&["/m", "--kv-dtype=bf16"])).unwrap_err(),
            "tensorfold serve: error: --kv-dtype is not served by the native engine yet; the \
             Python engine 0.6.6 serves it: python -m pip install \
             git+https://github.com/ashhart/TensorFold.git@v0.6.6"
        );
        assert_eq!(
            parse_serve(
                NativeRelease::V1_0_0,
                &args(&["/m", "--snapshot-dir", "/s"])
            )
            .unwrap_err(),
            "tensorfold serve: error: --snapshot-dir /s is not served by the native engine; \
             serve this command with --engine python"
        );
        assert_eq!(
            parse_serve(NativeRelease::V1_0_0, &args(&["/m", "--par", "2"])).unwrap_err(),
            "tensorfold serve: error: unrecognized arguments: --par"
        );
        let parsed = parse_serve(
            NativeRelease::V1_0_0,
            &args(&[
                "/m",
                "--name=n",
                "--port",
                "9",
                "--parallel",
                "4",
                "--alias",
                "a",
            ]),
        )
        .unwrap();
        assert_eq!(parsed.lanes(), 4);
        assert_eq!(parsed.model_ids(), ["n", "a"]);
        assert_eq!(parsed.port, 9);
    }
}
