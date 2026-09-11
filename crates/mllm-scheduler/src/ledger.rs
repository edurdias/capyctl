use mllm_domain::OwnerAccountId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainKind {
    System,
    DeviceMemory,
    Filesystem,
    RemoteStorage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Activation,
    Ready,
    Parked,
}

/// Sub-limit tagging on reservations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    HostKv,
    ParkedResidue,
    SharedService,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Domain {
    pub id: String,
    pub kind: DomainKind,
    pub observed_bytes: Option<i64>,
    pub observed_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub owner: OwnerAccountId,
    pub domain: String,
    pub bytes: i64,
    pub phase: Phase,
    pub category: Option<Category>,
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLimits {
    pub managed_limit: i64,
    pub free_reserve: i64,
    pub host_kv_limit: Option<i64>,
    pub parked_limit: Option<i64>,
    pub observation_ttl_secs: u64,
    pub now_unix: i64,
}

/// Per-domain ledger index: domain id -> owner -> that owner's reservations.
pub type DomainOwners =
    std::collections::HashMap<String, std::collections::HashMap<OwnerAccountId, Vec<Reservation>>>;

/// Indexes reservations per domain and per owner (union charging view).
pub fn index_by_domain(reservations: &[Reservation]) -> DomainOwners {
    let mut ledger: DomainOwners = DomainOwners::new();
    for r in reservations {
        ledger
            .entry(r.domain.clone())
            .or_default()
            .entry(r.owner.clone())
            .or_default()
            .push(r.clone());
    }
    ledger
}

/// Total bytes charged on a domain: the union over all owners, including the
/// candidate's own current reservations. Shared-service owners are counted
/// once because F0 enforces unique owner ids. Saturating: hostile inputs
/// must never wrap into a spurious admission grant.
pub fn charged_bytes(ledger: &DomainOwners, domain: &str) -> i64 {
    ledger
        .get(domain)
        .map(|owners| {
            owners
                .values()
                .flatten()
                .fold(0i64, |acc, r| acc.saturating_add(r.bytes))
        })
        .unwrap_or(0)
}
