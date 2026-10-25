//! ADR 0035: one model on several identical discrete GPUs of one host.
//!
//! A deployment with `topology.tensor_parallel: N` and fewer than two
//! `placement.hosts` is one instance on N GPUs: one owner whose footprint has
//! one allocation per chosen GPU's `device` domain plus the host's `system`
//! domain (§3). `topology: auto` (§1a) records, in its revision, the candidate
//! table: for each N from 1 to the host's discrete GPU count that the engine
//! can run, the per-GPU and host-RAM phases derived with `T = N`. Placement
//! chooses N from the cards' managed limits, never their free memory
//! ([`choose_tensor_parallel`]).

use super::*;
use crate::topology::{INSUFFICIENT_GPUS, TENSOR_PARALLEL_INDIVISIBLE};
use serde_json::Value;

/// ADR 0035 §3: the communication allowance each GPU of an instance on N > 1
/// GPUs is charged beside its rank's request (NCCL and custom-all-reduce
/// buffers, the contexts a rank opens on its peers' GPUs). A placeholder,
/// `derived`, until live rows MG1 and MG2 measure it.
pub const LOCAL_TP_COMMUNICATION_PLACEHOLDER_BYTES: i64 = 1 << 30;

/// ADR 0035 §3: the host RAM each rank after the first adds to the engine's
/// host overhead (a Python process importing torch and the engine). A
/// placeholder, `derived`, until MG1 and MG2 measure it.
pub const LOCAL_TP_HOST_OVERHEAD_PER_RANK_BYTES: i64 = 3 << 30;

/// ADR 0035 §3: the per-GPU communication allowance of an instance on
/// `ranks` GPUs; nothing on one GPU.
pub fn communication_allowance(ranks: u32) -> i64 {
    if ranks > 1 {
        LOCAL_TP_COMMUNICATION_PLACEHOLDER_BYTES
    } else {
        0
    }
}

/// ADR 0035 §3: the engine's host overhead on `ranks` GPUs: 4 GiB plus 3 GiB
/// for each rank after the first.
pub fn engine_host_overhead(ranks: u32) -> i64 {
    ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES.saturating_add(
        LOCAL_TP_HOST_OVERHEAD_PER_RANK_BYTES.saturating_mul(i64::from(ranks.max(1) - 1)),
    )
}

/// Whether derivation refuses a charge above a domain's managed limit
/// (resolution) or records it (the candidate table and the hints that name
/// what would fit, ADR 0035 §1, §1a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Limits {
    Enforce,
    Ignore,
}

/// ADR 0035 §1, §1a: how a revision on several GPUs of one host was asked
/// for. Absent on one GPU with no `topology: auto`, so every such revision
/// encodes exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalTopologyRecord {
    /// The instance's tensor-parallel size N: its `selected_devices`, rank
    /// order.
    pub tensor_parallel: u32,
    /// ADR 0035 §3: the communication allowance charged on each GPU,
    /// `derived`, when the phases were derived (absent beside declared
    /// `resources` and on one GPU).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub communication_bytes: Option<i64>,
    /// ADR 0035 §1a: present when the deployment says `topology: auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<AutoTable>,
}

/// ADR 0035 §1a: the candidate table of `topology: auto`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutoTable {
    /// Whether the rows were sized from the checkpoint's measured weights.
    /// A host chooses N only once they were (ADR 0014 §7).
    pub weights_measured: bool,
    /// One row per tensor-parallel size the engine can run on this host,
    /// N = 1 first.
    pub candidates: Vec<AutoCandidate>,
    /// The sizes up to the host's GPU count left out, and why (a closed
    /// code: the engine's support data, the attention heads, a requirement
    /// only N = 1 meets, no set of identical GPUs).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<AutoExcluded>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutoExcluded {
    pub tensor_parallel: u32,
    pub reason: String,
}

/// ADR 0035 §1a, §3: one row of the candidate table: what each GPU and the
/// host's RAM are charged at that size, phase by phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutoCandidate {
    pub tensor_parallel: u32,
    /// Each GPU's charge (on a unified host, the pool's).
    pub device: PhaseCharges,
    /// The host's system domain (zero on a unified host).
    pub system: PhaseCharges,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PhaseCharges {
    pub cold: i64,
    pub ready: i64,
    pub parking: i64,
    pub parked: i64,
    pub wake: i64,
}

impl PhaseCharges {
    /// The largest phase: what a card's limit must hold.
    pub fn peak(&self) -> i64 {
        [self.cold, self.ready, self.parking, self.parked, self.wake]
            .into_iter()
            .max()
            .unwrap_or(0)
    }
}

