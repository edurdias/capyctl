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
    /// Closed map: only the listed fields allowed (unlisted -> UnknownField).
    Struct(&'static [(&'static str, FieldSpec)]),
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
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("listeners", LISTENERS),
            ],
        },
        ConfigKind::Deployment => &KindSchema {
            required: &["schema_version", "kind", "name", "model"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("model", SCALAR),
                // No F0 consumers yet; empty allowlists reject everything
                // unknown inside these blocks.
                ("resources", FieldSpec::Struct(NO_FIELDS)),
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
