//! Config kinds and per-kind field allowlists.
//!
//! Field sets come from SPEC §16. For F0 this is a pragmatic subset: the
//! top-level required fields and the fields exercised by the tests. New
//! kinds' fields are added by extending the static tables in `schema()`
//! without reworking the walk.

/// Which config document kind a file is being validated as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    Server,
    Host,
    Deployment,
    Standalone,
    /// ADR 0018 §2: the capyctl-owned `engines.yaml` beside a role document.
    Engines,
}

impl ConfigKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigKind::Server => "server",
            ConfigKind::Host => "host",
            ConfigKind::Deployment => "deployment",
            ConfigKind::Standalone => "standalone",
            ConfigKind::Engines => "engines",
        }
    }
}

impl std::str::FromStr for ConfigKind {
    type Err = crate::error::ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "server" => Ok(ConfigKind::Server),
            "host" => Ok(ConfigKind::Host),
            "deployment" => Ok(ConfigKind::Deployment),
            "standalone" => Ok(ConfigKind::Standalone),
            "engines" => Ok(ConfigKind::Engines),
            _ => Err(crate::error::ConfigError::new(
                crate::error::ConfigErrorCode::SchemaVersion,
                "kind",
                format!("unknown config kind `{s}`"),
            )),
        }
    }
}

/// What shape a known field may take.
///
/// Every nested object gets an explicit allowlist — there is no
/// "accept anything" variant. Where a nested block has no F0 consumer
/// yet, it uses `Struct(NO_FIELDS)`: the empty allowlist rejects every
/// unknown key, which keeps the "reject unknown capyctl fields" constraint
/// end-to-end and still parses cleanly when the block is empty.
pub enum FieldSpec {
    /// Any scalar value.
    Scalar,
    /// String scalar matching the size/duration unit regex
    /// `^(\d+(?:\.\d+)?)\s?(B|KiB|MiB|GiB|TiB|s|m|h|ms)$`.
    Unit,
    /// String scalar carrying an exact byte quantity.
    Bytes,
    /// String scalar carrying an exact duration.
    Duration,
    /// Closed map: only the listed fields allowed (unlisted -> UnknownField).
    Struct(&'static [(&'static str, FieldSpec)]),
    /// Closed map whose listed fields are all required.
    RequiredStruct(&'static [(&'static str, FieldSpec)]),
    /// Transitional F1/F2 field: legacy scalar or strict F2 mapping.
    ScalarOrStruct(&'static [(&'static str, FieldSpec)]),
    /// Mapping whose keys are entry *names* (not capyctl fields — e.g.
    /// `listeners` entries named `management`/`inference`); every value
    /// must match the given entry spec, so entries themselves stay
    /// strictly allowlisted.
    MapOf(&'static FieldSpec),
    /// Sequence of items, each validated against the given spec.
    Seq(&'static FieldSpec),
    /// A field that used to live here and has moved (SPEC §15.3 strictness).
    /// It is refused like an unknown field, with a message naming where the
    /// setting lives now, so an old document fails with a pointer rather than
    /// a bare "unknown field".
    Moved(&'static str),
}

/// ADR 0014 §1: the host profile's engine tuning moved to the deployment.
pub const LAUNCH_SETTINGS_MOVED: &str =
    "engine tuning moved from the host profile's `launch_settings` to the deployment's \
     `engine_config`; the host keeps executable, environment, security \
     and host-fixed `args` only";

/// ADR 0008: a Hugging Face revision is itself the pinned commit.
pub const LOCKED_COMMIT_MOVED: &str =
    "`locked_commit` is retired: `revision` must itself be the full commit SHA \
     (model sources accept pinned revisions only)";

/// Empty allowlist for nested blocks with no F0 consumer yet: rejects
/// every unknown key, accepts only empty mappings.
const NO_FIELDS: &[(&str, FieldSpec)] = &[];

/// Allowlist for one config kind.
pub struct KindSchema {
    /// Fields that must be present at the top level.
    pub required: &'static [&'static str],
    /// Known top-level fields and their shapes.
    pub fields: &'static [(&'static str, FieldSpec)],
}