/// ADR 0035 §1a: one discrete GPU of a host as the choice of N reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoGpu {
    pub id: String,
    pub managed_limit: i64,
    pub product: Option<String>,
    /// Managed limit plus free reserve: the card as the policy declares it.
    pub total: i64,
}

/// The discrete GPUs of `host` (its `device` domains), lowest driver index
/// first.
pub fn auto_gpus(host: &HostPolicy) -> Vec<AutoGpu> {
    let mut gpus: Vec<AutoGpu> = host
        .devices
        .iter()
        .filter_map(|(id, device)| {
            let domain = host.domains.get(&device.domain)?;
            (domain.memory == DomainMemory::Device).then(|| AutoGpu {
                id: id.clone(),
                managed_limit: domain.managed_limit,
                product: device.product_name.clone(),
                total: domain.managed_limit.saturating_add(domain.free_reserve),
            })
        })
        .collect();
    gpus.sort_by(|a, b| order(&a.id).cmp(&order(&b.id)));
    gpus
}

fn order(id: &str) -> (u32, &str) {
    (gpu_index(id).unwrap_or(u32::MAX), id)
}

/// ADR 0035 §1 (owner decision 4): the same product and total memory within
/// 64 MiB.
fn identical(a: &AutoGpu, b: &AutoGpu) -> bool {
    a.product == b.product
        && (a.total - b.total).abs() <= crate::instances::SAME_TOTAL_TOLERANCE_BYTES
}

/// What [`choose_tensor_parallel`] decided for one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoChoice {
    /// The smallest candidate N for which N identical GPUs' managed limits
    /// each hold the per-GPU charge of every phase, and every such set of N
    /// GPUs in rank order, the lowest indices first.
    Chosen {
        tensor_parallel: u32,
        sets: Vec<Vec<String>>,
    },
    /// The checkpoint's weights are not measured: no N is chosen (ADR 0035
    /// §1a, ADR 0014 §7).
    Unmeasured,
    /// No candidate fits any set of this host's GPUs.
    NoFit,
}

/// ADR 0035 §1a: choose N on one host, from the candidate table and the
/// host's GPUs with their managed limits (never their free memory, so the
/// same deployment on the same cards gets the same N whatever else runs).
/// `measured_cold` gives the per-GPU device peak measured at N, when one is
/// recorded; it replaces that row's cold phase.
pub fn choose_tensor_parallel(
    table: &AutoTable,
    gpus: &[AutoGpu],
    measured_cold: &dyn Fn(u32) -> Option<i64>,
) -> AutoChoice {
    if !table.weights_measured {
        return AutoChoice::Unmeasured;
    }
    for candidate in &table.candidates {
        let n = candidate.tensor_parallel as usize;
        let mut device = candidate.device;
        if let Some(peak) = measured_cold(candidate.tensor_parallel) {
            device.cold = peak;
        }
        let charge = device.peak();
        let holding: Vec<&AutoGpu> = gpus
            .iter()
            .filter(|gpu| gpu.managed_limit >= charge)
            .collect();
        let mut sets = Vec::new();
        crate::instances::for_each_combination(holding.len(), n, &mut |pick| {
            let first = holding[pick[0]];
            if pick.iter().all(|&i| identical(first, holding[i])) {
                sets.push(pick.iter().map(|&i| holding[i].id.clone()).collect());
            }
        });
        if !sets.is_empty() {
            return AutoChoice::Chosen {
                tensor_parallel: candidate.tensor_parallel,
                sets,
            };
        }
    }
    AutoChoice::NoFit
}

fn gib(bytes: i64) -> String {
    format!(
        "{bytes} bytes ({:.2} GiB)",
        bytes as f64 / f64::from(1 << 30)
    )
}

