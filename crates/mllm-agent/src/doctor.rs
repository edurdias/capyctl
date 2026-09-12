//! doctor host: approved non-destructive checks (SPEC §4.2) — profile
//! presence, build fingerprints, and memory-domain observation. Destructive
//! park/restore qualification is an explicit operation, never part of
//! doctor. The live recipe is frozen only from reported reality (F1
//! design §8 step 2).

use mllm_scheduler::DomainKind;

#[derive(Debug, Clone)]
pub struct ProfileInput {
    pub name: String,
    /// The configured launch command (program + argument prefix).
    pub command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileReport {
    pub name: String,
    pub command_exists: bool,
    /// Stable fingerprint over the tool's own version output; redacted of
    /// any `--api-key <secret>` pattern (SPEC §8.2/§13.3). Always true for
    /// doctor: it never records secrets.
    pub build_fingerprint: Option<String>,
    pub fingerprint_redacted: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainObservation {
    pub kind: DomainKind,
    pub label: Option<String>,
    pub observed_bytes: i64,
    pub observed_at_unix: i64,
}

#[derive(Debug, Clone, Default)]
pub struct DoctorReport {
    pub profiles: Vec<ProfileReport>,
    pub domains: Vec<DomainObservation>,
}

#[derive(Debug, thiserror::Error)]
pub enum DoctorError {
    #[error("doctor failed: {0}")]
    Io(String),
}

/// Redact any `--api-key <value>` occurrence (SPEC §8.2/§13.3). Doctor
/// never records secrets, and the rule is unconditional.
pub fn redact_api_keys(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(pos) = rest.find("--api-key ") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + "--api-key ".len()..];
        let secret_end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        out.push_str("--api-key <redacted>");
        rest = &tail[secret_end..];
    }
    out.push_str(rest);
    out
}

fn meminfo_total_bytes() -> Option<i64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: i64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}


pub fn doctor_host(profiles: &[ProfileInput]) -> Result<DoctorReport, DoctorError> {
    let mut report = DoctorReport::default();

    for p in profiles {
        let exists = std::path::Path::new(&p.command[0]).exists();
        let (note, fingerprint) = if !exists {
            (format!("command {} not found", p.command[0]), None)
        } else {
            // Approved non-destructive check: `--version` capture only. The
            // program is never launched as an engine here.
            match std::process::Command::new(&p.command[0]).arg("--version").output() {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                    let combined = if stdout.trim().is_empty() { stderr } else { stdout };
                    (
                        String::new(),
                        Some(format!(
                            "{}:{}",
                            {
                                use sha2::{Digest, Sha256};
                                hex::encode(Sha256::digest(
                                    redact_api_keys(&combined).as_bytes(),
                                ))
                            },
                            redact_api_keys(&combined)
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(40)
                                .collect::<String>()
                        )),
                    )
                }
                Ok(o) => (format!("exited status {}", o.status), None),
                Err(e) => (format!("launch failed: {e}"), None),
            }
        };
        report.profiles.push(ProfileReport {
            name: p.name.clone(),
            command_exists: exists,
            build_fingerprint: fingerprint,
            fingerprint_redacted: true,
            note,
        });
    }

    if let Some(bytes) = meminfo_total_bytes() {
        report.domains.push(DomainObservation {
            kind: DomainKind::System,
            label: None,
            observed_bytes: bytes,
            observed_at_unix: now_unix(),
        });
    }

    Ok(report)
}