/// Schema lookup for a kind. Extend these tables as the SPEC §16 field
/// sets are filled in.
pub fn schema(kind: ConfigKind) -> &'static KindSchema {
    const SCALAR: FieldSpec = FieldSpec::Scalar;
    const UNIT: FieldSpec = FieldSpec::Unit;
    const BYTES: FieldSpec = FieldSpec::Bytes;
    const DURATION: FieldSpec = FieldSpec::Duration;

    // Listener entry shape (per SPEC §16): each named listener carries
    // bind address + authentication mode; unknown entry fields rejected.
    const LISTENER_ENTRY_FIELDS: &[(&str, FieldSpec)] =
        &[("bind", SCALAR), ("authentication", SCALAR)];
    static LISTENER_ENTRY: FieldSpec = FieldSpec::Struct(LISTENER_ENTRY_FIELDS);
    const LISTENERS: FieldSpec = FieldSpec::MapOf(&LISTENER_ENTRY);
    const SCHEDULER: &[(&str, FieldSpec)] = &[(
        "queue",
        FieldSpec::Struct(&[("max_buffered_bytes_total", UNIT)]),
    )];
    const TLS: &[(&str, FieldSpec)] = &[("mode", SCALAR), ("identity_dir", SCALAR)];
    // ADR 0028 §3: the group policy, stated the same way on a host and on
    // standalone (owner rule: standalone is a server plus one host).
    const GROUPS: FieldSpec = FieldSpec::Struct(&[
        ("peer_address", SCALAR),
        (
            "rendezvous_port_range",
            FieldSpec::Struct(&[("start", SCALAR), ("end", SCALAR)]),
        ),
        ("require_rdma", SCALAR),
    ]);
    const RESOURCE_POLICY: &[(&str, FieldSpec)] = &[
        ("allowed_devices", SCALAR),
        (
            "memory",
            FieldSpec::Struct(&[
                ("accounting", SCALAR),
                (
                    "system",
                    FieldSpec::Struct(&[
                        ("managed_limit", SCALAR),
                        ("free_reserve", SCALAR),
                        ("parked_limit", SCALAR),
                    ]),
                ),
            ]),
        ),
        // Owner rule 2026-09-25: the engines' port range, as on a host.
        (
            "endpoint_port_range",
            FieldSpec::Struct(&[("start", SCALAR), ("end", SCALAR)]),
        ),
        // SPEC §10, §16.2 (owner rule 2026-09-25: standalone is a server and
        // one host): the queue bounds, as on a host.
        ("queue", FieldSpec::Struct(QUEUE)),
        ("groups", GROUPS),
    ];
    const DEVICE: FieldSpec = FieldSpec::Struct(&[("id", SCALAR), ("sharing", SCALAR)]);
    const ALLOCATION: FieldSpec = FieldSpec::Struct(&[
        ("domain", SCALAR),
        ("bytes", BYTES),
        ("host_kv_bytes", BYTES),
    ]);
    const PHASE: FieldSpec = FieldSpec::Struct(&[
        ("allocations", FieldSpec::Seq(&ALLOCATION)),
        ("devices", FieldSpec::Seq(&DEVICE)),
    ]);
    const RECIPE: &[(&str, FieldSpec)] = &[
        ("cold", PHASE),
        ("ready", PHASE),
        ("parking", PHASE),
        ("parked", PHASE),
        ("wake", PHASE),
        // The short form, one figure each for every phase (`short_resources`).
        ("gpu", BYTES),
        ("ram", BYTES),
    ];
    // Spec §7: a deployment names where its weights come from. The union of every
    // variant's keys is listed once; which subset is legal is decided by the tagged
    // `ModelSource` in `effective.rs`, so a `repo` on an `http` source is refused
    // there rather than being silently ignored here.
    //
    // ADR 0008: the same variants may also be written externally tagged
    // (`{huggingface: {repo, revision}}`); each variant block is closed.
    const MODEL_SOURCE: FieldSpec = FieldSpec::Struct(&[
        ("type", SCALAR),
        ("path", SCALAR),
        ("repo", SCALAR),
        ("revision", SCALAR),
        ("files", FieldSpec::Seq(&SCALAR)),
        ("token_ref", SCALAR),
        ("locked_commit", FieldSpec::Moved(LOCKED_COMMIT_MOVED)),
        ("url", SCALAR),
        ("sha256", SCALAR),
        ("archive", SCALAR),
        ("local", FieldSpec::Struct(&[("path", SCALAR)])),
        (
            "huggingface",
            FieldSpec::Struct(&[
                ("repo", SCALAR),
                ("revision", SCALAR),
                ("files", FieldSpec::Seq(&SCALAR)),
                ("token_ref", SCALAR),
                ("locked_commit", FieldSpec::Moved(LOCKED_COMMIT_MOVED)),
            ]),
        ),
        (
            "http",
            FieldSpec::Struct(&[("url", SCALAR), ("sha256", SCALAR), ("archive", SCALAR)]),
        ),
    ]);
    // ADR 0008 (owner decision 2026-09-25): remote model sources are allowed
    // by default; a host states `denied` (or `disabled`) to turn one off.
    // `path` names the sources store (default: the models directory, so
    // downloads live in `<model_store>/sources`).
    const MODEL_SOURCES: FieldSpec = FieldSpec::Struct(&[
        ("huggingface", SCALAR),
        ("http", SCALAR),
        ("max_bytes", BYTES),
        ("allowed_hosts", FieldSpec::Seq(&SCALAR)),
        ("huggingface_endpoint", SCALAR),
        ("path", SCALAR),
        // Owner rule 2026-09-25: the protected file holding the host's
        // Hugging Face token (a secret is a file or a variable, never a flag).
        ("huggingface_token_file", SCALAR),
    ]);
    const MODEL: &[(&str, FieldSpec)] = &[
        // Spec §7: `path` predates `source` and still means a local source.
        ("path", SCALAR),
        ("source", MODEL_SOURCE),
        ("content_fingerprint", SCALAR),
        ("revision", SCALAR),
    ];
    // Spec §7: the directory a host keeps model weights under. A host states it
    // once; a deployment's relative local path is resolved against it.
    const MODEL_STORE: FieldSpec = FieldSpec::RequiredStruct(&[("path", SCALAR)]);
    // Owner rule 2026-09-25 (`crate::engine_settings`): the role's own engine
    // installation, the YAML form of `--vllm-bin` / `CAPYCTL_VLLM_BIN` and the
    // rest; published as the runtime profile `local`.
    const LOCAL_ENGINE: FieldSpec = FieldSpec::Struct(&[
        ("vllm", SCALAR),
        ("sglang", SCALAR),
        ("tensorfold", SCALAR),
        ("build_fingerprint", SCALAR),
        ("args", FieldSpec::Seq(&SCALAR)),
        ("kv_cache", BYTES),
        ("deep_park", SCALAR),
        ("trust_remote_code", SCALAR),
        ("installation_drift", SCALAR),
        // SPEC §13.3 amendment (owner decision 2026-09-25): the CUDA toolkit
        // of the role's own installation, published as its `cuda_home`.
        ("cuda_home", SCALAR),
    ]);
    const DOMAIN: FieldSpec = FieldSpec::Struct(&[
        ("managed_limit", BYTES),
        ("free_reserve", BYTES),
        ("host_kv_limit", BYTES),
        ("parked_limit", BYTES),
        ("memory", SCALAR),
        // ADR 0019: the device a `device` domain's memory belongs to.
        ("device", SCALAR),
    ]);
    const HOST_DEVICE: FieldSpec = FieldSpec::Struct(&[
        ("domain", SCALAR),
        ("sharing", SCALAR),
        ("physical_gpu_uuid", SCALAR),
    ]);
    const QUEUE: &[(&str, FieldSpec)] = &[
        ("max_pending_per_deployment", SCALAR),
        ("max_pending_total", SCALAR),
        ("max_buffered_bytes_total", BYTES),
        ("request_deadline", DURATION),
        ("admission_window", DURATION),
        // SPEC §10: idle bound between a relayed stream's backend events.
        ("stream_idle_timeout", DURATION),
    ];
    const F2_RESOURCE_POLICY: &[(&str, FieldSpec)] = &[
        ("domains", FieldSpec::MapOf(&DOMAIN)),
        ("devices", FieldSpec::MapOf(&HOST_DEVICE)),
        ("max_parked", SCALAR),
        ("observation_ttl", DURATION),
        ("device_sharing", SCALAR),
        (
            "endpoint_port_range",
            FieldSpec::Struct(&[("start", SCALAR), ("end", SCALAR)]),
        ),
        ("planner_max_states", SCALAR),
        ("queue", FieldSpec::Struct(QUEUE)),
        // ADR 0013 §2: host labels a deployment's `placement.selector` matches.
        // Host policy, published with the host document; never asserted by a
        // deployment.
        ("labels", FieldSpec::MapOf(&SCALAR)),
        // ADR 0028 §3: the host's multi-node group policy.
        ("groups", GROUPS),
    ];
    const SECURITY: &[(&str, FieldSpec)] = &[
        // Spec §3: `deep_park` replaces `experimental_controls`. It is a switch over
        // one named capability rather than a blanket "I accept experiments". SPEC
        // §9.1 / ADR 0012: omitted means enabled; `disabled` is the host opt-out.
        ("deep_park", SCALAR),
        // Spec §3: executing checkpoint-supplied Python is opt-in per host.
        ("trust_remote_code", SCALAR),
        ("credential_ref", SCALAR),
        ("admin_credential_ref", SCALAR),
        // ADR 0014 §6 (owner decision Q10): deployment extra arguments are
        // allowed unless the host says `denied`.
        ("extra_args", SCALAR),
        // ADR 0014 §8: security-sensitive options this installation approves by
        // name, and the directories an approved path option may name.
        ("approved_options", FieldSpec::Seq(&SCALAR)),
        ("approved_paths", FieldSpec::Seq(&SCALAR)),
        // ADR 0028 §2.1: environment names and globs a deployment may set.
        ("approved_env", FieldSpec::Seq(&SCALAR)),
        // ADR 0008 (owner decision 2026-09-23): `warn` (default) or `refuse`
        // when a launch finds the installation drifted from its registration.
        ("installation_drift", SCALAR),
    ];
    // ADR 0014 §2: a deployment's typed engine parameters. Common fields first,
    // then one block per engine family; which family block is legal is decided
    // against the selected installation at resolution.
    const ENGINE_MEMORY: FieldSpec =
        FieldSpec::Struct(&[("request", BYTES), ("kv_cache", BYTES), ("startup", BYTES)]);
    const ENGINE_CONFIG: FieldSpec = FieldSpec::Struct(&[
        ("dtype", SCALAR),
        ("quantization", SCALAR),
        ("kv_cache_dtype", SCALAR),
        ("context_length", SCALAR),
        ("max_concurrent_requests", SCALAR),
        ("cuda_graphs", SCALAR),
        ("language_model_only", SCALAR),
        ("trust_remote_code", SCALAR),
        ("memory", ENGINE_MEMORY),
        (
            "vllm",
            FieldSpec::Struct(&[
                ("block_size_tokens", SCALAR),
                ("max_num_batched_tokens", SCALAR),
                // ADR 0014 §4 (amended 2026-10-07): `eager` or `lazy`.
                ("safetensors_load_strategy", SCALAR),
                // ADR 0024: `auto` (default), `none`, or a parser name.
                ("tool_call_parser", SCALAR),
                ("reasoning_parser", SCALAR),
            ]),
        ),
        (
            "sglang",
            FieldSpec::Struct(&[
                ("max_total_tokens", SCALAR),
                ("chunked_prefill_size", SCALAR),
                ("tokenizer_workers", SCALAR),
                ("tool_call_parser", SCALAR),
                ("reasoning_parser", SCALAR),
            ]),
        ),
        // ADR 0023 §4: TensorFold's own typed fields.
        (
            "tensorfold",
            FieldSpec::Struct(&[("max_tokens", SCALAR), ("thinking", SCALAR)]),
        ),
        ("accept_extra_args", SCALAR),
        ("extra_args", FieldSpec::Seq(&SCALAR)),
        // ADR 0028 §2.1: engine environment, names approved by the profile.
        ("env", FieldSpec::MapOf(&SCALAR)),
    ]);
    const PROFILE: FieldSpec = FieldSpec::Struct(&[
        ("engine", SCALAR),
        ("revision", SCALAR),
        ("executable", SCALAR),
        ("build_fingerprint", SCALAR),
        // ADR 0014 §1: host-fixed arguments stay with the installation.
        ("args", FieldSpec::Seq(&SCALAR)),
        ("launch_settings", FieldSpec::Moved(LAUNCH_SETTINGS_MOVED)),
        ("env", FieldSpec::MapOf(&SCALAR)),
        // SPEC §13.3 amendment (owner decision 2026-09-25): the CUDA toolkit
        // the engine's JIT compilers use; `<cuda_home>/bin` joins its PATH.
        ("cuda_home", SCALAR),
        ("security", FieldSpec::Struct(SECURITY)),
        (
            "log_policy",
            FieldSpec::Struct(&[("max_file_bytes", BYTES), ("retained_files", SCALAR)]),
        ),
    ]);
    // Standalone `server:`/`host:` blocks mirror the generated standalone
    // shape minus `kind` (the wrapper document already carries the kind).
    const STANDALONE_SERVER: &[(&str, FieldSpec)] = &[
        ("name", SCALAR),
        ("state_dir", SCALAR),
        ("listeners", LISTENERS),
        ("tls", FieldSpec::Struct(TLS)),
        // SPEC §10 (W10): the switch drain bound of the embedded server.
        ("switching", FieldSpec::Struct(SWITCHING)),
        // SPEC §6.5 (W5), owner rule (standalone is a server and one host): the
        // embedded server's idle policy, as in a server document.
        ("lifecycle_defaults", FieldSpec::Struct(LIFECYCLE_DEFAULTS)),
        // SPEC §17 (M80): the router's per-request timing header.
        ("observability", FieldSpec::Struct(OBSERVABILITY)),
        // ADR 0028 §11 (decided 2026-10-06): the embedded server's group
        // request-stall timeout, as in a server document.
        ("groups", FieldSpec::Struct(SERVER_GROUPS)),
    ];
    /// SPEC §4.3 (owner decision P3): a role's shutdown drain bound, role-local
    /// in the server, host and standalone documents.
    const SHUTDOWN: &[(&str, FieldSpec)] = &[("drain_timeout", DURATION)];
    /// SPEC §6.5, §16.1 (W5): the controller-owned idle policy.
    const LIFECYCLE_DEFAULTS: &[(&str, FieldSpec)] = &[
        ("ready_idle_timeout", DURATION),
        ("parked_idle_timeout", DURATION),
    ];
    /// SPEC §10 (W10): request-driven and `--evict` switching bounds.
    const SWITCHING: &[(&str, FieldSpec)] = &[("drain_timeout", DURATION)];
    /// SPEC §17 (M80): router observability. `timing_header` adds the
    /// `x-capyctl-timing` response header; off unless set.
    const OBSERVABILITY: &[(&str, FieldSpec)] = &[("timing_header", SCALAR)];
    /// ADR 0028 §11 (decided 2026-10-06): the group request-stall timeout.
    const SERVER_GROUPS: &[(&str, FieldSpec)] = &[("stall_timeout", DURATION)];
    const STANDALONE_HOST: &[(&str, FieldSpec)] = &[
        ("name", SCALAR),
        ("state_dir", SCALAR),
        ("connection", SCALAR),
        // Owner decision 2026-09-25: the models directory (default
        // `~/models`) and the model-source policy, as on a host.
        ("model_store", MODEL_STORE),
        ("model_sources", MODEL_SOURCES),
        // Owner rule 2026-09-25: the engine installation and the runtime
        // directory, as on a host.
        ("local_engine", LOCAL_ENGINE),
        ("runtime_dir", SCALAR),
        ("resource_policy", FieldSpec::Struct(RESOURCE_POLICY)),
        // Emitted empty by the generator; empty allowlist accepts `{}`
        // only until profile shapes are specified.
        ("runtime_profiles", FieldSpec::Struct(NO_FIELDS)),
    ];

    match kind {
        ConfigKind::Server => &KindSchema {
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("listeners", LISTENERS),
                ("scheduler", FieldSpec::Struct(SCHEDULER)),
                ("state_dir", SCALAR),
                ("identity_dir", SCALAR),
                (
                    "enrollment",
                    FieldSpec::RequiredStruct(&[
                        ("bootstrap_address", SCALAR),
                        ("control_address", SCALAR),
                    ]),
                ),
                ("shutdown", FieldSpec::Struct(SHUTDOWN)),
                // SPEC §6.5, §16.1 (W5): the controller-owned idle policy.
                ("lifecycle_defaults", FieldSpec::Struct(LIFECYCLE_DEFAULTS)),
                // Owner decision 2026-09-23: control-session heartbeat bounds.
                (
                    "control",
                    FieldSpec::Struct(&[
                        ("heartbeat_suspend_after", DURATION),
                        ("heartbeat_lost_after", DURATION),
                    ]),
                ),
                // SPEC §10 (W10): the switch drain bound.
                ("switching", FieldSpec::Struct(SWITCHING)),
                // SPEC §17 (M80): router observability.
                ("observability", FieldSpec::Struct(OBSERVABILITY)),
                // ADR 0028 §11 (decided 2026-10-06): multi-node groups.
                ("groups", FieldSpec::Struct(SERVER_GROUPS)),
            ],
        },
        ConfigKind::Host => &KindSchema {
            // Owner decision 2026-09-25: the model store defaults to
            // `~/models` (or `--models-root`, `CAPYCTL_MODELS_ROOT`); the role
            // states the resolved directory in the document it publishes.
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("model_store", MODEL_STORE),
                ("model_sources", MODEL_SOURCES),
                ("state_dir", SCALAR),
                ("identity_dir", SCALAR),
                ("runtime_dir", SCALAR),
                ("local_engine", LOCAL_ENGINE),
                (
                    "ingress",
                    FieldSpec::Struct(&[
                        ("bind", SCALAR),
                        ("address", SCALAR),
                        ("transport", SCALAR),
                    ]),
                ),
                ("listeners", LISTENERS),
                ("hardware_fingerprint", SCALAR),
                ("environment_fingerprint", SCALAR),
                ("device_inventory_digest", SCALAR),
                ("resource_policy", FieldSpec::Struct(F2_RESOURCE_POLICY)),
                ("runtime_profiles", FieldSpec::MapOf(&PROFILE)),
                // Role-local: the period of the agent's engine load reports.
                ("load_report_interval", DURATION),
                ("shutdown", FieldSpec::Struct(SHUTDOWN)),
            ],
        },
        ConfigKind::Deployment => &KindSchema {
            required: &["schema_version", "kind", "name", "model"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                // ADR 0013 §2: `host` is shorthand for `placement.hosts: [host]`.
                ("host", SCALAR),
                ("instances", SCALAR),
                (
                    "placement",
                    FieldSpec::Struct(&[
                        ("hosts", FieldSpec::Seq(&SCALAR)),
                        ("selector", FieldSpec::MapOf(&SCALAR)),
                        ("strategy", SCALAR),
                        ("max_per_host", SCALAR),
                    ]),
                ),
                // ADR 0028 §2: the multi-node topology.
                (
                    "topology",
                    FieldSpec::Struct(&[
                        ("tensor_parallel", SCALAR),
                        ("pipeline_parallel", SCALAR),
                    ]),
                ),
                ("model", FieldSpec::ScalarOrStruct(MODEL)),
                ("routes", FieldSpec::Seq(&SCALAR)),
                ("runtime_profile", SCALAR),
                ("runtime_profile_revision", SCALAR),
                ("recipe", SCALAR),
                ("residency", SCALAR),
                ("recovery", SCALAR),
                ("devices", FieldSpec::Seq(&DEVICE)),
                // ADR 0014 §5: optional; when omitted the phases are derived
                // from `engine_config.memory`.
                ("resources", FieldSpec::Struct(RECIPE)),
                ("engine_config", ENGINE_CONFIG),
                ("request_deadline", DURATION),
                // ADR 0014 amendment A1: deployment-level lifecycle bounds,
                // beside `request_deadline`; derived when omitted.
                (
                    "timeouts",
                    FieldSpec::Struct(&[("initialize", DURATION), ("wake", DURATION)]),
                ),
                ("env", FieldSpec::Struct(NO_FIELDS)),
                // SPEC §6.5 (ADR 0013 amendment 2026-09-23): the warm-residency
                // commitment, a deployment-level policy like `instances`.
                ("lifecycle", FieldSpec::Struct(&[("warm", SCALAR)])),
            ],
        },
        ConfigKind::Standalone => &KindSchema {
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                // Owner decision 2026-09-25: the state root, as `--state-dir`
                // and `CAPYCTL_STATE_DIR` name it (they win over it).
                ("state_dir", SCALAR),
                ("server", FieldSpec::Struct(STANDALONE_SERVER)),
                ("host", FieldSpec::Struct(STANDALONE_HOST)),
                ("shutdown", FieldSpec::Struct(SHUTDOWN)),
            ],
        },
        // ADR 0018 §2: the capyctl-owned engines file beside a role document.
        ConfigKind::Engines => &KindSchema {
            required: &["schema_version", "kind"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("runtime_profiles", FieldSpec::MapOf(&PROFILE)),
            ],
        },
    }
}
