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
}

impl ConfigKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigKind::Server => "server",
            ConfigKind::Host => "host",
            ConfigKind::Deployment => "deployment",
            ConfigKind::Standalone => "standalone",
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
/// unknown key, which keeps the "reject unknown mllm fields" constraint
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
    /// Mapping whose keys are entry *names* (not mllm fields — e.g.
    /// `listeners` entries named `management`/`inference`); every value
    /// must match the given entry spec, so entries themselves stay
    /// strictly allowlisted.
    MapOf(&'static FieldSpec),
    /// Sequence of items, each validated against the given spec.
    Seq(&'static FieldSpec),
}

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
    const RESOURCE_POLICY: &[(&str, FieldSpec)] = &[
        ("allowed_devices", SCALAR),
        (
            "memory",
            FieldSpec::Struct(&[
                ("accounting", SCALAR),
                (
                    "system",
                    FieldSpec::Struct(&[("managed_limit", SCALAR), ("free_reserve", SCALAR)]),
                ),
            ]),
        ),
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
    ];
    // Spec §7: a deployment names where its weights come from. The union of every
    // variant's keys is listed once; which subset is legal is decided by the tagged
    // `ModelSource` in `effective.rs`, so a `repo` on an `http` source is refused
    // there rather than being silently ignored here.
    const MODEL_SOURCE: FieldSpec = FieldSpec::Struct(&[
        ("type", SCALAR),
        ("path", SCALAR),
        ("repo", SCALAR),
        ("revision", SCALAR),
        ("locked_commit", SCALAR),
        ("url", SCALAR),
        ("sha256", SCALAR),
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
    const DOMAIN: FieldSpec = FieldSpec::Struct(&[
        ("managed_limit", BYTES),
        ("free_reserve", BYTES),
        ("host_kv_limit", BYTES),
        ("parked_limit", BYTES),
        ("memory", SCALAR),
    ]);
    const HOST_DEVICE: FieldSpec = FieldSpec::Struct(&[("domain", SCALAR), ("sharing", SCALAR)]);
    const QUEUE: &[(&str, FieldSpec)] = &[
        ("max_pending_per_deployment", SCALAR),
        ("max_pending_total", SCALAR),
        ("max_buffered_bytes_total", BYTES),
        ("request_deadline", DURATION),
        ("admission_window", DURATION),
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
    ];
    const SECURITY: &[(&str, FieldSpec)] = &[
        // Spec §3: `deep_park` replaces `experimental_controls`. It is a switch over
        // one named capability rather than a blanket "I accept experiments", and it
        // defaults to enabled, so omitting it keeps parking available.
        ("deep_park", SCALAR),
        // Spec §3: executing checkpoint-supplied Python is opt-in per host.
        ("trust_remote_code", SCALAR),
        ("credential_ref", SCALAR),
        ("admin_credential_ref", SCALAR),
    ];
    const REQUESTED_BUDGET: FieldSpec = FieldSpec::Struct(&[
        ("kv_cache_bytes", BYTES),
        ("swap_space_bytes", BYTES),
        ("gpu_utilization_pct", SCALAR),
        ("static_memory_fraction_bps", SCALAR),
    ]);
    const LAUNCH_SETTINGS: &[(&str, FieldSpec)] = &[
        ("engine", SCALAR),
        ("tensor_parallel_size", SCALAR),
        ("pipeline_parallel_size", SCALAR),
        ("enable_sleep_mode", SCALAR),
        ("kv_cache_dtype", SCALAR),
        ("block_size_tokens", SCALAR),
        ("cpu_offload_bytes", BYTES),
        ("recipe", SCALAR),
        ("requested_budget", REQUESTED_BUDGET),
    ];
    const PROFILE: FieldSpec = FieldSpec::Struct(&[
        ("engine", SCALAR),
        ("revision", SCALAR),
        ("executable", SCALAR),
        ("build_fingerprint", SCALAR),
        ("args", FieldSpec::Seq(&SCALAR)),
        ("launch_settings", FieldSpec::Struct(LAUNCH_SETTINGS)),
        ("env", FieldSpec::MapOf(&SCALAR)),
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
    ];
    const STANDALONE_HOST: &[(&str, FieldSpec)] = &[
        ("name", SCALAR),
        ("state_dir", SCALAR),
        ("connection", SCALAR),
        // Spec §7: allowed here so a standalone document can carry the store the
        // host block is translated into; the generated default does not set one yet.
        ("model_store", MODEL_STORE),
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
            ],
        },
        ConfigKind::Host => &KindSchema {
            // Spec §7: the model store is required, not defaulted. Guessing a
            // directory would make a relative model path resolve somewhere the
            // operator never named.
            required: &["schema_version", "kind", "name", "model_store"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("model_store", MODEL_STORE),
                ("listeners", LISTENERS),
                ("hardware_fingerprint", SCALAR),
                ("environment_fingerprint", SCALAR),
                ("resource_policy", FieldSpec::Struct(F2_RESOURCE_POLICY)),
                ("runtime_profiles", FieldSpec::MapOf(&PROFILE)),
            ],
        },
        ConfigKind::Deployment => &KindSchema {
            required: &["schema_version", "kind", "name", "model"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("model", FieldSpec::ScalarOrStruct(MODEL)),
                ("routes", FieldSpec::Seq(&SCALAR)),
                ("runtime_profile", SCALAR),
                ("runtime_profile_revision", SCALAR),
                ("recipe", SCALAR),
                ("residency", SCALAR),
                ("recovery", SCALAR),
                ("devices", FieldSpec::Seq(&DEVICE)),
                ("resources", FieldSpec::Struct(RECIPE)),
                ("request_deadline", DURATION),
                ("env", FieldSpec::Struct(NO_FIELDS)),
            ],
        },
        ConfigKind::Standalone => &KindSchema {
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("server", FieldSpec::Struct(STANDALONE_SERVER)),
                ("host", FieldSpec::Struct(STANDALONE_HOST)),
            ],
        },
    }
}