/// ADR 0035 §1, §1a: what would fit this host, named in every refusal for
/// memory or GPU count: the smallest tensor-parallel size whose per-GPU
/// charge the host's cards hold and how many GPUs that needs, or that no
/// size up to the host's GPU count fits, with the per-GPU charge at the
/// largest size and the cards that would hold it.
pub fn fit_hint(table: &AutoTable, gpus: &[AutoGpu]) -> String {
    match choose_tensor_parallel(table, gpus, &|_| None) {
        AutoChoice::Chosen {
            tensor_parallel, ..
        } => {
            let charge = table
                .candidates
                .iter()
                .find(|c| c.tensor_parallel == tensor_parallel)
                .map_or(0, |c| c.device.peak());
            format!(
                "what would fit: `topology: {{tensor_parallel: {tensor_parallel}}}`, on \
                 {tensor_parallel} identical GPUs of this host, each charged {}",
                gib(charge)
            )
        }
        AutoChoice::Unmeasured => "what would fit is known once the checkpoint is measured: \
                                   no tensor-parallel size is chosen before"
            .into(),
        AutoChoice::NoFit => {
            let largest = gpus.iter().map(|gpu| gpu.managed_limit).max().unwrap_or(0);
            match table.candidates.last() {
                Some(last) => format!(
                    "what would fit: no tensor_parallel up to this host's {} GPU(s) fits; at \
                     tensor_parallel: {n} each GPU is charged {}, above the {} the largest \
                     GPU here manages; {n} identical GPUs each managing at least that would \
                     hold it",
                    gpus.len(),
                    gib(last.device.peak()),
                    gib(largest),
                    n = last.tensor_parallel,
                ),
                None => format!(
                    "what would fit: no tensor_parallel up to this host's {} GPU(s) can run \
                     this deployment",
                    gpus.len()
                ),
            }
        }
    }
}

/// The per-GPU charge of a candidate row, its cold phase replaced by the
/// peak measured at that size when one is recorded.
fn charge_at(candidate: &AutoCandidate, measured_cold: &dyn Fn(u32) -> Option<i64>) -> i64 {
    let mut device = candidate.device;
    if let Some(peak) = measured_cold(candidate.tensor_parallel) {
        device.cold = peak;
    }
    device.peak()
}

/// ADR 0035 §1a: the refusal of a `topology: auto` deployment none of whose
/// sizes fits a host's GPUs ([`AutoChoice::NoFit`]), as a closed code and the
/// text that names what would fit there.
///
/// The per-GPU charge falls as N grows (each rank holds `ceil(sharded / N)`
/// of the weights) toward what every rank holds whatever N is (its KV cache,
/// the replicated tensors, the margin and the allowances). When the table's
/// two largest sizes say that this charge would come within the cards'
/// managed limit at some N above the host's GPU count, only a host with more
/// identical GPUs would hold the deployment: `insufficient_gpus`, naming that
/// N (an estimate from the table, `share = a / N + b`). Otherwise
/// `insufficient_device_memory`, naming the per-GPU charge at the largest N
/// this host offers and how many cards managing at least that would hold it.
pub fn auto_refusal(
    table: &AutoTable,
    gpus: &[AutoGpu],
    measured_cold: &dyn Fn(u32) -> Option<i64>,
) -> (&'static str, String) {
    let count = gpus.len();
    let limit = gpus.iter().map(|gpu| gpu.managed_limit).max().unwrap_or(0);
    let Some(last) = table.candidates.last() else {
        return (
            "insufficient_device_memory",
            format!("no tensor_parallel up to this host's {count} GPU(s) can run this deployment"),
        );
    };
    let largest = charge_at(last, measured_cold);
    let more_gpus = match table.candidates.as_slice() {
        [.., before, last] if limit > 0 => {
            // The charge without the communication allowance, which is the
            // same at every N above 1.
            let bare = |c: &AutoCandidate| {
                charge_at(c, measured_cold) as f64
                    - communication_allowance(c.tensor_parallel) as f64
            };
            let (n1, n2) = (
                f64::from(before.tensor_parallel),
                f64::from(last.tensor_parallel),
            );
            let a = (bare(before) - bare(last)) * n1 * n2 / (n2 - n1);
            let b = bare(last) - a / n2;
            let room = limit as f64 - LOCAL_TP_COMMUNICATION_PLACEHOLDER_BYTES as f64 - b;
            (a > 0.0 && room > 0.0)
                .then(|| (a / room).ceil())
                .filter(|n| *n > n2 && *n <= crate::topology::MAX_DIMENSION as f64)
                .map(|n| n as u32)
        }
        _ => None,
    };
    match more_gpus {
        Some(n) => (
            crate::topology::INSUFFICIENT_GPUS,
            format!(
                "no tensor_parallel up to this host's {count} GPU(s) fits; at tensor_parallel: \
                 {} each GPU is charged {}, above the {} each GPU here manages; about {n} \
                 identical GPUs like these would hold it (estimated from the candidate table)",
                last.tensor_parallel,
                gib(largest),
                gib(limit),
            ),
        ),
        None => (
            "insufficient_device_memory",
            format!(
                "no tensor_parallel up to this host's {count} GPU(s) fits; at tensor_parallel: \
                 {n} each GPU is charged {}, above the {} the largest GPU here manages; {n} \
                 identical GPUs each managing at least that would hold it",
                gib(largest),
                gib(limit),
                n = last.tensor_parallel,
            ),
        ),
    }
}

