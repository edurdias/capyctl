#[path = "../src/f2_correctness.rs"]
mod f2_correctness;
#[path = "../src/f2_streamed.rs"]
mod f2_streamed;

use f2_correctness::MarkerCase;
use f2_streamed::StreamedMarker;

fn stream() -> StreamedMarker {
    let case = MarkerCase::new(7).unwrap();
    assert!(case.prompt().contains("F2_MARKER_0007"));
    StreamedMarker::new(case, "served").unwrap()
}
fn chunk(content: &str, finish: &str) -> String {
    format!(
        r#"{{"model":"served","object":"chat.completion.chunk","choices":[{{"index":0,"delta":{{"content":"{content}"}},"finish_reason":{finish}}}]}}"#
    )
}
#[test]
fn reconstructs_ordered_marker_before_terminal() {
    let mut s = stream();
    s.push_data(&chunk("F2_MARKER_", "null")).unwrap();
    s.push_data(&chunk("0007", "\"stop\"")).unwrap();
    s.push_data("[DONE]").unwrap();
    s.complete().unwrap();
}
#[test]
fn rejects_missing_terminal_and_early_terminal() {
    assert!(stream().complete().is_err());
    let mut s = stream();
    assert!(s.push_data("[DONE]").is_err());
    assert!(s.push_data(&chunk("F2_MARKER_0007", "\"stop\"")).is_err());
    assert!(s.complete().is_err());
}
#[test]
fn rejects_model_drift_and_bad_finish() {
    for data in [
        chunk("F2_MARKER_0007", "\"length\""),
        chunk("F2_MARKER_0007", "\"stop\"").replace("served", "other"),
    ] {
        let mut s = stream();
        assert!(s.push_data(&data).is_err());
        assert!(s.complete().is_err());
    }
}
#[test]
fn rejects_post_finish_content_and_duplicate_terminal() {
    for extra in [chunk("", "null"), "[DONE]".into()] {
        let mut s = stream();
        s.push_data(&chunk("F2_MARKER_0007", "\"stop\"")).unwrap();
        s.push_data("[DONE]").unwrap();
        assert!(s.push_data(&extra).is_err());
        assert!(s.complete().is_err());
    }
}
#[test]
fn rejects_malformed_alternative_output_and_event_limits() {
    for data in [
        "null".into(),
        chunk("x", "null").replace("\"content\":", "\"tool_calls\":[{}],\"content\":"),
        "x".repeat(65_537),
    ] {
        assert!(stream().push_data(&data).is_err());
    }
    let mut s = stream();
    for _ in 0..256 {
        s.push_data(&chunk("", "null")).unwrap();
    }
    assert!(s.push_data(&chunk("", "null")).is_err());
    assert!(s.complete().is_err());
}

#[test]
fn rejects_reordered_content_even_with_valid_terminal() {
    let mut s = stream();
    s.push_data(&chunk("0007", "null")).unwrap();
    s.push_data(&chunk("F2_MARKER_", "\"stop\"")).unwrap();
    s.push_data("[DONE]").unwrap();
    assert!(s.complete().is_err());
}

#[test]
fn rejects_duplicate_fields_wrong_role_and_usage_only_events() {
    for data in [
        chunk("x", "null").replace("\"content\":", "\"content\":null,\"content\":"),
        chunk("x", "null").replace("\"content\":", "\"role\":\"user\",\"content\":"),
        r#"{"model":"served","object":"chat.completion.chunk","choices":[],"usage":{}}"#.into(),
    ] {
        assert!(stream().push_data(&data).is_err());
    }
}

#[test]
fn validates_model_before_copy_and_bounds_cumulative_metadata() {
    let case = MarkerCase::new(7).unwrap();
    for model in [String::new(), "m".repeat(257), "m\n".into()] {
        assert!(StreamedMarker::new(case, &model).is_err());
    }
    let data = chunk("", "null").replacen(
        '{',
        &format!("{{\"metadata\":\"{}\",", "x".repeat(60_000)),
        1,
    );
    let mut s = stream();
    for _ in 0..17 {
        s.push_data(&data).unwrap();
    }
    assert!(s.push_data(&data).is_err());
    assert!(s.complete().is_err());
}
