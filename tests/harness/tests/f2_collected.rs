#[path = "../src/f2_collected.rs"]
mod f2_collected;
#[path = "../src/f2_correctness.rs"]
mod f2_correctness;

use f2_collected::check_collected_marker;
use f2_correctness::MarkerCase;

fn valid() -> String {
    r#"{"model":"served","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"F2_MARKER_0007"},"finish_reason":"stop"}]}"#.into()
}

fn check(body: &str) -> bool {
    check_collected_marker(MarkerCase::new(7).unwrap(), "served", body.as_bytes()).is_ok()
}

#[test]
fn accepts_exact_marker_and_json_escapes() {
    assert!(MarkerCase::new(7)
        .unwrap()
        .prompt()
        .contains("F2_MARKER_0007"));
    assert!(check(&valid()));
    assert!(check(
        &valid().replace("F2_MARKER_0007", r"\tF2_MARKER_\u0030007\n")
    ));
}

#[test]
fn rejects_wrong_identity_content_and_terminal() {
    for (old, new) in [
        ("served", "other"),
        ("assistant", "user"),
        ("F2_MARKER_0007", "F2_MARKER_0008"),
        ("stop", "length"),
        ("chat.completion", "chat.completion.chunk"),
        ("\"index\":0", "\"index\":1"),
    ] {
        assert!(!check(&valid().replace(old, new)), "{old}");
    }
}

#[test]
fn rejects_malformed_and_duplicate_envelopes() {
    for body in [
        "null".into(),
        "{}".into(),
        valid()[..40].into(),
        valid().replace("\"model\":", "\"model\":\"served\",\"model\":"),
        valid().replace("\"content\":", "\"content\":null,\"content\":"),
        valid().replace("\"content\":\"F2_MARKER_0007\"", "\"content\":null"),
    ] {
        assert!(!check(&body));
    }
}

#[test]
fn rejects_alternative_output_and_errors() {
    for field in [
        r#""tool_calls":[{}]"#,
        r#""function_call":{}"#,
        r#""refusal":"private""#,
        r#""reasoning_content":"private""#,
    ] {
        assert!(!check(
            &valid().replace("\"role\":", &format!("{field},\"role\":"))
        ));
    }
    assert!(!check(&valid().replacen('{', "{\"error\":{},", 1)));
}

#[test]
fn bounds_input_and_diagnostics() {
    let case = MarkerCase::new(7).unwrap();
    for model in [String::new(), "m".repeat(257), "bad\nmodel".into()] {
        assert!(check_collected_marker(case, &model, valid().as_bytes()).is_err());
    }
    assert!(!check(&" ".repeat(1_048_577)));
    let error = check_collected_marker(case, "served", b"private-response").unwrap_err();
    assert!(!format!("{error:?}").contains("private-response"));
}

#[test]
fn requires_exactly_one_choice_and_accepts_benign_metadata() {
    let mut body: serde_json::Value = serde_json::from_str(&valid()).unwrap();
    body["id"] = "opaque-metadata".into();
    body["usage"] = serde_json::json!({"completion_tokens": 8});
    body["choices"][0]["message"]["tool_calls"] = serde_json::json!([]);
    body["choices"][0]["message"]["refusal"] = serde_json::Value::Null;
    assert!(check(&body.to_string()));
    let choice = body["choices"][0].clone();
    body["choices"] = serde_json::json!([choice.clone(), choice]);
    assert!(!check(&body.to_string()));
    body["choices"] = serde_json::json!([]);
    assert!(!check(&body.to_string()));
}

#[test]
fn rejects_trailing_data_and_duplicate_nullable_fields() {
    assert!(!check(&(valid() + "{}")));
    assert!(!check(&valid().replacen(
        '{',
        "{\"error\":null,\"error\":null,",
        1
    )));
    assert!(!check(&valid().replace(
        "\"role\":",
        "\"refusal\":null,\"refusal\":null,\"role\":"
    )));
}