/// ADR 0035 §1, §1a: the number of attention heads in the checkpoint's
/// `config.json` (`num_attention_heads`, or its `text_config`'s for a
/// multimodal model). `None` when the checkpoint is not materialized here
/// or names none: then nothing is refused.
pub(super) fn attention_heads(root: Option<&Path>) -> Option<u64> {
    let config = crate::context_fit::read_model_config(root?).ok()?;
    config
        .get("num_attention_heads")
        .or_else(|| config.get("text_config")?.get("num_attention_heads"))
        .and_then(Value::as_u64)
        .filter(|heads| *heads > 0)
}

/// ADR 0035 §1: the GPUs of an instance on `n` GPUs of one host: the host
/// has discrete GPUs (a unified host has one GPU domain), the instance holds
/// `n` of them (else this host has no set of `n` identical GPUs), and pinned
/// GPUs are the same product with the same total memory.
pub(super) fn check_local_devices(
    n: u32,
    devices: &[DeviceClaim],
    host: &HostPolicy,
) -> Result<(), ConfigError> {
    let gpus = auto_gpus(host);
    if gpus.is_empty() {
        return Err(invalid(
            "topology",
            format!(
                "{}: host `{}` has no discrete GPU with memory of its own (a unified-memory \
                 host has one GPU domain), so a model runs on one GPU there; remove `topology`",
                crate::topology::MULTI_GPU_UNSUPPORTED,
                host.name
            ),
        ));
    }
    if devices.len() != n as usize {
        return Err(insufficient_gpus(n, host));
    }
    let Some(pinned) = devices
        .iter()
        .map(|claim| gpus.iter().find(|gpu| gpu.id == claim.id))
        .collect::<Option<Vec<&AutoGpu>>>()
    else {
        // A device the host does not have is refused by name below.
        return Ok(());
    };
    if let Some(other) = pinned.iter().find(|gpu| !identical(pinned[0], gpu)) {
        let describe = |gpu: &AutoGpu| {
            format!(
                "{} ({}, {})",
                gpu.id,
                gpu.product.as_deref().unwrap_or("product unknown"),
                gib(gpu.total)
            )
        };
        return Err(invalid(
            "devices",
            format!(
                "{}: the GPUs of one instance are the same product with the same total memory \
                 (within 64 MiB); {} and {} differ",
                crate::topology::MULTI_GPU_MIXED_DEVICES,
                describe(pinned[0]),
                describe(other)
            ),
        ));
    }
    Ok(())
}

/// ADR 0035 §1: vLLM and SGLang split attention heads across ranks, so N
/// must divide them (`tensor_parallel_indivisible`, naming the head count and
/// the sizes that divide it). llama.cpp's layer split has no such rule.
pub(super) fn check_heads(engine: Engine, n: u32, root: Option<&Path>) -> Result<(), ConfigError> {
    if n <= 1 || !matches!(engine, Engine::Vllm | Engine::Sglang) {
        return Ok(());
    }
    let Some(heads) = attention_heads(root) else {
        return Ok(());
    };
    if heads.is_multiple_of(u64::from(n)) {
        return Ok(());
    }
    let divisors: Vec<String> = (1..=heads.min(64))
        .filter(|d| heads.is_multiple_of(*d))
        .map(|d| d.to_string())
        .collect();
    Err(invalid(
        "topology.tensor_parallel",
        format!(
            "{TENSOR_PARALLEL_INDIVISIBLE}: the model has {heads} attention heads, which \
             tensor_parallel: {n} does not divide; the sizes that divide them are {}",
            divisors.join(", ")
        ),
    ))
}

/// ADR 0035 §1: a deployment asking for `n` GPUs on a host that has no set
/// of `n` identical discrete GPUs.
pub(super) fn insufficient_gpus(n: u32, host: &HostPolicy) -> ConfigError {
    let gpus = auto_gpus(host);
    let largest_identical = gpus
        .iter()
        .map(|gpu| gpus.iter().filter(|other| identical(gpu, other)).count())
        .max()
        .unwrap_or(0);
    let identical_note = if largest_identical < gpus.len() {
        format!(", at most {largest_identical} of them identical")
    } else {
        String::new()
    };
    invalid(
        "topology.tensor_parallel",
        format!(
            "{INSUFFICIENT_GPUS}: tensor_parallel: {n} needs {n} identical discrete GPUs; \
             host `{}` has {}{identical_note}",
            host.name,
            gpus.len()
        ),
    )
}

