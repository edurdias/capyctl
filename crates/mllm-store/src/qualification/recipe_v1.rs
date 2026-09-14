//! Code-owned Fake corpus and recipe preflight. Native collection is unsupported.
use crate::lifecycle::LifecycleError;
use mllm_config::effective::candidate::{
    CandidateCaseKind, CandidateLaunch, CandidateReviewedSnapshot,
};
use mllm_domain::qualification::{CandidateResponseObservation, CandidateSecurityEndpoint};
use sha2::{Digest, Sha256};

/// Exact UTF-8 bytes hashed by the installed Fake program; no trailing newline.
pub const CORPUS: &str = r#"[{"prompt":"Repeat exactly: MLLM_ALPHA_71","expected":"MLLM_ALPHA_71"},{"prompt":"Repeat exactly: MLLM_BETA_29","expected":"MLLM_BETA_29"}]"#;
pub const PROGRAM_REVISION: &str = "qualification-fake-v1";
const REQUEST_OUTPUT_TOKENS: u32 = 16;
const READY_PROMPT: &str = "Repeat exactly: MLLM_READY_13";

fn fixed_request(deployment: &str, content: &str, stream: bool) -> String {
    serde_json::json!({"model":format!("candidate-{deployment}"),"messages":[{"role":"user","content":content}],"temperature":0,"max_tokens":REQUEST_OUTPUT_TOKENS,"stream":stream}).to_string()
}

pub(crate) fn ready_request(deployment: &str) -> String {
    fixed_request(deployment, READY_PROMPT, false)
}

/// Constructed only after the store validates committed source records. It is
/// deliberately neither deserializable nor an externally supplied pass predicate.
pub(crate) struct SuiteCaseEvidence {
    pub id: String,
    pub kind: CandidateCaseKind,
    pub cycle: u32,
    pub requests: u32,
    pub references: Vec<String>,
}

/// Pure full-suite evaluator. Ordered cases and their exact required cardinality
/// come from the frozen program, never from a stored success flag.
pub(crate) fn evaluate_suite(
    snapshot: &CandidateReviewedSnapshot,
    evidence: &[SuiteCaseEvidence],
    requests_used: u32,
) -> Result<(), LifecycleError> {
    let program = QualificationProgram::resolve(snapshot)?;
    if evidence.len() != snapshot.cases().len() || requests_used != program.required_requests {
        return Err(LifecycleError::Conflict);
    }
    let mut unique = std::collections::BTreeSet::new();
    let mut requests = 0;
    for (case, actual) in snapshot.cases().iter().zip(evidence) {
        let (expected_requests, expected_references) = match case.kind() {
            CandidateCaseKind::ColdInitialize | CandidateCaseKind::Restore => (0, 1),
            CandidateCaseKind::ReadyProbe => (1, 1),
            CandidateCaseKind::MarkerNonstreaming | CandidateCaseKind::MarkerStreaming => (2, 2),
            CandidateCaseKind::Security => (2, 3),
            CandidateCaseKind::Park => (0, 2),
        };
        if actual.id != case.id()
            || actual.kind != case.kind()
            || actual.cycle != case.cycle()
            || actual.requests != expected_requests
            || actual.references.len() != expected_references
        {
            return Err(LifecycleError::Conflict);
        }
        for digest in &actual.references {
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || !unique.insert(digest)
            {
                return Err(LifecycleError::CorruptStoredData);
            }
        }
        requests += actual.requests;
    }
    if requests != requests_used || unique.len() != program.required_references as usize {
        return Err(LifecycleError::Conflict);
    }
    Ok(())
}

pub(crate) struct SecurityCheck {
    pub id: &'static str,
    pub endpoint: CandidateSecurityEndpoint,
    pub status: u16,
}
const SECURITY_CHECKS: [SecurityCheck; 3] = [
    SecurityCheck {
        id: "runtime-credential-admin-control",
        endpoint: CandidateSecurityEndpoint::AdminControl,
        status: 403,
    },
    SecurityCheck {
        id: "missing-inference-credential",
        endpoint: CandidateSecurityEndpoint::Inference,
        status: 401,
    },
    SecurityCheck {
        id: "unauthorized-health-generation",
        endpoint: CandidateSecurityEndpoint::HealthGeneration,
        status: 403,
    },
];
pub(crate) fn security_check(index: usize) -> Option<&'static SecurityCheck> {
    SECURITY_CHECKS.get(index)
}

