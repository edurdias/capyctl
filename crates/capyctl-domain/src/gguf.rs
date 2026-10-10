//! ADR 0029 §9: what a llama.cpp launch's GGUF header and files tell
//! resolution, as the host measured them beside the checkpoint's digest
//! (ADR 0014 §7). The header is read, never tensor data
//! (`capyctl_config::gguf`); the derivation that uses these facts is
//! `capyctl_config::context_fit::llamacpp`.

use serde::{Deserialize, Serialize};

/// One llama.cpp launch's GGUF facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GgufFacts {
    /// ADR 0029 §9, ADR 0014 amendment A6: the bytes the launch loads: the
    /// rendered GGUF (all its shards), the multimodal projector and the draft
    /// model (all its shards). Zero while pending, when no model could be
    /// picked or when the draft model could not be counted.
    pub weights_bytes: i64,
    /// `<arch>.context_length`, the model's training context, when the header
    /// states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub training_context: Option<u32>,
    /// The KV cache the header describes, or why CapyCTL does not derive it.
    pub kv: GgufKv,
}

/// ADR 0029 §9: the KV cache a header describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GgufKv {
    /// ADR 0014 §7: a provisional revision's placeholder, before any host
    /// measured the checkpoint. Never measured, never on the wire.
    Pending,
    /// Full attention in every layer that holds a cache.
    Attention(GgufKvShape),
    /// A cache CapyCTL does not derive; the deployment states
    /// `memory.kv_cache` or `resources` instead.
    Refused(GgufKvRefusal),
}

/// ADR 0029 §9: the elements one cached token holds, summed over the layers
/// that keep a cache: `Σ head_count_kv × key_length` for K and
/// `Σ head_count_kv × value_length` for V.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GgufKvShape {
    /// The layers that keep a cache (`block_count` less the MTP layers the
    /// main context skips).
    pub layers: u32,
    pub k_values: u64,
    pub v_values: u64,
    /// The V elements when every layer's row is as wide as the widest one:
    /// llama.cpp pads the V cache so without flash attention
    /// (`llama-kv-cache.cpp`, `n_embd_v_gqa_max`). Equal to `v_values` when
    /// every layer has the same width.
    pub v_values_padded: u64,
}

/// ADR 0029 §9: why a header's cache is not derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GgufKvRefusal {
    /// No single GGUF model could be picked from the checkpoint.
    NoModel,
    /// ADR 0014 amendment A6, ADR 0029 §8: the draft model the arguments
    /// name could not be counted: missing, a split set incomplete or not
    /// named by its first shard, or outside `security.approved_paths`.
    Draft,
    /// The header could not be read, or states no attention shape.
    Unreadable,
    /// Sliding-window (or chunked) attention layers.
    SlidingWindow,
    /// Recurrent layers (`ssm.*`, `wkv.*`).
    Recurrent,
    /// Attention and recurrent layers mixed (`full_attention_interval`).
    Hybrid,
    /// Multi-head latent attention (`attention.kv_lora_rank`).
    Mla,
    /// A cache llama.cpp lays out its own way (an indexer cache, an encoder,
    /// or none at all).
    Layout,
}

impl GgufKvRefusal {
    pub const ALL: [Self; 8] = [
        Self::NoModel,
        Self::Draft,
        Self::Unreadable,
        Self::SlidingWindow,
        Self::Recurrent,
        Self::Hybrid,
        Self::Mla,
        Self::Layout,
    ];

