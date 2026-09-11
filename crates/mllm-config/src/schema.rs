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
pub enum FieldSpec {
    /// Any scalar value.
    Scalar,
    /// String scalar matching the size/duration unit regex
    /// `^(\d+(?:\.\d+)?)\s?(B|KiB|MiB|GiB|TiB|s|m|h|ms)$`.
    Unit,
    /// Closed map: only the listed fields allowed (unlisted -> UnknownField).
    Struct(&'static [(&'static str, FieldSpec)]),
    /// Map whose entries are unrestricted (F0 escape hatch for later growth).
    OpenMap,
    /// Sequence of items, each validated against the given spec.
    Seq(&'static FieldSpec),
}

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
    match kind {
        ConfigKind::Server => &KindSchema {
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("listeners", FieldSpec::OpenMap),
                (
                    "scheduler",
                    FieldSpec::Struct(&[(
                        "queue",
                        FieldSpec::Struct(&[("max_buffered_bytes_total", FieldSpec::Unit)]),
                    )]),
                ),
            ],
        },
        ConfigKind::Host => &KindSchema {
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("listeners", FieldSpec::OpenMap),
            ],
        },
        ConfigKind::Deployment => &KindSchema {
            required: &["schema_version", "kind", "name", "model"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("model", SCALAR),
                ("resources", FieldSpec::OpenMap),
                ("env", FieldSpec::OpenMap),
            ],
        },
        ConfigKind::Standalone => &KindSchema {
            required: &["schema_version", "kind", "name"],
            fields: &[
                ("schema_version", SCALAR),
                ("kind", SCALAR),
                ("name", SCALAR),
                ("server", FieldSpec::OpenMap),
                ("host", FieldSpec::OpenMap),
            ],
        },
    }
}