#[derive(Debug)]
pub struct QualificationProgram {
    required_requests: u32,
    required_references: u32,
}
impl QualificationProgram {
    pub fn marker_request(
        &self,
        deployment: &str,
        ordinal: u32,
        stream: bool,
    ) -> Result<String, LifecycleError> {
        let expected = Self::marker(ordinal)?;
        Ok(fixed_request(
            deployment,
            &format!("Repeat exactly: {expected}"),
            stream,
        ))
    }
    pub(crate) fn marker(ordinal: u32) -> Result<&'static str, LifecycleError> {
        match ordinal {
            0 => Ok("MLLM_ALPHA_71"),
            1 => Ok("MLLM_BETA_29"),
            _ => Err(LifecycleError::Invalid),
        }
    }
    pub fn resolve(snapshot: &CandidateReviewedSnapshot) -> Result<Self, LifecycleError> {
        let recipe = snapshot.effective_recipe();
        let profile = recipe.profile();
        if !matches!(profile.launch_settings(), CandidateLaunch::Fake)
            || profile.build_fingerprint() != PROGRAM_REVISION
            || !profile.runtime_auth()
            || !profile.admin_auth()
            || !profile.experimental_controls()
            || recipe.recipe() != "standard"
            || snapshot.cases().len() > 128
        {
            return Err(LifecycleError::Unsupported);
        }
        let digest = format!("{:x}", Sha256::digest(CORPUS.as_bytes()));
        let mut required_requests = 0_u32;
        let mut required_references = 0_u32;
        for case in snapshot.cases() {
            let (requests, references) = match case.kind() {
                CandidateCaseKind::ReadyProbe => (1, 1),
                CandidateCaseKind::MarkerNonstreaming | CandidateCaseKind::MarkerStreaming => {
                    if case.count() != 2 || case.corpus_digest() != Some(digest.as_str()) {
                        return Err(LifecycleError::Unsupported);
                    }
                    (2, 2)
                }
                CandidateCaseKind::Security => (2, 3),
                CandidateCaseKind::Park => (0, 2),
                CandidateCaseKind::ColdInitialize | CandidateCaseKind::Restore => (0, 1),
            };
            if case.request_budget() < requests {
                return Err(LifecycleError::Unsupported);
            }
            required_requests = required_requests
                .checked_add(requests)
                .ok_or(LifecycleError::Unsupported)?;
            required_references = required_references
                .checked_add(references)
                .ok_or(LifecycleError::Unsupported)?;
        }
        if required_requests > snapshot.limits().max_requests()
            || required_requests > 4096
            || required_references > 4096
        {
            return Err(LifecycleError::Unsupported);
        }
        // Fake input accounting is one token per UTF-8 byte of the sole user
        // message's content, with no framing tokens. These fixed ASCII prompts
        // are not native tokenizer measurements. Candidate IDs are 26-byte ULIDs.
        // Security's generation-capable requests use the same Ready template.
        for content in [
            READY_PROMPT.to_owned(),
            format!("Repeat exactly: {}", Self::marker(0)?),
            format!("Repeat exactly: {}", Self::marker(1)?),
        ] {
            for stream in [false, true] {
                let body = fixed_request("00000000000000000000000000", &content, stream);
                if body.len() as i64 > snapshot.limits().max_request_body_bytes()
                    || content.len() > snapshot.limits().max_input_tokens_per_request() as usize
                    || REQUEST_OUTPUT_TOKENS > snapshot.limits().max_output_tokens_per_request()
                {
                    return Err(LifecycleError::Unsupported);
                }
            }
        }
        Ok(Self {
            required_requests,
            required_references,
        })
    }
    pub fn required_requests(&self) -> u32 {
        self.required_requests
    }
    pub fn required_references(&self) -> u32 {
        self.required_references
    }
}

