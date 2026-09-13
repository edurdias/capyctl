use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryObservation {
    pub domain: String,
    pub capacity_bytes: i64,
    pub available_bytes: i64,
    pub sampled_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentFloor {
    pub owner: String,
    pub domain: String,
    pub bytes: i64,
    pub sampled_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLimit {
    pub domain: String,
    pub managed_bytes: i64,
    pub free_reserve_bytes: i64,
    pub host_kv_bytes: Option<i64>,
    pub parked_bytes: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceClaim {
    pub device: String,
    pub sharing: Sharing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub domain: String,
    pub bytes: i64,
    pub host_kv_bytes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcePhase {
    Cold,
    Ready,
    Parking,
    Parked,
    Wake,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseFootprint {
    pub phase: ResourcePhase,
    pub allocations: Vec<Allocation>,
    pub devices: Vec<DeviceClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeFootprints {
    pub cold: PhaseFootprint,
    pub ready: PhaseFootprint,
    pub parking: PhaseFootprint,
    pub parked: PhaseFootprint,
    pub wake: PhaseFootprint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSnapshot {
    pub epoch: u64,
    pub owners: BTreeMap<String, PhaseFootprint>,
}

#[test]
fn phase_names_include_transient_parking() {
    assert_ne!(ResourcePhase::Parking, ResourcePhase::Parked);
    assert_ne!(ResourcePhase::Cold, ResourcePhase::Wake);
}
