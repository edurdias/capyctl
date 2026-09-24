//! SPEC §4.1, §13.3: bootstrap is server-authenticated; enrolled authority is
//! always taken from the verified TLS peer and the durable host registry.
use crate::ownership::SharedCoordinatorState;
use mllm_agent::identity::{csr_key_digest, CertificateAuthority};
use mllm_protocol::pb::{
    bootstrap_server::Bootstrap, host_identity_server::HostIdentity, EnrollRequest, EnrollResponse,
    RenewHostRequest, VerifyHostRequest, VerifyHostResponse,
};
use mllm_store::{
    enrollment::{CertificateRecord, Redemption},
    StoreError,
};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tonic::{Request, Response, Status};

/// Intentionally not Debug: the invitation is a narrowly scoped bearer secret.
pub struct HostInvitation {
    pub id: String,
    pub secret: String,
    pub host_name: String,
    pub expires_unix: i64,
    pub ca_pem: String,
}
pub struct EnrollmentAuthority {
    state: SharedCoordinatorState,
    ca: CertificateAuthority,
    revocations: watch::Sender<u64>,
}
#[derive(Debug, thiserror::Error)]
#[error("host identity authorization failed")]
pub struct EnrollmentError;
/// SPEC §14: why an invitation or a revocation was refused, so the management
/// API answers 400, 404, 409 or 500 instead of one conflict for everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EnrollmentRefusal {
    #[error("invalid enrollment request")]
    Invalid,
    #[error("host not found")]
    NotFound,
    #[error("host name already enrolled")]
    Conflict,
    #[error("enrollment authority failed")]
    Internal,
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn now() -> i64 {
    mllm_protocol::now_unix_ms() / 1000
}
impl EnrollmentAuthority {
    /// Called only after AgentControl binds this report to its verified peer.
    pub fn publish_inventory(&self,host:&str,inventory:&mllm_protocol::pb::ReportInventory)->Result<(),EnrollmentError> {
        crate::host_publication::publish(&self.state,host,inventory).map_err(|_|EnrollmentError)
    }
    /// ADR 0008 (owner decision 2026-09-23): journal an installation drift the
    /// host newly reported in its status.
    pub fn record_installation_drift(&self, host: &str, installation: &str, registered: &str, observed: &str) -> Result<(), EnrollmentError> {
        self.state.lock().map_err(|_| EnrollmentError)?
            .store().record_installation_drift(host, installation, registered, observed)
            .map_err(|_| EnrollmentError)
    }
    /// Owner decision 4 (2026-09-22): the enrolled hosts whose drain still has
    /// an unsettled Stop. `None` when the store cannot be read; callers fail
    /// closed and place nothing on any host then.
    pub fn hosts_with_pending_drain(&self) -> Option<std::collections::BTreeSet<String>> {
        self.state
            .lock()
            .ok()?
            .store()
            .hosts_with_pending_drain()
            .ok()
    }
    pub fn new(state: SharedCoordinatorState, ca: CertificateAuthority) -> Self {
        let (revocations, _) = watch::channel(0);
        Self {
            state,
            ca,
            revocations,
        }
    }
    pub fn controller_id(&self) -> String {
        digest(self.ca.certificate_pem().as_bytes())
    }
    pub fn invite(
        &self,
        name: &str,
        lifetime_seconds: i64,
        now: i64,
    ) -> Result<HostInvitation, EnrollmentRefusal> {
        // SPEC §14: the request's own faults are told apart from a name that
        // is already enrolled and from a failure of this authority.
        if !(1..=3600).contains(&lifetime_seconds) || !mllm_store::enrollment::valid_name(name) {
            return Err(EnrollmentRefusal::Invalid);
        }
        let expiry = now
            .checked_add(lifetime_seconds)
            .ok_or(EnrollmentRefusal::Invalid)?;
        let mut secret = [0_u8; 32];
        OsRng
            .try_fill_bytes(&mut secret)
            .map_err(|_| EnrollmentRefusal::Internal)?;
        let secret = hex::encode(secret);
        let id = digest(secret.as_bytes());
        self.state
            .lock()
            .map_err(|_| EnrollmentRefusal::Internal)?
            .store()
            .create_host_invitation(&id, name, expiry, now)
            .map_err(|error| match error {
                // The inputs were checked above: what remains is the name.
                StoreError::Conflict => EnrollmentRefusal::Conflict,
                _ => EnrollmentRefusal::Internal,
            })?;
        Ok(HostInvitation {
            id,
            secret,
            host_name: name.into(),
            expires_unix: expiry,
            ca_pem: self.ca.certificate_pem().into(),
        })
    }
    /// SPEC §§4.1, 13.3: revoke a host by id or name. The revocation commits
    /// first; every open control stream of the host then rechecks its
    /// certificate and closes, and no command is dispatched to it again
    /// (`AgentSessions::dispatch` reauthorizes the peer per command). Dispatch
    /// to every Ready engine on it closes now; ownership, reservations and
    /// request leases stay, and its engines count as unverified until an
    /// operator stops or drains them with evidence. Idempotent: a retry of a
    /// revoked host answers the same identity with `newly_revoked: false`.
    pub fn revoke(&self, host: &str) -> Result<mllm_store::enrollment::Revocation, EnrollmentRefusal> {
        if !mllm_store::enrollment::valid_name(host) {
            return Err(EnrollmentRefusal::Invalid);
        }
        let revocation = {
            let owner = self.state.lock().map_err(|_| EnrollmentRefusal::Internal)?;
            let revocation = owner.store().revoke_host(host).map_err(|error| match error {
                // A well-formed name that names no enrolled host.
                StoreError::Conflict => EnrollmentRefusal::NotFound,
                _ => EnrollmentRefusal::Internal,
            })?;
            // SPEC §13.3: revocation prevents new work. Close dispatch to the
            // host's Ready engines in the same critical section; nothing is
            // released. A store that cannot list them leaves the readiness
            // supervisor to close them when the session ends.
            if let Ok(launches) = owner.store().remote_ready_launches(owner.session()) {
                for launch in launches.iter().filter(|launch| launch.host_id == revocation.host_id) {
                    let _ = owner
                        .store()
                        .suspend_remote_dispatch(owner.session(), &launch.step_id);
                }
            }
            revocation
        };
        // Committed before notifying streams; their next authorization read is denied.
        self.revocations.send_modify(|v| *v = v.wrapping_add(1));
        Ok(revocation)
    }
    /// Active U4 streams select on this signal, then recheck their own registry
    /// entry. Every new command must also recheck; a cached handshake is insufficient.
    pub fn revocations(&self) -> watch::Receiver<u64> {
        self.revocations.subscribe()
    }
    pub fn authorize_certificate(
        &self,
        peer_der: &[u8],
        claimed_host: &str,
        now: i64,
    ) -> Result<CertificateRecord, EnrollmentError> {
        if peer_der.is_empty() || peer_der.len() > 65536 {
            return Err(EnrollmentError);
        }
        let record = self
            .state
            .lock()
            .map_err(|_| EnrollmentError)?
            .store()
            .certificate_host(&digest(peer_der), now)
            .map_err(|_| EnrollmentError)?;
        if !claimed_host.is_empty() && record.host_id != claimed_host {
            return Err(EnrollmentError);
        }
        Ok(record)
    }
    pub fn authorize_peer<T>(
        &self,
        request: &Request<T>,
        claimed_host: &str,
    ) -> Result<CertificateRecord, EnrollmentError> {
        // peer_certs is supplied by tonic after the configured mTLS handshake;
        // request fields and headers cannot supply or override it.
        let certs = request.peer_certs().ok_or(EnrollmentError)?;
        let leaf = certs.first().ok_or(EnrollmentError)?;
        self.authorize_certificate(leaf.as_ref(), claimed_host, now())
    }
    fn response(&self, record: CertificateRecord) -> EnrollResponse {
        EnrollResponse {
            host_id: record.host_id,
            server_certificate: self.ca.certificate_pem().into(),
            lease_expires_unix: record.expires_unix,
            client_certificate: record.certificate_pem,
            certificate_fingerprint: record.fingerprint,
        }
    }
    fn issue(&self, host: &str, csr: &[u8], now: i64) -> Result<CertificateRecord, StoreError> {
        let issued = self
            .ca
            .issue_host(host, csr, now)
            .map_err(|_| StoreError::Conflict)?;
        Ok(CertificateRecord {
            host_id: host.into(),
            fingerprint: issued.fingerprint,
            certificate_pem: issued.pem,
            expires_unix: issued.expires_unix,
        })
    }
}
fn denied() -> Status {
    Status::permission_denied("host identity authorization failed")
}
#[tonic::async_trait]
impl Bootstrap for EnrollmentAuthority {
    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let request = request.into_inner();
        if request.protocol_version != "1"
            || !request.client_public_key.is_empty()
            || request.invitation_secret.len() != 64
            || !request
                .invitation_secret
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || request.invitation_id != digest(request.invitation_secret.as_bytes())
        {
            return Err(denied());
        }
        let key = csr_key_digest(&request.csr_der).map_err(|_| denied())?;
        let redemption = Redemption {
            invitation_digest: request.invitation_id,
            transaction_id: request.transaction_id,
            host_name: request.host_name,
            key_digest: key,
            csr_digest: digest(&request.csr_der),
        };
        let now = now();
        let result = self
            .state
            .lock()
            .map_err(|_| denied())?
            .store()
            .redeem_host_invitation(&redemption, now, |host| {
                self.issue(host, &request.csr_der, now)
            })
            .map_err(|_| denied())?;
        Ok(Response::new(self.response(result)))
    }
}
#[tonic::async_trait]
impl HostIdentity for EnrollmentAuthority {
    async fn verify(
        &self,
        request: Request<VerifyHostRequest>,
    ) -> Result<Response<VerifyHostResponse>, Status> {
        if request.get_ref().host_id.is_empty() {
            return Err(denied());
        }
        let peer = self
            .authorize_peer(&request, &request.get_ref().host_id)
            .map_err(|_| denied())?;
        Ok(Response::new(VerifyHostResponse {
            host_id: peer.host_id,
        }))
    }
    async fn renew(
        &self,
        request: Request<RenewHostRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let peer = self.authorize_peer(&request, "").map_err(|_| denied())?;
        let request = request.into_inner();
        if request.protocol_version != "1" {
            return Err(denied());
        }
        let key = csr_key_digest(&request.csr_der).map_err(|_| denied())?;
        let now = now();
        let issued = self
            .state
            .lock()
            .map_err(|_| denied())?
            .store()
            .renew_host_certificate(
                &peer.fingerprint,
                &key,
                &request.transaction_id,
                &digest(&request.csr_der),
                now,
                |host| self.issue(host, &request.csr_der, now),
            )
            .map_err(|_| denied())?;
        Ok(Response::new(self.response(issued)))
    }
}