/// The deployment as an explicit `tensor_parallel: n` states it on `set`
/// (rank order): no `topology` at n = 1, and no `devices` when `set` is
/// empty (a unified host takes its default device).
fn variant(document: &Value, n: u32, set: &[String]) -> Value {
    let mut variant = document.clone();
    let object = variant
        .as_object_mut()
        .expect("a deployment document is a mapping");
    if n == 1 {
        object.remove("topology");
    } else {
        object.insert("topology".into(), serde_json::json!({"tensor_parallel": n}));
    }
    object.remove("devices");
    if !set.is_empty() {
        object.insert(
            "devices".into(),
            Value::Array(set.iter().map(|id| serde_json::json!({"id": id})).collect()),
        );
    }
    variant
}

impl EffectiveDeployment {
    /// ADR 0035 §1, §3: what `validate config` and `deploy` show: the charge
    /// on each GPU (rank 0's domain; every rank's is the same) and on the
    /// host's system domain, phase by phase, at this revision's N.
    pub fn per_gpu_charges(&self) -> AutoCandidate {
        row(self, self.local_tensor_parallel())
    }
}

/// ADR 0035 §1, §1a: what `validate config --host`, `deploy` and status show
/// of a deployment on several GPUs of one host or under `topology: auto`:
/// its GPUs in rank order, the charge on each GPU and on the host's RAM
/// phase by phase, and for `auto` the candidate table and the size this host
/// would choose now from its cards' managed limits (with the startup peak
/// `measured_cold` gives for a size, when one is recorded). `None` on one
/// GPU without `auto`.
pub fn topology_view(
    effective: &EffectiveDeployment,
    measured_cold: &dyn Fn(u32) -> Option<i64>,
) -> Option<Value> {
    let record = effective.topology.as_ref()?;
    let charges = effective.per_gpu_charges();
    let mut view = serde_json::json!({
        "tensor_parallel": record.tensor_parallel,
        "gpus": effective
            .selected_devices
            .iter()
            .map(|claim| claim.id.clone())
            .collect::<Vec<_>>(),
        "per_gpu": charges.device,
        "system": charges.system,
    });
    if let Some(bytes) = record.communication_bytes {
        view["communication_bytes"] = serde_json::json!(bytes);
    }
    if let Some(table) = &record.auto {
        let gpus = auto_gpus(&effective.host);
        let (chosen, note) = match choose_tensor_parallel(table, &gpus, measured_cold) {
            AutoChoice::Chosen {
                tensor_parallel, ..
            } => (serde_json::json!(tensor_parallel), Value::Null),
            AutoChoice::Unmeasured => (
                Value::Null,
                serde_json::json!("no size is chosen until the checkpoint is measured"),
            ),
            AutoChoice::NoFit => (Value::Null, serde_json::json!(fit_hint(table, &gpus))),
        };
        view["requested"] = serde_json::json!(crate::topology::AUTO);
        view["candidates"] = serde_json::json!(table);
        view["chosen"] = chosen;
        if !note.is_null() {
            view["note"] = note;
        }
    }
    Some(view)
}

/// A row of the table from a resolution at `n`.
fn row(effective: &EffectiveDeployment, n: u32) -> AutoCandidate {
    let host = &effective.host;
    let device_domain = effective
        .selected_devices
        .first()
        .and_then(|claim| host.devices.get(&claim.id))
        .map(|device| device.domain.as_str());
    let charges = |pick: &dyn Fn(&Allocation) -> bool| {
        let bytes = |phase: &PhaseFootprint| {
            phase
                .allocations
                .iter()
                .filter(|a| pick(a))
                .map(|a| a.bytes)
                .sum::<i64>()
        };
        let r = &effective.resources;
        PhaseCharges {
            cold: bytes(&r.cold),
            ready: bytes(&r.ready),
            parking: bytes(&r.parking),
            parked: bytes(&r.parked),
            wake: bytes(&r.wake),
        }
    };
    let memory = |a: &Allocation| host.domains.get(&a.domain).map(|d| d.memory);
    AutoCandidate {
        tensor_parallel: n,
        device: charges(&|a| Some(a.domain.as_str()) == device_domain),
        system: charges(&|a| {
            Some(a.domain.as_str()) != device_domain && memory(a) == Some(DomainMemory::Distinct)
        }),
    }
}

