#[path = "../src/f2_correctness.rs"]
mod f2_correctness;

use f2_correctness::{CorrectnessError, MarkerCase, MarkerResponse};

#[test]
fn corpus_has_bounded_distinct_markers_and_a_repeated_public_prefix() {
    let first = MarkerCase::new(0).unwrap();
    assert_eq!(first.marker(), "F2_MARKER_0000");
    assert_eq!(
        first.prompt(),
        "Reply with exactly this marker, without explanation: F2_MARKER_0000"
    );
    let mut markers = std::collections::BTreeSet::new();
    for index in 0..4096 {
        let case = MarkerCase::new(index).unwrap();
        assert!(markers.insert(case.marker()));
        assert!(
            case.prompt()
                .starts_with("Reply with exactly this marker, without explanation: ")
        );
        assert!(case.prompt().is_ascii());
        assert!(case.prompt().len() < 128);
    }
    assert_eq!(MarkerCase::new(4096), Err(CorrectnessError::CaseOutOfRange));
}

fn response(parts: &[&str], reason: &str, terminal: bool) -> Result<(), CorrectnessError> {
    let mut response = MarkerResponse::new(MarkerCase::new(7).unwrap());
    for part in parts {
        response.push(part)?;
    }
    response.finish_reason(reason)?;
    if terminal {
        response.terminal()?;
    }
    response.complete()
}

#[test]
fn streamed_and_collected_content_require_the_same_exact_marker() {
    assert_eq!(response(&["F2_MARKER_0007"], "stop", true), Ok(()));
    assert_eq!(
        response(&[" \r\nF2_", "MAR", "KER_", "0007\t"], "stop", true),
        Ok(())
    );
    for parts in [
        vec!["F2_MARKER_0008"],
        vec!["F2_MARKER_0007 F2_MARKER_0008"],
        vec!["F2_MARKER_000"],
        vec!["F2_MARKER_", "007", "0"],
        vec!["F2_MARKER_0007", "!"],
        vec!["\u{a0}F2_MARKER_0007"],
        vec![""],
    ] {
        assert_eq!(
            response(&parts, "stop", true),
            Err(CorrectnessError::ContentMismatch)
        );
    }
}

#[test]
fn exact_text_does_not_excuse_missing_terminal_or_bad_finish_reason() {
    assert_eq!(
        response(&["F2_MARKER_0007"], "stop", false),
        Err(CorrectnessError::Incomplete)
    );
    for reason in ["length", "content_filter", "tool_calls", "", "STOP"] {
        assert_eq!(
            response(&["F2_MARKER_0007"], reason, true),
            Err(CorrectnessError::FinishReason)
        );
        let mut ignored = MarkerResponse::new(MarkerCase::new(7).unwrap());
        ignored.push("F2_MARKER_0007").unwrap();
        let _ = ignored.finish_reason(reason);
        assert_eq!(ignored.complete(), Err(CorrectnessError::FinishReason));
    }
    assert_eq!(
        MarkerResponse::new(MarkerCase::new(7).unwrap()).complete(),
        Err(CorrectnessError::Incomplete)
    );
}

#[test]
fn invalid_terminal_order_and_post_finish_content_latch_failure() {
    for violation in 0..5 {
        let mut response = MarkerResponse::new(MarkerCase::new(7).unwrap());
        response.push("F2_MARKER_0007").unwrap();
        let error = match violation {
            0 => response.terminal(),
            1 => {
                response.finish_reason("stop").unwrap();
                response.push("other marker")
            }
            2 => {
                response.finish_reason("stop").unwrap();
                response.finish_reason("stop")
            }
            3 => {
                response.finish_reason("stop").unwrap();
                response.terminal().unwrap();
                response.terminal()
            }
            _ => {
                response.finish_reason("stop").unwrap();
                response.terminal().unwrap();
                response.push("")
            }
        };
        assert_eq!(error, Err(CorrectnessError::ProtocolOrder));
        assert_eq!(response.complete(), Err(CorrectnessError::ProtocolOrder));
    }
}

#[test]
fn byte_limit_is_checked_before_appending_and_failure_cannot_be_repaired() {
    let mut response = MarkerResponse::new(MarkerCase::new(7).unwrap());
    response.push(&" ".repeat(114)).unwrap();
    response.push("F2_MARKER_0007").unwrap();
    response.finish_reason("stop").unwrap();
    response.terminal().unwrap();
    assert_eq!(response.complete(), Ok(()));

    let mut response = MarkerResponse::new(MarkerCase::new(7).unwrap());
    assert_eq!(
        response.push(&"x".repeat(129)),
        Err(CorrectnessError::ContentLimit)
    );
    assert_eq!(
        response.push("F2_MARKER_0007"),
        Err(CorrectnessError::ContentLimit)
    );
    assert_eq!(response.complete(), Err(CorrectnessError::ContentLimit));

    let mut response = MarkerResponse::new(MarkerCase::new(7).unwrap());
    assert_eq!(
        response.push(&"é".repeat(65)),
        Err(CorrectnessError::ContentLimit)
    );
}

#[test]
fn empty_chunks_are_bounded_even_without_content_growth() {
    let mut response = MarkerResponse::new(MarkerCase::new(7).unwrap());
    for _ in 0..256 {
        response.push("").unwrap();
    }
    assert_eq!(response.push(""), Err(CorrectnessError::ChunkLimit));
    assert_eq!(response.complete(), Err(CorrectnessError::ChunkLimit));
}
