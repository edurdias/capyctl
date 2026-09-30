//! Certificate material for SPEC §4.1 enrollment. Persistence and invitation
//! authorization belong to the caller; certificate issuance alone grants neither.
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, PublicKeyData,
};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use x509_parser::prelude::{FromDer, X509Certificate, X509CertificationRequest};

const MAX_KEY_BYTES: usize = 16_384;
const MAX_CSR_BYTES: usize = 16_384;
const MAX_CERT_BYTES: usize = 65_536;
const HOST_LIFETIME_SECONDS: i64 = 30 * 24 * 60 * 60;
const CA_LIFETIME_SECONDS: i64 = 365 * 24 * 60 * 60;
const CLOCK_SKEW_SECONDS: i64 = 300;

#[derive(Debug, thiserror::Error)]
#[error("invalid certificate identity material")]
pub struct IdentityError;

/// Intentionally not Debug or Serialize: callers must explicitly persist the key
/// in owner-only storage, never include it in normal reports or configuration.
pub struct HostKey(KeyPair);

impl HostKey {
    pub fn generate() -> Result<Self, IdentityError> {
        KeyPair::generate().map(Self).map_err(|_| IdentityError)
    }

    pub fn from_pem(pem: &str) -> Result<Self, IdentityError> {
        if pem.len() > MAX_KEY_BYTES {
            return Err(IdentityError);
        }
        KeyPair::from_pem(pem).map(Self).map_err(|_| IdentityError)
    }

    pub fn private_key_pem(&self) -> String {
        self.0.serialize_pem()
    }

    pub fn public_key_der(&self) -> Vec<u8> {
        self.0.subject_public_key_info()
    }

    /// SPEC §4.1: the private key stays on the enrolling host. This signed CSR
    /// proves possession; its requested names and permissions are not trusted.
    pub fn enrollment_request(&self) -> Result<Vec<u8>, IdentityError> {
        CertificateParams::default()
            .serialize_request(&self.0)
            .map(|csr| csr.der().to_vec())
            .map_err(|_| IdentityError)
    }
}

pub struct IssuedCertificate {
    pub pem: String,
    pub fingerprint: String,
    pub expires_unix: i64,
}

/// A private controller CA. Loading rejects a mismatched key, non-CA material,
/// or an invalid root signature rather than creating unusable host credentials.
pub struct CertificateAuthority {
    issuer: Issuer<'static, KeyPair>,
    certificate: String,
    not_before: i64,
    not_after: i64,
}

impl CertificateAuthority {
    pub fn generate(now_unix: i64) -> Result<Self, IdentityError> {
        let key = KeyPair::generate().map_err(|_| IdentityError)?;
        let mut params = parameters("capyctl controller CA", now_unix, CA_LIFETIME_SECONDS)?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let certificate = params.self_signed(&key).map_err(|_| IdentityError)?.pem();
        Ok(Self {
            not_before: params.not_before.unix_timestamp(),
            not_after: params.not_after.unix_timestamp(),
            issuer: Issuer::new(params, key),
            certificate,
        })
    }

    pub fn from_pem(certificate: &str, private_key: &str) -> Result<Self, IdentityError> {
        if certificate.len() > MAX_CERT_BYTES || private_key.len() > MAX_KEY_BYTES {
            return Err(IdentityError);
        }
        let key = KeyPair::from_pem(private_key).map_err(|_| IdentityError)?;
        let (remaining, pem) =
            x509_parser::pem::parse_x509_pem(certificate.as_bytes()).map_err(|_| IdentityError)?;
        let (der_remaining, cert) =
            X509Certificate::from_der(&pem.contents).map_err(|_| IdentityError)?;
        if remaining.iter().any(|b| !b.is_ascii_whitespace())
            || !der_remaining.is_empty()
            || pem.label != "CERTIFICATE"
            || !cert.is_ca()
            || cert.subject() != cert.issuer()
            || cert.public_key().raw != key.subject_public_key_info()
            || !cert
                .key_usage()
                .map_err(|_| IdentityError)?
                .is_some_and(|usage| usage.value.key_cert_sign())
            || cert.verify_signature(None).is_err()
        {
            return Err(IdentityError);
        }
        let not_before = cert.validity().not_before.timestamp();
        let not_after = cert.validity().not_after.timestamp();
        let issuer = Issuer::from_ca_cert_pem(certificate, key).map_err(|_| IdentityError)?;
        Ok(Self {
            issuer,
            certificate: certificate.into(),
            not_before,
            not_after,
        })
    }

    pub fn certificate_pem(&self) -> &str {
        &self.certificate
    }