    /// The stable code carried on the wire.
    pub fn code(self) -> &'static str {
        match self {
            Self::NoModel => "no_model",
            Self::Draft => "draft",
            Self::Unreadable => "unreadable",
            Self::SlidingWindow => "sliding_window",
            Self::Recurrent => "recurrent",
            Self::Hybrid => "hybrid",
            Self::Mla => "mla",
            Self::Layout => "layout",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|refusal| refusal.code() == code)
    }

    /// What a refusal tells the operator.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoModel => "the checkpoint holds no single GGUF model to render",
            Self::Draft => {
                "the draft model the arguments name cannot be counted: it must be a GGUF file \
                 (a split set named by its first shard, every shard present) inside \
                 security.approved_paths"
            }
            Self::Unreadable => "the GGUF header cannot be read or states no attention shape",
            Self::SlidingWindow => "the model uses sliding-window attention",
            Self::Recurrent => "the model has recurrent layers",
            Self::Hybrid => "the model mixes attention and recurrent layers",
            Self::Mla => "the model uses multi-head latent attention",
            Self::Layout => "llama.cpp lays out this model's cache its own way",
        }
    }

    /// Whether the refusal leaves the weights the launch loads unknown (no
    /// model to render, or a draft model that could not be counted), so a
    /// declared budget cannot be checked against them either.
    pub fn weights_unmeasured(self) -> bool {
        matches!(self, Self::NoModel | Self::Draft)
    }
}

impl GgufFacts {
    /// ADR 0014 §7: the facts a provisional revision is frozen with until a
    /// host measures the checkpoint.
    pub const PENDING: Self = Self {
        weights_bytes: 0,
        training_context: None,
        kv: GgufKv::Pending,
    };

    /// Whether every field is in range for measured facts: weights not
    /// negative, a stated training context positive, an attention shape with
    /// layers and both K and V elements. `Pending` is never measured.
    pub fn is_valid(&self) -> bool {
        self.weights_bytes >= 0
            && self.training_context != Some(0)
            && match self.kv {
                GgufKv::Pending => false,
                GgufKv::Attention(shape) => {
                    shape.layers > 0
                        && shape.k_values > 0
                        && shape.v_values > 0
                        && shape.v_values_padded >= shape.v_values
                }
                GgufKv::Refused(_) => true,
            }
    }
}

/// What a llama.cpp revision was sized with, recorded so a snapshot
/// re-derives identically; the memory block's `weights_bytes` is then
/// `facts.weights_bytes`, what the launch loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GgufSizing {
    /// The whole checkpoint's measured weight files: what a launch plan
    /// names and the host verifies (ADR 0014 §7).
    pub checkpoint_weights_bytes: i64,
    pub facts: GgufFacts,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_codes_round_trip() {
        for refusal in GgufKvRefusal::ALL {
            assert_eq!(GgufKvRefusal::from_code(refusal.code()), Some(refusal));
        }
        assert_eq!(GgufKvRefusal::from_code("pending"), None);
    }

    #[test]
    fn measured_facts_are_checked() {
        let shape = GgufKvShape {
            layers: 2,
            k_values: 1024,
            v_values: 1024,
            v_values_padded: 1024,
        };
        let facts = GgufFacts {
            weights_bytes: 1,
            training_context: Some(4096),
            kv: GgufKv::Attention(shape),
        };
        assert!(facts.is_valid());
        assert!(!GgufFacts::PENDING.is_valid());
        assert!(!GgufFacts {
            training_context: Some(0),
            ..facts
        }
        .is_valid());
        assert!(!GgufFacts {
            kv: GgufKv::Attention(GgufKvShape {
                v_values_padded: 1,
                ..shape
            }),
            ..facts
        }
        .is_valid());
        assert!(GgufFacts {
            kv: GgufKv::Refused(GgufKvRefusal::Mla),
            ..facts
        }
        .is_valid());
    }

    #[test]
    fn the_recorded_form_is_stable() {
        let sizing = GgufSizing {
            checkpoint_weights_bytes: 10,
            facts: GgufFacts {
                weights_bytes: 8,
                training_context: None,
                kv: GgufKv::Refused(GgufKvRefusal::SlidingWindow),
            },
        };
        let value = serde_json::to_value(sizing).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "checkpoint_weights_bytes": 10,
                "facts": {"weights_bytes": 8, "kv": {"refused": "sliding_window"}}
            })
        );
        assert_eq!(serde_json::from_value::<GgufSizing>(value).unwrap(), sizing);
        assert_eq!(
            serde_json::to_value(GgufFacts::PENDING).unwrap()["kv"],
            "pending"
        );
    }
}