pub(crate) fn marker_response_facts(
    deployment: &str,
    item: u32,
    stream: bool,
    response: &CandidateResponseObservation,
) -> Result<(bool, bool, bool, String), LifecycleError> {
    let expected = QualificationProgram::marker(item)?;
    let model = format!("candidate-{deployment}");
    match response {
        CandidateResponseObservation::Nonstreaming {
            model: actual,
            content,
            finish_reason,
        } => {
            if actual.len() + content.len() + finish_reason.len() > 1048576 {
                return Err(LifecycleError::Invalid);
            }
            Ok((
                !stream && actual == &model,
                content == expected,
                finish_reason == "stop",
                response_digest(&("nonstreaming", actual, content, finish_reason))?,
            ))
        }
        CandidateResponseObservation::Streaming { chunks, completed } => {
            if chunks.len() > 4096 {
                return Err(LifecycleError::Invalid);
            }
            let mut content = String::new();
            let mut bytes = 0_usize;
            let mut ordered = true;
            let mut models = true;
            let mut terminal = false;
            let mut facts = Vec::new();
            for (index, chunk) in chunks.iter().enumerate() {
                bytes = bytes.saturating_add(
                    chunk.model.len()
                        + chunk.content.len()
                        + chunk.finish_reason.as_ref().map_or(0, String::len),
                );
                if bytes > 1048576 {
                    return Err(LifecycleError::Invalid);
                }
                ordered &= chunk.index == index as u32 && !terminal;
                models &= chunk.model == model;
                if let Some(finish) = &chunk.finish_reason {
                    terminal = true;
                    ordered &=
                        finish == "stop" && chunk.content.is_empty() && index + 1 == chunks.len();
                }
                content.push_str(&chunk.content);
                facts.push((
                    chunk.index,
                    &chunk.model,
                    &chunk.content,
                    &chunk.finish_reason,
                ));
            }
            Ok((
                stream && models,
                content == expected,
                *completed && ordered && terminal,
                response_digest(&("streaming", facts, completed))?,
            ))
        }
        _ => Ok((false, false, false, response_digest(&"no_response")?)),
    }
}
fn response_digest(value: &impl serde::Serialize) -> Result<String, LifecycleError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(crate::lifecycle::completion::encode(value)?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_config::effective::candidate::validate_candidate_reviewed_snapshot_text;
    use serde_json::{json, Value};

    fn manifest() -> Value {
        let mut v: Value = serde_json::from_str(include_str!(
            "../../../mllm-config/tests/fixtures/candidate-fake.json"
        ))
        .unwrap();
        v["effective_recipe"]["resolved_profile"]["build_fingerprint"] =
            json!("qualification-fake-v1");
        v["effective_recipe"]["resolved_profile"]["admin_auth"] = json!(true);
        v["limits"]["max_requests"] = json!(12);
        for case in v["cases"].as_array_mut().unwrap() {
            match case["kind"].as_str().unwrap() {
                "security" => case["request_budget"] = json!(2),
                "marker_streaming" | "marker_nonstreaming" => {
                    case["count"] = json!(2);
                    case["request_budget"] = json!(2);
                    case["corpus_digest"] =
                        json!("a9f783fef0d21e31fa96b74eb6df40d77179109e1cf807b8e1980c36bcfd6395");
                }
                _ => {}
            }
        }
        v
    }

    #[test]
    fn supported_fake_preflight_counts_every_generation_capable_attempt() {
        let snapshot = validate_candidate_reviewed_snapshot_text(&manifest().to_string()).unwrap();
        let program = QualificationProgram::resolve(&snapshot).unwrap();
        assert_eq!(program.required_requests(), 12);
    }

    // Catches allowing a fixed request to exceed frozen authority before launch.
    #[test]
    fn fixed_requests_must_fit_body_input_and_output_limits() {
        for field in [
            "max_request_body_bytes",
            "max_input_tokens_per_request",
            "max_output_tokens_per_request",
        ] {
            let mut value = manifest();
            value["limits"][field] = json!(1);
            let snapshot = validate_candidate_reviewed_snapshot_text(&value.to_string()).unwrap();
            assert!(
                matches!(
                    QualificationProgram::resolve(&snapshot),
                    Err(LifecycleError::Unsupported)
                ),
                "{field}"
            );
        }
    }

    // Catches omitted deferred checks, refunded spending, reordered cases and
    // duplicate source coverage even when every supplied entry looks complete.
    #[test]
    fn suite_requires_every_ordered_case_and_unique_source() {
        use CandidateCaseKind::*;
        let snapshot = validate_candidate_reviewed_snapshot_text(include_str!(
            "../../../mllm-config/tests/fixtures/candidate-fake-qualification.json"
        ))
        .unwrap();
        let mut serial = 0;
        let mut evidence: Vec<_> = [
            ("cold_initialize-0", ColdInitialize, 0, 0, 1),
            ("ready_probe-0", ReadyProbe, 0, 1, 1),
            ("marker_nonstreaming-0", MarkerNonstreaming, 0, 2, 2),
            ("marker_streaming-0", MarkerStreaming, 0, 2, 2),
            ("security-0", Security, 0, 2, 3),
            ("park-1", Park, 1, 0, 2),
            ("restore-1", Restore, 1, 0, 1),
            ("ready_probe-1", ReadyProbe, 1, 1, 1),
            ("marker_nonstreaming-1", MarkerNonstreaming, 1, 2, 2),
            ("marker_streaming-1", MarkerStreaming, 1, 2, 2),
        ]
        .into_iter()
        .map(|(id, kind, cycle, requests, count)| SuiteCaseEvidence {
            id: id.into(),
            kind,
            cycle,
            requests,
            references: (0..count)
                .map(|_| {
                    serial += 1;
                    format!("{serial:064x}")
                })
                .collect(),
        })
        .collect();
        evaluate_suite(&snapshot, &evidence, 12).unwrap();
        assert!(evaluate_suite(&snapshot, &evidence, 11).is_err());
        assert!(evaluate_suite(&snapshot, &evidence[..9], 12).is_err());
        evidence.swap(0, 1);
        assert!(evaluate_suite(&snapshot, &evidence, 12).is_err());
        evidence.swap(0, 1);
        let status = evidence[5].references.pop().unwrap();
        assert!(evaluate_suite(&snapshot, &evidence, 12).is_err());
        evidence[5].references.push(status);
        evidence[1].references[0] = evidence[0].references[0].clone();
        assert!(evaluate_suite(&snapshot, &evidence, 12).is_err());
    }

    #[test]
    fn insufficient_security_budget_is_rejected_before_execution() {
        let mut v = manifest();
        v["cases"][4]["request_budget"] = json!(1);
        let snapshot = validate_candidate_reviewed_snapshot_text(&v.to_string()).unwrap();
        assert!(matches!(
            QualificationProgram::resolve(&snapshot),
            Err(LifecycleError::Unsupported)
        ));
    }
}
