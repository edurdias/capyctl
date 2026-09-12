//! doctor host: profile fingerprint capture and memory-domain observation
//! (F1 design §8 step 2 — the live recipe is frozen only from reported
//! reality). Approved non-destructive checks only (SPEC §4.2).

use mllm_agent::doctor::{doctor_host, DoctorReport, ProfileInput};

#[test]
fn doctor_reports_profile_presence_and_fingerprint() {
    // `/bin/echo --version` exists and prints to stdout: fingerprintable.
    let report = doctor_host(&[ProfileInput {
        name: "echo-profile".into(),
        command: vec!["/bin/echo".into(), "--version".into()],
    }])
    .unwrap();
    assert_eq!(report.profiles.len(), 1);
    let p = &report.profiles[0];
    assert_eq!(p.name, "echo-profile");
    assert!(p.command_exists);
    assert!(p.build_fingerprint.is_some(), "echo output becomes the fingerprint");
    assert!(p.fingerprint_redacted, "no secrets in fingerprints");
}

#[test]
fn doctor_reports_missing_profile_without_launching() {
    let report = doctor_host(&[ProfileInput {
        name: "missing-vllm".into(),
        command: vec!["/nonexistent/vllm-binary".into()],
    }])
    .unwrap();
    let p = &report.profiles[0];
    assert!(!p.command_exists);
    assert!(p.build_fingerprint.is_none());
    assert!(p.note.contains("not found"));
}

#[test]
fn doctor_observes_system_memory_domain() {
    let report = doctor_host(&[]).unwrap();
    let sys = report
        .domains
        .iter()
        .find(|d| d.kind == mllm_scheduler::DomainKind::System)
        .expect("system domain observed from /proc/meminfo");
    assert!(sys.observed_bytes > 0);
    assert!(sys.observed_at_unix > 0);
}

#[test]
fn doctor_is_idempotent_and_non_destructive() {
    let profiles = [ProfileInput {
        name: "p".into(),
        command: vec!["/bin/echo".into(), "--version".into()],
    }];
    let a = doctor_host(&profiles).unwrap();
    let b = doctor_host(&profiles).unwrap();
    assert_eq!(
        a.profiles[0].build_fingerprint, b.profiles[0].build_fingerprint,
        "fingerprint stable across runs"
    );
}

// Silence unused-import warning for the report type used above.
#[allow(dead_code)]
fn _type_check(r: DoctorReport) -> DoctorReport {
    r
}