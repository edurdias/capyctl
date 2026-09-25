use mllm_agent::identity::{CertificateAuthority, HostKey};
use x509_parser::prelude::*;

const NOW: i64 = 1_790_000_000;

// T05: enrollment issues a client identity for the host's own key.
#[test]
fn issued_host_identity_preserves_key_and_limits_authority() {
    let authority = CertificateAuthority::generate(NOW).unwrap();
    let key = HostKey::generate().unwrap();
    let request = key.enrollment_request().unwrap();
    let issued = authority.issue_host("host-a", &request, NOW).unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(issued.pem.as_bytes()).unwrap();
    let (_, cert) = X509Certificate::from_der(&pem.contents).unwrap();
    assert_eq!(cert.public_key().raw, key.public_key_der());
    assert!(!cert.is_ca());
    let usage = cert.extended_key_usage().unwrap().unwrap();
    assert!(usage.value.client_auth);
    assert!(!usage.value.server_auth);
    assert_eq!(cert.validity().not_after.timestamp(), issued.expires_unix);
    assert!(issued.expires_unix > NOW);
    assert!(issued.expires_unix <= NOW + 31 * 24 * 60 * 60);
    assert!(cert.subject().to_string().contains("host-a"));
    let (_, ca_pem) =
        x509_parser::pem::parse_x509_pem(authority.certificate_pem().as_bytes()).unwrap();
    let (_, ca_cert) = X509Certificate::from_der(&ca_pem.contents).unwrap();
    cert.verify_signature(Some(ca_cert.public_key())).unwrap();
}

// T05/T37: proof of possession is checked; CSR extensions are not authority.
#[test]
fn host_cannot_request_ca_or_server_privileges() {
    let authority = CertificateAuthority::generate(NOW).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["controller.example".into()]).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let request = params.serialize_request(&key).unwrap();
    let issued = authority.issue_host("host-b", request.der(), NOW).unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(issued.pem.as_bytes()).unwrap();
    let (_, cert) = X509Certificate::from_der(&pem.contents).unwrap();
    assert!(!cert.is_ca());
    assert!(!cert.key_usage().unwrap().unwrap().value.key_cert_sign());
    assert!(
        !cert
            .extended_key_usage()
            .unwrap()
            .unwrap()
            .value
            .server_auth
    );
    assert!(cert.subject_alternative_name().unwrap().is_none());
    let mut corrupted = request.der().to_vec();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 1;
    assert!(authority.issue_host("host-b", &corrupted, NOW).is_err());
}

// T06: renewal keeps the local key; persisted CA material must match.
#[test]
fn keys_and_authority_can_reload_without_changing_identity() {
    let authority = CertificateAuthority::generate(NOW).unwrap();
    let key = HostKey::generate().unwrap();
    let restored = HostKey::from_pem(&key.private_key_pem()).unwrap();
    assert_eq!(key.public_key_der(), restored.public_key_der());
    let issuer =
        CertificateAuthority::from_pem(authority.certificate_pem(), &authority.private_key_pem())
            .unwrap();
    assert_eq!(issuer.certificate_pem(), authority.certificate_pem());
    assert!(issuer
        .issue_host("host-a", &restored.enrollment_request().unwrap(), NOW + 60)
        .is_ok());
    let other = CertificateAuthority::generate(NOW).unwrap();
    assert!(
        CertificateAuthority::from_pem(authority.certificate_pem(), &other.private_key_pem())
            .is_err()
    );
}

// T05/T37: malformed or oversized input never reaches certificate issuance.
#[test]
fn invalid_enrollment_inputs_are_rejected() {
    let authority = CertificateAuthority::generate(NOW).unwrap();
    let request = HostKey::generate().unwrap().enrollment_request().unwrap();
    for host in ["", "../host", "host\nforged", "a:b", "a b"] {
        assert!(
            authority.issue_host(host, &request, NOW).is_err(),
            "{host:?}"
        );
    }
    assert!(authority.issue_host("host", &vec![0; 16_385], NOW).is_err());
    assert!(authority.issue_host("host", &request, i64::MAX).is_err());
    assert!(authority.issue_host("host", &request, NOW - 600).is_err());
    let mut trailing = request;
    trailing.push(0);
    assert!(authority.issue_host("host", &trailing, NOW).is_err());
}