    pub fn private_key_pem(&self) -> String {
        self.issuer.key().serialize_pem()
    }

    /// SPEC §4.1, §13.3: only the registry-assigned host identity is certified.
    /// Do not accept CA, SAN, or server-auth privileges from the host's CSR.
    pub fn issue_host(
        &self,
        host_id: &str,
        csr_der: &[u8],
        now_unix: i64,
    ) -> Result<IssuedCertificate, IdentityError> {
        if host_id.is_empty()
            || host_id.len() > 128
            || !host_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            || csr_der.is_empty()
            || csr_der.len() > MAX_CSR_BYTES
            || now_unix < self.not_before
            || now_unix >= self.not_after
        {
            return Err(IdentityError);
        }
        let (remaining, _) =
            X509CertificationRequest::from_der(csr_der).map_err(|_| IdentityError)?;
        if !remaining.is_empty() {
            return Err(IdentityError);
        }
        let request = CertificateSigningRequestParams::from_der(&csr_der.into())
            .map_err(|_| IdentityError)?;
        let mut params = parameters(host_id, now_unix, HOST_LIFETIME_SECONDS)?;
        params.not_after = params
            .not_after
            .min(OffsetDateTime::from_unix_timestamp(self.not_after).map_err(|_| IdentityError)?);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        // Include issuance time and host in the serial; renewing the same key must
        // not inherit the old certificate's serial merely because SPKI is stable.
        let mut serial = Sha256::new();
        serial.update(host_id.as_bytes());
        serial.update(now_unix.to_be_bytes());
        serial.update(csr_der);
        let mut serial = serial.finalize()[..20].to_vec();
        serial[0] &= 0x7f;
        params.serial_number = Some(serial.into());
        let certificate = params
            .signed_by(&request.public_key, &self.issuer)
            .map_err(|_| IdentityError)?;
        Ok(IssuedCertificate {
            pem: certificate.pem(),
            fingerprint: hex::encode(Sha256::digest(certificate.der())),
            expires_unix: params.not_after.unix_timestamp(),
        })
    }
}

fn parameters(name: &str, now: i64, lifetime: i64) -> Result<CertificateParams, IdentityError> {
    if now < 0 {
        return Err(IdentityError);
    }
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, name);
    let mut params = CertificateParams::default();
    params.not_before = OffsetDateTime::from_unix_timestamp(
        now.checked_sub(CLOCK_SKEW_SECONDS).ok_or(IdentityError)?,
    )
    .map_err(|_| IdentityError)?;
    params.not_after =
        OffsetDateTime::from_unix_timestamp(now.checked_add(lifetime).ok_or(IdentityError)?)
            .map_err(|_| IdentityError)?;
    params.distinguished_name = distinguished_name;
    Ok(params)
}

/// Verify proof of possession before the enrollment registry is consulted.
pub fn csr_key_digest(csr_der: &[u8]) -> Result<String, IdentityError> {
    if csr_der.is_empty() || csr_der.len() > MAX_CSR_BYTES {
        return Err(IdentityError);
    }
    let (remaining, request) =
        X509CertificationRequest::from_der(csr_der).map_err(|_| IdentityError)?;
    if !remaining.is_empty() || request.verify_signature().is_err() {
        return Err(IdentityError);
    }
    Ok(hex::encode(Sha256::digest(
        request.certification_request_info.subject_pki.raw,
    )))
}
impl CertificateAuthority {
    /// SPEC §4.1: server-authenticated bootstrap never reuses a host credential.
    pub fn issue_server(
        &self,
        dns_name: &str,
        key: &HostKey,
        now: i64,
    ) -> Result<IssuedCertificate, IdentityError> {
        if dns_name.is_empty()
            || dns_name.len() > 253
            || !dns_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            || now < self.not_before
            || now >= self.not_after
        {
            return Err(IdentityError);
        }
        let mut params = parameters(dns_name, now, HOST_LIFETIME_SECONDS)?;
        params.subject_alt_names = CertificateParams::new(vec![dns_name.to_owned()])
            .map_err(|_| IdentityError)?
            .subject_alt_names;
        params.not_after = params
            .not_after
            .min(OffsetDateTime::from_unix_timestamp(self.not_after).map_err(|_| IdentityError)?);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params
            .signed_by(&key.0, &self.issuer)
            .map_err(|_| IdentityError)?;
        Ok(IssuedCertificate {
            pem: certificate.pem(),
            fingerprint: hex::encode(Sha256::digest(certificate.der())),
            expires_unix: params.not_after.unix_timestamp(),
        })
    }
}
