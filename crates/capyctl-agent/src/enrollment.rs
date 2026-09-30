//! SPEC §4.1: persist the enrollment transaction and private key before any
//! bootstrap RPC. Trust verification precedes transmission of the invitation.
use crate::{
    identity::{csr_key_digest, CertificateAuthority, HostKey, IdentityError},
    identity_storage::IdentityDirectory,
};
use capyctl_protocol::pb::{
    bootstrap_client::BootstrapClient, host_identity_client::HostIdentityClient, EnrollRequest,
    EnrollResponse, RenewHostRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};
use x509_parser::prelude::{FromDer, X509Certificate};
const CA_FILE: &str = "controller-ca.json";
/// The owner-only file holding the enrolled host's identity and key.
pub const HOST_FILE: &str = "host-identity.json";
#[derive(Debug, thiserror::Error)]
#[error("identity enrollment failed")]
pub struct EnrollmentError;
impl From<IdentityError> for EnrollmentError {
    fn from(_: IdentityError) -> Self {
        Self
    }
}
/// Join files are private bearer credentials: deliberately no Debug implementation.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinInvitation {
    pub version: u32,
    pub server_address: String,
    pub control_address: String,
    pub server_ca: String,
    pub invitation_id: String,
    pub invitation_secret: String,
    pub host_name: String,
    pub expires_unix: i64,
    /// ADR 0016: present only on a recovery invitation, naming the revoked
    /// host id it re-enrolls. An ordinary invitation omits it, so its file is
    /// exactly as before; an agent that predates recovery refuses a recovery
    /// invitation instead of enrolling a new host with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recover_host_id: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaBundle {
    version: u32,
    certificate: String,
    private_key: String,
}
pub fn initialize_controller_ca(
    storage: &IdentityDirectory,
    now: i64,
) -> Result<CertificateAuthority, EnrollmentError> {
    let ca = CertificateAuthority::generate(now)?;
    let bytes = serde_json::to_vec(&CaBundle {
        version: 1,
        certificate: ca.certificate_pem().into(),
        private_key: ca.private_key_pem(),
    })
    .map_err(|_| EnrollmentError)?;
    storage
        .create_bundle(CA_FILE, &bytes)
        .map_err(|_| EnrollmentError)?;
    Ok(ca)
}
pub fn load_controller_ca(
    storage: &IdentityDirectory,
) -> Result<CertificateAuthority, EnrollmentError> {
    let bytes = storage
        .read_bundle(CA_FILE)
        .map_err(|_| EnrollmentError)?
        .ok_or(EnrollmentError)?;
    let bundle: CaBundle = serde_json::from_slice(&bytes).map_err(|_| EnrollmentError)?;
    if bundle.version != 1 {
        return Err(EnrollmentError);
    }
    Ok(CertificateAuthority::from_pem(
        &bundle.certificate,
        &bundle.private_key,
    )?)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostBundle {
    version: u32,
    server_address: String,
    control_address: String,
    server_ca: String,
    invitation_id: String,
    host_name: String,
    transaction_id: String,
    private_key: String,
    csr_der: Vec<u8>,
    issued: Option<HostCertificate>,
    #[serde(default)]
    renewal_transaction: Option<String>,
    /// ADR 0016: the host id a recovery enrollment must be issued for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recover_host_id: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostCertificate {
    host_id: String,
    certificate: String,
    fingerprint: String,
    expires_unix: i64,
}
pub struct PendingEnrollment {
    bundle: HostBundle,
    persisted_digest: String,
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}
fn endpoint(address: &str, ca: &str) -> Result<Endpoint, EnrollmentError> {
    if address.len() > 2048 || ca.len() > 65536 || address.contains('@') {
        return Err(EnrollmentError);
    }
    let endpoint = Endpoint::from_shared(address.to_owned()).map_err(|_| EnrollmentError)?;
    if endpoint.uri().scheme_str() != Some("https")
        || endpoint.uri().host().is_none()
        || endpoint
            .uri()
            .path_and_query()
            .is_some_and(|p| p.as_str() != "/")
    {
        return Err(EnrollmentError);
    }
    validate_ca(ca)?;
    endpoint
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15))
        .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(ca)))
        .map_err(|_| EnrollmentError)
}
fn validate_ca(ca: &str) -> Result<(), EnrollmentError> {
    let (remaining, pem) =
        x509_parser::pem::parse_x509_pem(ca.as_bytes()).map_err(|_| EnrollmentError)?;
    let (der_remaining, cert) =
        X509Certificate::from_der(&pem.contents).map_err(|_| EnrollmentError)?;
    if pem.label != "CERTIFICATE"
        || remaining.iter().any(|b| !b.is_ascii_whitespace())
        || !der_remaining.is_empty()
        || !cert.is_ca()
        || cert.subject() != cert.issuer()
        || cert.verify_signature(None).is_err()
    {
        return Err(EnrollmentError);
    }
    Ok(())
}
fn valid_invitation(invitation: &JoinInvitation) -> Result<(), EnrollmentError> {
    if invitation.version != 1
        || !name(&invitation.host_name)
        || invitation.invitation_secret.len() != 64
        || !invitation
            .invitation_secret
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
        || invitation.invitation_id != digest(invitation.invitation_secret.as_bytes())
        || invitation
            .recover_host_id
            .as_deref()
            .is_some_and(|id| !name(id))
    {
        return Err(EnrollmentError);
    }
    endpoint(&invitation.server_address, &invitation.server_ca)?;
    endpoint(&invitation.control_address, &invitation.server_ca)?;
    Ok(())
}
fn fresh_bundle(
    invitation: &JoinInvitation,
    recover_host_id: Option<String>,
) -> Result<HostBundle, EnrollmentError> {
    let key = HostKey::generate()?;
    // Random key material provides an unguessable, stable transaction ID.
    let transaction_id = digest(&HostKey::generate()?.public_key_der());
    Ok(HostBundle {
        version: 1,
        server_address: invitation.server_address.clone(),
        control_address: invitation.control_address.clone(),
        server_ca: invitation.server_ca.clone(),
        invitation_id: invitation.invitation_id.clone(),
        host_name: invitation.host_name.clone(),
        transaction_id,
        private_key: key.private_key_pem(),
        csr_der: key.enrollment_request()?,
        issued: None,
        renewal_transaction: None,
        recover_host_id,
    })
}
impl PendingEnrollment {
    /// ADR 0016 (owner decision 2026-09-24): prepare the re-enrollment of a
    /// revoked host under its same host id, from a recovery invitation.
    ///
    /// A new key is always generated: the revoked certificate's key is never
    /// reused. The retained identity is replaced only when it belongs to the
    /// same controller and names the same host id (or is absent, when the
    /// identity files were lost); anything else is refused, never adopted.
    /// The bundle is persisted before any RPC, so a retry with the same
    /// invitation resumes the same enrollment transaction. The host journal
    /// lives beside the identity and is not touched here.
    pub fn prepare_recovery(
        storage: &IdentityDirectory,
        invitation: &JoinInvitation,
    ) -> Result<Self, EnrollmentError> {
        valid_invitation(invitation)?;
        let host = invitation.recover_host_id.clone().ok_or(EnrollmentError)?;
        let existing = storage
            .read_bundle(HOST_FILE)
            .map_err(|_| EnrollmentError)?;
        let bytes = match existing {
            None => {
                let bytes = serde_json::to_vec(&fresh_bundle(invitation, Some(host))?)
                    .map_err(|_| EnrollmentError)?;
                storage
                    .create_bundle(HOST_FILE, &bytes)
                    .map_err(|_| EnrollmentError)?;
                bytes
            }
            Some(bytes) => {
                let retained = Self::decode(&bytes)?;
                if retained.bundle.invitation_id == invitation.invitation_id {
                    // A retry of this very recovery: same key, same transaction.
                    bytes
                } else {
                    let same_host = retained
                        .bundle
                        .issued
                        .as_ref()
                        .map(|c| c.host_id.as_str())
                        .or(retained.bundle.recover_host_id.as_deref())
                        == Some(host.as_str());
                    if !same_host
                        || retained.bundle.server_ca != invitation.server_ca
                        || retained.bundle.host_name != invitation.host_name
                    {
                        return Err(EnrollmentError);
                    }
                    let replacement = serde_json::to_vec(&fresh_bundle(invitation, Some(host))?)
                        .map_err(|_| EnrollmentError)?;
                    storage
                        .replace_bundle(HOST_FILE, &retained.persisted_digest, &replacement)
                        .map_err(|_| EnrollmentError)?;
                    replacement
                }
            }
        };
        let pending = Self::decode(&bytes)?;
        if pending.bundle.recover_host_id.as_deref() != invitation.recover_host_id.as_deref() {
            return Err(EnrollmentError);
        }
        pending.request(invitation)?;
        Ok(pending)
    }
    pub fn prepare(
        storage: &IdentityDirectory,
        invitation: &JoinInvitation,
    ) -> Result<Self, EnrollmentError> {
        // ADR 0016: a recovery invitation re-enrolls an existing identity and
        // is prepared only by `prepare_recovery`.
        if invitation.recover_host_id.is_some() {
            return Err(EnrollmentError);
        }
        if invitation.version != 1
            || !name(&invitation.host_name)
            || invitation.invitation_secret.len() != 64
            || !invitation
                .invitation_secret
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || invitation.invitation_id != digest(invitation.invitation_secret.as_bytes())
        {
            return Err(EnrollmentError);
        }
        endpoint(&invitation.server_address, &invitation.server_ca)?;
        endpoint(&invitation.control_address, &invitation.server_ca)?;
        let bytes = match storage
            .read_bundle(HOST_FILE)
            .map_err(|_| EnrollmentError)?
        {
            Some(bytes) => bytes,
            None => {
                let key = HostKey::generate()?;
                // Random key material provides an unguessable, stable transaction ID.
                let transaction_id = digest(&HostKey::generate()?.public_key_der());
                let bundle = HostBundle {
                    version: 1,
                    server_address: invitation.server_address.clone(),
                    control_address: invitation.control_address.clone(),
                    server_ca: invitation.server_ca.clone(),
                    invitation_id: invitation.invitation_id.clone(),
                    host_name: invitation.host_name.clone(),
                    transaction_id,
                    private_key: key.private_key_pem(),
                    csr_der: key.enrollment_request()?,
                    issued: None,
                    renewal_transaction: None,
                    recover_host_id: None,
                };
                let bytes = serde_json::to_vec(&bundle).map_err(|_| EnrollmentError)?;
                storage
                    .create_bundle(HOST_FILE, &bytes)
                    .map_err(|_| EnrollmentError)?;
                bytes
            }
        };
        let pending = Self::decode(&bytes)?;
        pending.request(invitation)?;
        Ok(pending)
    }
    fn decode(bytes: &[u8]) -> Result<Self, EnrollmentError> {
        let bundle: HostBundle = serde_json::from_slice(bytes).map_err(|_| EnrollmentError)?;
        if bundle.version != 1 || !name(&bundle.host_name) || !name(&bundle.transaction_id) {
            return Err(EnrollmentError);
        }
        endpoint(&bundle.server_address, &bundle.server_ca)?;
        endpoint(&bundle.control_address, &bundle.server_ca)?;
        let key = HostKey::from_pem(&bundle.private_key)?;
        if csr_key_digest(&bundle.csr_der)? != digest(&key.public_key_der()) {
            return Err(EnrollmentError);
        }
        if let Some(cert) = &bundle.issued {
            validate_host_certificate(cert, &bundle.server_ca, &key, 0)?;
        }
        Ok(Self {
            bundle,
            persisted_digest: digest(bytes),
        })
    }
    pub fn load(storage: &IdentityDirectory) -> Result<Self, EnrollmentError> {
        Self::decode(
            &storage
                .read_bundle(HOST_FILE)
                .map_err(|_| EnrollmentError)?
                .ok_or(EnrollmentError)?,
        )
    }
    pub fn request(&self, invitation: &JoinInvitation) -> Result<EnrollRequest, EnrollmentError> {
        if self.bundle.invitation_id != invitation.invitation_id
            || self.bundle.host_name != invitation.host_name
            || self.bundle.control_address != invitation.control_address
            || self.bundle.server_address != invitation.server_address
            || self.bundle.server_ca != invitation.server_ca
            || invitation.version != 1
            || digest(invitation.invitation_secret.as_bytes()) != invitation.invitation_id
            || self.bundle.recover_host_id != invitation.recover_host_id
        {
            return Err(EnrollmentError);
        }
        Ok(EnrollRequest {
            invitation_id: invitation.invitation_id.clone(),
            invitation_secret: invitation.invitation_secret.clone(),
            host_name: invitation.host_name.clone(),
            client_public_key: Vec::new(),
            transaction_id: self.bundle.transaction_id.clone(),
            csr_der: self.bundle.csr_der.clone(),
            protocol_version: "1".into(),
        })
    }
    /// Connecting completes authenticated TLS before request bytes are sent.
    pub async fn enroll(
        &mut self,
        storage: &IdentityDirectory,
        invitation: &JoinInvitation,
        now: i64,
    ) -> Result<String, EnrollmentError> {
        let request = self.request(invitation)?;
        let channel = endpoint(&self.bundle.server_address, &self.bundle.server_ca)?
            .connect()
            .await
            .map_err(|_| EnrollmentError)?;
        let response = BootstrapClient::new(channel)
            .max_decoding_message_size(131072)
            .max_encoding_message_size(32768)
            .enroll(request)
            .await
            .map_err(|_| EnrollmentError)?
            .into_inner();
        self.accept_certificate(storage, response, now)
    }
    pub fn accept_certificate(
        &mut self,
        storage: &IdentityDirectory,
        response: EnrollResponse,
        now: i64,
    ) -> Result<String, EnrollmentError> {
        if response.server_certificate != self.bundle.server_ca {
            return Err(EnrollmentError);
        }
        let certificate = HostCertificate {
            host_id: response.host_id,
            certificate: response.client_certificate,
            fingerprint: response.certificate_fingerprint,
            expires_unix: response.lease_expires_unix,
        };
        if self
            .bundle
            .issued
            .as_ref()
            .is_some_and(|old| old.host_id != certificate.host_id)
            // ADR 0016: a recovery is issued for exactly the host it names.
            || self
                .bundle
                .recover_host_id
                .as_ref()
                .is_some_and(|host| *host != certificate.host_id)
        {
            return Err(EnrollmentError);
        }
        validate_host_certificate(
            &certificate,
            &self.bundle.server_ca,
            &HostKey::from_pem(&self.bundle.private_key)?,
            now,
        )?;
        let old = self.bundle.issued.replace(certificate);
        let old_renewal = self.bundle.renewal_transaction.take();
        let bytes = serde_json::to_vec(&self.bundle).map_err(|_| EnrollmentError)?;
        if storage
            .replace_bundle(HOST_FILE, &self.persisted_digest, &bytes)
            .is_err()
        {
            self.bundle.issued = old;
            self.bundle.renewal_transaction = old_renewal;
            return Err(EnrollmentError);
        }
        self.persisted_digest = digest(&bytes);
        Ok(self
            .bundle
            .issued
            .as_ref()
            .ok_or(EnrollmentError)?
            .host_id
            .clone())
    }
    /// Persist renewal identity before sending: a lost reply recovers exactly the
    /// same issued certificate, including after the host process restarts.
    pub async fn renew(
        &mut self,
        storage: &IdentityDirectory,
        control_address: &str,
        now: i64,
    ) -> Result<(), EnrollmentError> {
        let endpoint = endpoint(control_address, &self.bundle.server_ca)?
            .tls_config(self.tls_config(now)?)
            .map_err(|_| EnrollmentError)?;
        if self.bundle.renewal_transaction.is_none() {
            self.bundle.renewal_transaction = Some(digest(&HostKey::generate()?.public_key_der()));
            let bytes = serde_json::to_vec(&self.bundle).map_err(|_| EnrollmentError)?;
            if storage
                .replace_bundle(HOST_FILE, &self.persisted_digest, &bytes)
                .is_err()
            {
                self.bundle.renewal_transaction = None;
                return Err(EnrollmentError);
            }
            self.persisted_digest = digest(&bytes);
        }
        let request = RenewHostRequest {
            csr_der: self.bundle.csr_der.clone(),
            protocol_version: "1".into(),
            transaction_id: self
                .bundle
                .renewal_transaction
                .clone()
                .ok_or(EnrollmentError)?,
        };
        let channel = endpoint.connect().await.map_err(|_| EnrollmentError)?;
        let response = HostIdentityClient::new(channel)
            .max_decoding_message_size(131072)
            .max_encoding_message_size(32768)
            .renew(request)
            .await
            .map_err(|_| EnrollmentError)?
            .into_inner();
        self.accept_certificate(storage, response, now)?;
        Ok(())
    }
    pub fn control_endpoint(&self, now: i64) -> Result<Endpoint, EnrollmentError> {
        endpoint(&self.bundle.control_address, &self.bundle.server_ca)?
            .http2_keep_alive_interval(Duration::from_secs(10))
            .keep_alive_timeout(Duration::from_secs(5))
            .keep_alive_while_idle(true)
            .tls_config(self.tls_config(now)?)
            .map_err(|_| EnrollmentError)
    }
    pub fn controller_id(&self) -> String {
        digest(self.bundle.server_ca.as_bytes())
    }
    pub fn host_id(&self) -> Option<&str> {
        self.bundle.issued.as_ref().map(|c| c.host_id.as_str())
    }
    pub fn tls_config(&self, now: i64) -> Result<ClientTlsConfig, EnrollmentError> {
        let cert = self.bundle.issued.as_ref().ok_or(EnrollmentError)?;
        validate_host_certificate(
            cert,
            &self.bundle.server_ca,
            &HostKey::from_pem(&self.bundle.private_key)?,
            now,
        )?;
        Ok(ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.bundle.server_ca))
            .identity(Identity::from_pem(
                &cert.certificate,
                &self.bundle.private_key,
            )))
    }
}
fn validate_host_certificate(
    host: &HostCertificate,
    ca_pem: &str,
    key: &HostKey,
    now: i64,
) -> Result<(), EnrollmentError> {
    if host.certificate.len() > 65536 || !name(&host.host_id) {
        return Err(EnrollmentError);
    }
    let (_, ca_pem) =
        x509_parser::pem::parse_x509_pem(ca_pem.as_bytes()).map_err(|_| EnrollmentError)?;
    let (_, ca) = X509Certificate::from_der(&ca_pem.contents).map_err(|_| EnrollmentError)?;
    let (remaining, pem) = x509_parser::pem::parse_x509_pem(host.certificate.as_bytes())
        .map_err(|_| EnrollmentError)?;
    let (der_remaining, cert) =
        X509Certificate::from_der(&pem.contents).map_err(|_| EnrollmentError)?;
    let eku = cert
        .extended_key_usage()
        .map_err(|_| EnrollmentError)?
        .ok_or(EnrollmentError)?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok());
    if remaining.iter().any(|b| !b.is_ascii_whitespace())
        || !der_remaining.is_empty()
        || pem.label != "CERTIFICATE"
        || cert.is_ca()
        || !eku.value.client_auth
        || eku.value.server_auth
        || eku.value.any
        || cn != Some(host.host_id.as_str())
        || cert.issuer() != ca.subject()
        || cert.public_key().raw != key.public_key_der()
        || cert.verify_signature(Some(ca.public_key())).is_err()
        || digest(&pem.contents) != host.fingerprint
        || cert.validity().not_after.timestamp() != host.expires_unix
        || now >= host.expires_unix
        || (now != 0 && now < cert.validity().not_before.timestamp())
    {
        return Err(EnrollmentError);
    }
    Ok(())
}