/// The leading closed code of a refusal's detail (`code: ...`).
fn leading_code(error: &ConfigError) -> String {
    error
        .detail
        .split(": ")
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// ADR 0035 §1a: the candidate table of `document` (an expanded deployment)
/// on `host`: each N from 1 to the host's discrete GPU count resolved on its
/// lowest set of N identical GPUs, as the explicit `tensor_parallel: N`
/// resolves, with no managed limit refused (the table records the charge;
/// [`choose_tensor_parallel`] judges it). A size the explicit deployment is
/// refused at (the engine's support data, the attention heads, a declared
/// `deep` or `host_backed` while the engine's N > 1 parking gate is closed,
/// `lmcache: required`) is left out with its code. N = 1's refusal is the
/// deployment's.
pub(super) fn candidate_table(
    document: &Value,
    host: &Value,
    facts: CheckpointFacts,
) -> Result<AutoTable, ConfigError> {
    let facts = CheckpointFacts {
        member_of: None,
        ..facts
    };
    let count = crate::instances::discrete_gpu_count(host) as u32;
    let mut candidates = Vec::new();
    let mut excluded = Vec::new();
    for n in 1..=count.max(1) {
        let set = if count == 0 {
            Vec::new()
        } else {
            match crate::instances::eligible_sets(host, n as usize, false)
                .into_iter()
                .next()
            {
                Some(set) => set,
                None => {
                    excluded.push(AutoExcluded {
                        tensor_parallel: n,
                        reason: INSUFFICIENT_GPUS.into(),
                    });
                    continue;
                }
            }
        };
        match resolve_with(&variant(document, n, &set), host, facts, Limits::Ignore) {
            Ok(effective) => candidates.push(row(&effective, n)),
            Err(error) if n == 1 => return Err(error),
            Err(error) => excluded.push(AutoExcluded {
                tensor_parallel: n,
                reason: leading_code(&error),
            }),
        }
    }
    Ok(AutoTable {
        weights_measured: facts.weights_bytes.is_some_and(|bytes| bytes > 0),
        candidates,
        excluded,
    })
}

/// ADR 0035 §1a: resolve `topology: auto` on one host: the candidate table,
/// and the revision of the N this host chooses now from its cards' managed
/// limits, on its lowest fitting set (the explicit `tensor_parallel: N`'s
/// revision, which launches exactly as it does), with the table recorded.
/// Before the weights are measured no N is chosen: the provisional revision
/// is N = 1's. When no N fits, `insufficient_device_memory` names the
/// per-GPU charge at the largest N and the cards that would hold it.
pub(super) fn resolve_auto(
    document: &Value,
    host: &Value,
    facts: CheckpointFacts,
) -> Result<EffectiveDeployment, ConfigError> {
    let table = candidate_table(document, host, facts)?;
    let policy = normalize_host_policy(host)?;
    let gpus = auto_gpus(&policy);
    let (n, set) = if gpus.is_empty() {
        (1, Vec::new())
    } else {
        match choose_tensor_parallel(&table, &gpus, &|_| None) {
            AutoChoice::Chosen {
                tensor_parallel,
                sets,
            } => (tensor_parallel, sets.into_iter().next().unwrap_or_default()),
            AutoChoice::Unmeasured => (1, vec![gpus[0].id.clone()]),
            AutoChoice::NoFit => {
                return Err(invalid(
                    "topology",
                    format!(
                        "insufficient_device_memory: no tensor-parallel size of `topology: \
                         auto` fits the GPUs of host `{}`; {}",
                        policy.name,
                        fit_hint(&table, &gpus)
                    ),
                ))
            }
        }
    };
    let facts = CheckpointFacts {
        member_of: None,
        ..facts
    };
    let mut effective = resolve_with(&variant(document, n, &set), host, facts, Limits::Enforce)?;
    let record = effective.topology.get_or_insert(LocalTopologyRecord {
        tensor_parallel: n,
        communication_bytes: None,
        auto: None,
    });
    record.auto = Some(table);
    Ok(effective)
}

/// ADR 0035 §1: a refusal for device memory or GPU count names what would
/// fit this host. Other refusals, a group, a deployment whose `resources`
/// name their domains, and a host without discrete GPUs are left as they are.
pub(super) fn with_fit_hint(
    error: ConfigError,
    document: &Value,
    host: &Value,
    facts: CheckpointFacts,
) -> ConfigError {
    let code = leading_code(&error);
    if code != "insufficient_device_memory" && code != INSUFFICIENT_GPUS {
        return error;
    }
    let names_domains = document
        .get("resources")
        .is_some_and(|r| !r.is_null() && !crate::short_resources::is_short(r));
    let grouped = !matches!(
        crate::instances::parse_instance_spec(document),
        Ok(spec) if spec.group.is_none()
    );
    if names_domains || grouped {
        return error;
    }
    let Ok(policy) = normalize_host_policy(host) else {
        return error;
    };
    let gpus = auto_gpus(&policy);
    if gpus.is_empty() {
        return error;
    }
    let Ok(table) = candidate_table(document, host, facts) else {
        return error;
    };
    let hint = fit_hint(&table, &gpus);
    ConfigError {
        detail: format!("{}; {hint}", error.detail),
        ..error
    }
}

impl EffectiveDeployment {
    /// ADR 0035: the instance's local tensor-parallel size N, the number of
    /// its selected devices in rank order (rank r on the r-th). 1 on one GPU
    /// and for a group member (one device per host, ADR 0028).
    pub fn local_tensor_parallel(&self) -> u32 {
        u32::try_from(self.selected_devices.len())
            .unwrap_or(u32::MAX)
            .max(1)
    }

    /// ADR 0035 §4: the namespaces of the instance's GPUs in rank order, as
    /// the engine child's `CUDA_VISIBLE_DEVICES` lists them
    /// ([`CudaNamespace::environment_of`]). On one GPU, [`Self::cuda_namespace`]'s
    /// answer as a list of one. With several, every GPU is pinned by its
    /// published UUID, or every one by its `gpuN` index in PCI bus order when
    /// any UUID is missing; a GPU named neither way is [`UnpinnableDevice`].
    pub fn cuda_namespaces(&self) -> Result<Option<Vec<CudaNamespace>>, UnpinnableDevice> {
        if self.selected_devices.len() <= 1 {
            return Ok(self.cuda_namespace()?.map(|namespace| vec![namespace]));
        }
        let devices = self
            .selected_devices
            .iter()
            .map(|claim| self.host.devices.get(&claim.id).map(|d| (claim, d)))
            .collect::<Option<Vec<_>>>()
            .ok_or(UnpinnableDevice)?;
        if let Some(uuids) = devices
            .iter()
            .map(|(_, device)| device.physical_gpu_uuid.clone())
            .collect::<Option<Vec<String>>>()
        {
            return Ok(Some(uuids.into_iter().map(CudaNamespace::Uuid).collect()));
        }
        devices
            .iter()
            .map(|(claim, _)| gpu_index(&claim.id).map(CudaNamespace::PciIndex))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or(UnpinnableDevice)
    }

    /// ADR 0035 §3: each GPU's device domain allocation in the Ready
    /// footprint, in rank order, as [`Self::ready_device_allocation`] names
    /// one (driver index, bytes). A rank's sizing renders from one GPU's
    /// grant; on one GPU this is a list of that one.
    pub fn ready_device_allocations(&self) -> Vec<(Option<u32>, i64)> {
        self.selected_devices
            .iter()
            .filter_map(|claim| {
                let domain_name = &self.host.devices.get(&claim.id)?.domain;
                let domain = self.host.domains.get(domain_name)?;
                if domain.memory != DomainMemory::Device {
                    return None;
                }
                let bytes = self
                    .resources
                    .ready
                    .allocations
                    .iter()
                    .find(|a| &a.domain == domain_name)?
                    .bytes;
                Some((domain.device.as_deref().and_then(gpu_index), bytes))
            })
            .collect()
    }
}

impl CudaNamespace {
    /// ADR 0035 §4: the variables that narrow the engine child to `list`, in
    /// rank order: `CUDA_VISIBLE_DEVICES` with the UUIDs joined by `,`, or
    /// `CUDA_DEVICE_ORDER=PCI_BUS_ID` and the indices when any is an index.
    /// One namespace gives exactly [`CudaNamespace::environment`].
    pub fn environment_of(list: &[CudaNamespace]) -> Vec<(&'static str, String)> {
        if let [one] = list {
            return one.environment();
        }
        let joined = |text: Vec<String>| text.join(",");
        let indices: Option<Vec<String>> = list
            .iter()
            .map(|namespace| match namespace {
                CudaNamespace::PciIndex(index) => Some(index.to_string()),
                CudaNamespace::Uuid(_) => None,
            })
            .collect();
        match indices {
            Some(indices) if !indices.is_empty() => vec![
                ("CUDA_DEVICE_ORDER", "PCI_BUS_ID".into()),
                ("CUDA_VISIBLE_DEVICES", joined(indices)),
            ],
            _ => vec![(
                "CUDA_VISIBLE_DEVICES",
                joined(
                    list.iter()
                        .map(|namespace| match namespace {
                            CudaNamespace::Uuid(uuid) => uuid.clone(),
                            CudaNamespace::PciIndex(index) => index.to_string(),
                        })
                        .collect(),
                ),
            )],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    fn host(gpus: u32) -> HostPolicy {
        let all: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/f2-deployment.json")).unwrap();
        let mut host = all["host"].clone();
        let mut domains = serde_json::json!({
            "system": {"memory": "distinct", "managed_limit": "200GiB", "free_reserve": "16GiB",
                       "parked_limit": "100GiB", "host_kv_limit": "4GiB"}
        });
        let mut devices = serde_json::json!({});
        for n in 0..gpus {
            let id = format!("gpu{n}");
            domains[&id] = serde_json::json!({"memory": "device", "device": id,
                "managed_limit": "40GiB", "free_reserve": "8GiB", "parked_limit": "4GiB"});
            devices[&id] = serde_json::json!({"domain": id, "sharing": "shared"});
        }
        host["resource_policy"]["domains"] = domains;
        host["resource_policy"]["devices"] = devices;
        normalize_host_policy(&host).unwrap()
    }

    fn claims(n: u32) -> Vec<DeviceClaim> {
        (0..n)
            .map(|i| DeviceClaim {
                id: format!("gpu{i}"),
                sharing: Sharing::Shared,
            })
            .collect()
    }

    // T46 (ADR 0035 §3): a `host_backed` copy on N GPUs is every rank's share
    // x 1.5 in host RAM, beside 4 GiB plus 3 GiB per rank after the first;
    // each GPU parks its own residue. The derivation is built now; resolution
    // refuses the tier while the engine's N > 1 parking gate is closed.
    #[test]
    fn a_host_backed_copy_holds_every_ranks_share() {
        let share = 10 * GIB;
        let footprints = super::super::engine_config::derive_resources(
            12 * GIB,
            Some(20 * GIB),
            Residency::HostBacked,
            &claims(2),
            &host(2),
            Some(share),
            super::super::engine_config::Overheads {
                device: ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES,
                communication: communication_allowance(2),
            },
            Limits::Enforce,
        )
        .unwrap();
        let overhead = 4 * GIB + 3 * GIB;
        let copy = host_backed_copy_bytes(2 * share);
        assert_eq!(copy, 30 * GIB);
        let bytes = |phase: &PhaseFootprint| -> Vec<(String, i64)> {
            phase
                .allocations
                .iter()
                .map(|a| (a.domain.clone(), a.bytes))
                .collect()
        };
        let on_card = 12 * GIB + ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES + GIB;
        assert_eq!(
            bytes(&footprints.ready),
            [
                ("gpu0".to_owned(), on_card),
                ("gpu1".to_owned(), on_card),
                ("system".to_owned(), overhead + copy)
            ]
        );
        // ADR 0035 §§3, 9: each GPU parks its context, graphs and
        // communication buffers, the allowances beside its request.
        let parked_gpu = ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES + GIB;
        assert_eq!(
            bytes(&footprints.parked),
            [
                ("gpu0".to_owned(), parked_gpu),
                ("gpu1".to_owned(), parked_gpu),
                ("system".to_owned(), overhead + copy)
            ]
        );
        assert_eq!(
            bytes(&footprints.cold)[0].1,
            20 * GIB + ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES + GIB
        );
        // The candidate table derives a charge above a card's limit; resolution
        // refuses it with the per-GPU figure.
        let large = |limits| {
            super::super::engine_config::derive_resources(
                45 * GIB,
                None,
                Residency::RestartOnly,
                &claims(2),
                &host(2),
                Some(share),
                super::super::engine_config::Overheads {
                    device: ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES,
                    communication: communication_allowance(2),
                },
                limits,
            )
        };
        assert!(large(Limits::Ignore).is_ok());
        let error = large(Limits::Enforce).unwrap_err();
        assert!(error.detail.starts_with("insufficient_device_memory"));
        assert!(error.detail.contains("on each of its 2 GPUs"));
        assert_eq!(engine_host_overhead(1), 4 * GIB);
        assert_eq!(engine_host_overhead(4), 13 * GIB);
        assert_eq!(communication_allowance(1), 0);
    }
}
