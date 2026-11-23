//! ADR 0041 §3: the closed extraction request and whole-response evidence.
use capyctl_config::effective::DeploymentApi;
use capyctl_config::extractions::FieldType;
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtractionRequestRefusal {
    InvalidRequest {
        detail: String,
    },
    FieldTypeUnsupported {
        field: String,
        field_type: FieldType,
    },
}

impl std::fmt::Display for ExtractionRequestRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest { detail } => f.write_str(detail),
            Self::FieldTypeUnsupported { field, field_type } => {
                write!(
                    f,
                    "field `{field}`: type `{}` is not supported",
                    field_type.name()
                )
            }
        }
    }
}

fn invalid(detail: impl Into<String>) -> ExtractionRequestRefusal {
    ExtractionRequestRefusal::InvalidRequest {
        detail: detail.into(),
    }
}

/// ADR 0041 §3: shape mismatch is judged before validation or activation,
/// including when the other API's member is null or both members occur.
pub fn other_api_key(api: DeploymentApi, body: &Value) -> Option<&'static str> {
    let key = match api {
        DeploymentApi::Decisions => "fields",
        DeploymentApi::Extractions => "questions",
        _ => return None,
    };
    body.as_object()
        .filter(|body| body.contains_key(key))
        .map(|_| key)
}

/// ADR 0041 §3: extraction uses the same completion envelope as decisions.
pub fn extraction_answered(answer: &Value) -> bool {
    crate::forward::decision_answered(answer)
}

/// ADR 0041 §3: PDF spans refer to the engine's returned text, never the asset key.
pub(crate) fn pdf_answered(answer: &Value, fields: &Value) -> bool {
    if !extraction_answered(answer) {
        return false;
    }
    let Some(state) = answer.get("state_text").and_then(Value::as_str) else {
        return false;
    };
    let state_len = state.chars().count() as u64;
    answer["answers"]
        .as_object()
        .unwrap()
        .iter()
        .all(|(name, answer)| {
            let text_list = fields[name]["type"] == "list" && fields[name]["items"] == "text";
            let span_bounds = |span: &Value| {
                span.as_array()
                    .filter(|span| span.len() == 2)
                    .is_some_and(|span| match (span[0].as_u64(), span[1].as_u64()) {
                        (Some(start), Some(end)) => start < end && end <= state_len,
                        _ => false,
                    })
            };
            let span_matches = |span: &Value, text: &Value| {
                let Some(text) = text.as_str() else {
                    return span.is_null() && text.is_null();
                };
                let Some(span) = span.as_array().filter(|span| span.len() == 2) else {
                    return false;
                };
                let (Some(start), Some(end)) = (span[0].as_u64(), span[1].as_u64()) else {
                    return false;
                };
                start < end
                    && end <= state_len
                    && state
                        .chars()
                        .skip(start as usize)
                        .take((end - start) as usize)
                        .eq(text.chars())
            };
            let page_valid = |page: &Value| page.is_null() || page.as_u64().is_some_and(|n| n > 0);
            if answer.get("page").is_some_and(|page| !page_valid(page)) {
                return false;
            }
            if answer["type"] == "text" {
                if !answer.get("span").is_some_and(|span| {
                    span_matches(span, answer.get("guess").unwrap_or(&answer["value"]))
                }) {
                    return false;
                }
            } else if let Some(spans) = answer.get("spans").filter(|spans| !spans.is_null()) {
                let Some(spans) = spans.as_array() else {
                    return false;
                };
                let Some(values) = answer.get("value").and_then(Value::as_array) else {
                    return false;
                };
                if spans.len() != values.len()
                    || !spans.iter().zip(values).all(|(span, text)| {
                        if text_list {
                            span_matches(span, text)
                        } else {
                            span.is_null() || span_bounds(span)
                        }
                    })
                {
                    return false;
                }
            } else if text_list && answer.get("value").is_some_and(|value| !value.is_null()) {
                return false;
            }
            answer.get("pages").is_none_or(|pages| {
                pages.is_null()
                    || pages.as_array().is_some_and(|pages| {
                        answer
                            .get("spans")
                            .and_then(Value::as_array)
                            .is_some_and(|spans| {
                                pages.len() == spans.len() && pages.iter().all(page_valid)
                            })
                    })
            })
        })
}

// Validate canonical standard base64 without decoding or allocating PDF bytes.
fn standard_base64(data: &str) -> bool {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = data.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let padding = if bytes.ends_with(b"==") {
        2
    } else {
        usize::from(bytes.ends_with(b"="))
    };
    let content = &bytes[..bytes.len() - padding];
    content.iter().all(|byte| digit(*byte).is_some())
        && match padding {
            2 => digit(content[content.len() - 1]).is_some_and(|n| n & 15 == 0),
            1 => digit(content[content.len() - 1]).is_some_and(|n| n & 3 == 0),
            _ => true,
        }
}

/// Strict ISO calendar date, including Gregorian leap-year rules.
pub(crate) fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return false;
    }
    let year = value[..4].parse::<u32>().unwrap();
    let month = value[5..7].parse::<u32>().unwrap();
    let day = value[8..].parse::<u32>().unwrap();
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|n| n.is_finite())
}

/// ADR 0041 §3: validate every closed member before waiting or activation.
pub fn validate_extraction_request(
    body: &Value,
    declared: &[FieldType],
) -> Result<(), ExtractionRequestRefusal> {
    let body = body
        .as_object()
        .ok_or_else(|| invalid("body must be an object"))?;
    for key in body.keys() {
        if ![
            "model",
            "state",
            "fields",
            "images",
            "assets",
            "reference_date",
            "options",
        ]
        .contains(&key.as_str())
        {
            return Err(invalid(format!("member `{key}` is not accepted")));
        }
    }
    if !body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        return Err(invalid("model must be a non-empty string"));
    }
    let images = match body.get("images") {
        None => &[][..],
        Some(Value::Array(images)) if images.len() <= 4 => images.as_slice(),
        _ => return Err(invalid("images must be an array of at most 4 items")),
    };
    for image in images {
        let valid = match image {
            Value::String(s) => s
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:")),
            Value::Object(obj) => {
                obj.len() == 2
                    && ["content_type", "base64"]
                        .iter()
                        .all(|key| obj.get(*key).is_some_and(Value::is_string))
            }
            _ => false,
        };
        if !valid {
            return Err(invalid("images items must be data: strings or objects with exactly string content_type and base64"));
        }
    }
    if let Some(assets) = body.get("assets") {
        let assets = assets
            .as_object()
            .filter(|assets| assets.len() == 1)
            .ok_or_else(|| invalid("assets must contain exactly one named PDF"))?;
        let (name, asset) = assets.iter().next().unwrap();
        let _asset = asset
            .as_object()
            .filter(|asset| {
                !name.is_empty()
                    && asset
                        .keys()
                        .all(|key| ["format", "data_base64", "type"].contains(&key.as_str()))
                    && asset.get("format").and_then(Value::as_str) == Some("pdf")
                    && asset
                        .get("data_base64")
                        .and_then(Value::as_str)
                        .is_some_and(standard_base64)
                    && asset
                        .get("type")
                        .is_none_or(|kind| kind.as_str() == Some("document"))
            })
            .ok_or_else(|| {
                invalid("assets must contain a closed PDF asset with standard data_base64")
            })?;
        if !images.is_empty() {
            return Err(invalid("images and PDF assets cannot be combined"));
        }
        match body.get("state") {
            None => {}
            Some(Value::String(state)) if state.is_empty() || state == name => {}
            _ => {
                return Err(invalid(
                    "PDF state must be absent, empty, or the sole asset key",
                ))
            }
        }
    } else {
        match body.get("state") {
            None if !images.is_empty() => {}
            Some(Value::String(state)) if !state.is_empty() || !images.is_empty() => {}
            _ => {
                return Err(invalid(
                    "state must be a string, non-empty unless images is non-empty",
                ))
            }
        }
    }
    if body
        .get("reference_date")
        .is_some_and(|date| !date.as_str().is_some_and(valid_date))
    {
        return Err(invalid(
            "reference_date must be an ISO calendar date YYYY-MM-DD",
        ));
    }
    if let Some(options) = body.get("options") {
        let options = options
            .as_object()
            .ok_or_else(|| invalid("options must be an object"))?;
        for (key, value) in options {
            if !["include_quantiles", "return_state_text"].contains(&key.as_str())
                || !value.is_boolean()
            {
                return Err(invalid(format!(
                    "options setting `{key}` must be an accepted boolean"
                )));
            }
        }
    }
    let fields = body
        .get("fields")
        .and_then(Value::as_object)
        .filter(|fields| !fields.is_empty())
        .ok_or_else(|| invalid("fields must be a non-empty object"))?;
    for (name, spec) in fields {
        let mut chars = name.bytes();
        if !(1..=64).contains(&name.len())
            || !chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            || !chars.all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        {
            return Err(invalid(format!("field `{name}` has an invalid name")));
        }
        let spec = spec
            .as_object()
            .ok_or_else(|| invalid(format!("field `{name}` must be an object")))?;
        let field_type = FieldType::ALL
            .iter()
            .copied()
            .find(|t| spec.get("type").and_then(Value::as_str) == Some(t.name()))
            .ok_or_else(|| {
                invalid(format!(
                    "field `{name}` setting `type` must name a known field type"
                ))
            })?;
        validate_spec(name, field_type, spec)?;
        if !declared.contains(&field_type) {
            return Err(ExtractionRequestRefusal::FieldTypeUnsupported {
                field: name.clone(),
                field_type,
            });
        }
    }
    Ok(())
}

fn validate_spec(
    name: &str,
    kind: FieldType,
    spec: &Map<String, Value>,
) -> Result<(), ExtractionRequestRefusal> {
    use FieldType::*;
    let fail = |setting: &str| {
        invalid(format!(
            "field `{name}` setting `{setting}` is unknown or has an invalid shape"
        ))
    };
    if kind == List && !spec.contains_key("items") {
        return Err(fail("items"));
    }
    if matches!(kind, Estimate | Forecast) {
        for setting in ["min", "max"] {
            if !spec.contains_key(setting) {
                return Err(fail(setting));
            }
        }
    }
    for (setting, value) in spec {
        let valid = match setting.as_str() {
            "type" => true,
            "instructions" => value.is_string(),
            // ADR 0041 §3: null thresholds belong only to scalar answers.
            "null_threshold" => {
                !matches!(kind, List | Estimate | Forecast)
                    && finite(value).is_some_and(|n| n > 0.0 && n <= 1.0)
            }
            "unit" => value.is_string(),
            "integer" => value.is_boolean(),
            "decimals" => value.as_u64().is_some_and(|n| n <= 6),
            "items" => value
                .as_str()
                .is_some_and(|s| ["text", "number", "date"].contains(&s)),
            "min" | "max" => finite(value).is_some() || value.as_str().is_some_and(valid_date),
            "step" => {
                finite(value).is_some_and(|n| n > 0.0)
                    || value
                        .as_str()
                        .is_some_and(|s| ["month", "year"].contains(&s))
            }
            "scale" => value
                .as_str()
                .is_some_and(|s| ["linear", "log"].contains(&s)),
            "horizon" => value.as_u64().is_some_and(|n| (1..=64).contains(&n)),
            "interval" => value.as_str().is_some_and(|s| {
                [
                    "step", "minute", "hour", "day", "week", "month", "quarter", "year",
                ]
                .contains(&s)
            }),
            _ => false,
        };
        if !valid {
            return Err(fail(setting));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(spec: Value) -> Value {
        json!({"model":"extract", "state":"input", "fields":{"field":spec}})
    }

    // T50: named PDF assets are shape-checked, never converted by CapyCTL.
    #[test]
    fn pdf_asset_boundaries() {
        let pdf = json!({"format":"pdf","data_base64":"JVBERi0="});
        let mut body = request(json!({"type":"text"}));
        body["assets"] = json!({"invoice":pdf});
        body["options"] = json!({"return_state_text":false});
        for state in [None, Some(""), Some("invoice")] {
            body.as_object_mut().unwrap().remove("state");
            if let Some(state) = state {
                body["state"] = json!(state);
            }
            assert_eq!(validate_extraction_request(&body, &FieldType::ALL), Ok(()));
        }
        body["assets"]["invoice"]["type"] = json!("document");
        body["images"] = json!([]);
        assert!(validate_extraction_request(&body, &FieldType::ALL).is_ok());
        for state in [json!("other text"), json!(null), json!(["invoice"])] {
            body["state"] = state;
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_err());
        }
        body["state"] = json!("invoice");
        body["images"] = json!(["data:image/png;base64,AAAA"]);
        assert!(validate_extraction_request(&body, &FieldType::ALL).is_err());
        body.as_object_mut().unwrap().remove("images");
        for assets in [
            json!(null),
            json!([]),
            json!({}),
            json!({"":pdf}),
            json!({"invoice":pdf,"second":pdf}),
            json!({"invoice":null}),
            json!({"invoice":{"format":"png","data_base64":"AAAA"}}),
            json!({"invoice":{"format":"PDF","data_base64":"AAAA"}}),
            json!({"invoice":{"format":"pdf"}}),
            json!({"invoice":{"format":"pdf","data_base64":1}}),
            json!({"invoice":{"format":"pdf","data_base64":"AAAA","extra":true}}),
            json!({"invoice":{"format":"pdf","data_base64":"AAAA","type":"image"}}),
        ] {
            body["assets"] = assets;
            assert!(
                validate_extraction_request(&body, &FieldType::ALL).is_err(),
                "{body}"
            );
        }
        for data in [
            "", "x", "AA", "AA-_", "AA A", "AA\nA", "====", "A===", "AB==", "AAB=",
        ] {
            body["assets"] = json!({"invoice":{"format":"pdf","data_base64":data}});
            assert!(
                validate_extraction_request(&body, &FieldType::ALL).is_err(),
                "{data:?}"
            );
        }
        for data in ["AAAA", "AA==", "AAA=", "+/8="] {
            body["assets"] = json!({"invoice":{"format":"pdf","data_base64":data}});
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_ok());
        }
    }

    // T50: PDF evidence uses echoed Unicode text, not the request's asset key.
    #[test]
    fn pdf_response_evidence() {
        fn pdf_answered(answer: &Value) -> bool {
            super::pdf_answered(
                answer,
                &json!({
                    "text":{"type":"text"},
                    "list":{"type":"list","items":"text"}
                }),
            )
        }
        let mut answer = json!({
            "state_text":"[Page 1]\né🙂 invoice",
            "answers":{
                "text":{"type":"text","value":"é🙂","guess":"é🙂","span":[9,11],"page":1},
                "list":{"type":"list","value":["é🙂","invoice"],"spans":[[9,11],[12,19]],"pages":[1,1]}
            }
        });
        assert!(pdf_answered(&answer));
        for state in [json!(null), json!(12), json!("invoice")] {
            let mut bad = answer.clone();
            bad["state_text"] = state;
            assert!(!pdf_answered(&bad));
        }
        let mut missing = answer.clone();
        missing.as_object_mut().unwrap().remove("state_text");
        assert!(!pdf_answered(&missing));
        for (member, value) in [
            ("span", json!([9, 15])),
            ("span", json!([0, 2])),
            ("page", json!(0)),
            ("page", json!("1")),
        ] {
            let mut bad = answer.clone();
            bad["answers"]["text"][member] = value;
            assert!(!pdf_answered(&bad));
        }
        for pages in [json!([1]), json!([1, 0]), json!("1")] {
            let mut bad = answer.clone();
            bad["answers"]["list"]["pages"] = pages;
            assert!(!pdf_answered(&bad));
        }
        answer["answers"]["text"]
            .as_object_mut()
            .unwrap()
            .remove("page");
        answer["answers"]["list"]
            .as_object_mut()
            .unwrap()
            .remove("pages");
        assert!(pdf_answered(&answer));
        let mut missing_spans = answer.clone();
        missing_spans["answers"]["list"]
            .as_object_mut()
            .unwrap()
            .remove("spans");
        assert!(!pdf_answered(&missing_spans));
        assert!(extraction_answered(
            &json!({"answers":{"x":{"type":"text"}}})
        ));
        let typed_lists = json!({
            "state_text":"[Page 1]\n1,250 USD; 12 November 2026",
            "answers":{
                "numbers":{"type":"list","value":[1250],"spans":[[9,14]],"pages":[1]},
                "dates":{"type":"list","value":["2026-11-12"],"spans":[[20,36]],"pages":[1]}
            }
        });
        let fields = json!({
            "numbers":{"type":"list","items":"number"},
            "dates":{"type":"list","items":"date"}
        });
        assert!(super::pdf_answered(&typed_lists, &fields));
        let mut bad = typed_lists.clone();
        bad["answers"]["dates"]["spans"] = json!([[20, 100]]);
        assert!(!super::pdf_answered(&bad, &fields));
    }

    fn accepts(spec: Value) {
        assert_eq!(
            validate_extraction_request(&request(spec), &FieldType::ALL),
            Ok(())
        );
    }

    fn rejects(spec: Value, setting: &str) {
        let Err(ExtractionRequestRefusal::InvalidRequest { detail }) =
            validate_extraction_request(&request(spec), &FieldType::ALL)
        else {
            panic!("expected invalid setting {setting}");
        };
        assert!(detail.contains("field"), "{detail}");
        assert!(detail.contains(setting), "{detail}");
    }

    // T50: every type and every legal setting family.
    #[test]
    fn accepts_all_types_and_settings() {
        for kind in ["text", "date", "time", "datetime"] {
            accepts(json!({"type":kind,"instructions":"","null_threshold":1}));
        }
        accepts(
            json!({"type":"number","unit":"USD","integer":false,"decimals":0,"null_threshold":0.00001}),
        );
        accepts(json!({"type":"duration","unit":"years","integer":true,"decimals":6}));
        for items in ["text", "number", "date"] {
            accepts(json!({"type":"list","items":items}));
        }
        accepts(json!({"type":"list","items":"number","decimals":6}));
        accepts(json!({"type":"estimate","min":1,"max":10,"step":0.5,"scale":"log","unit":"USD"}));
        accepts(
            json!({"type":"estimate","min":"2024-02-29","max":"2025-01-01","step":"month","scale":"linear"}),
        );
        for horizon in [1, 64] {
            for interval in [
                "step", "minute", "hour", "day", "week", "month", "quarter", "year",
            ] {
                accepts(
                    json!({"type":"forecast","min":0,"max":100,"horizon":horizon,"interval":interval,"step":1}),
                );
            }
        }
    }

    // T50: other semantic oddities remain the engine's judgement.
    #[test]
    fn accepts_other_settings_independently_of_type() {
        for kind in FieldType::ALL {
            accepts(json!({
                "type":kind.name(), "unit":"arbitrary", "integer":true,
                "decimals":2, "items":"text", "min":10, "max":-1,
                "step":"month", "scale":"log", "horizon":2, "interval":"day"
            }));
        }
        accepts(json!({"type":"forecast","min":0,"max":10}));
        accepts(json!({"type":"estimate","min":1,"max":1}));
        accepts(json!({"type":"estimate","min":0,"max":"2026-01-01","scale":"log"}));
        accepts(json!({"type":"estimate","min":"2026-01-01","max":"2025-01-01"}));
        accepts(json!({"type":"list","items":"text","decimals":2}));
        accepts(json!({"type":"duration","unit":"day"}));
    }

    // T50: non-scalar thresholds fail router validation, even with legal values.
    #[test]
    fn rejects_null_threshold_on_non_scalars() {
        for kind in ["list", "estimate", "forecast"] {
            for threshold in [0.00001, 0.5, 1.0] {
                rejects(
                    json!({
                        "type":kind, "items":"text", "min":0, "max":10,
                        "null_threshold":threshold
                    }),
                    "null_threshold",
                );
            }
        }
    }

    // T50: every field accepts settings by shape, and rejects unknown names.
    #[test]
    fn malformed_standalone_settings_are_rejected_for_every_type() {
        for kind in FieldType::ALL {
            for (setting, value) in [
                ("unknown", json!(true)),
                ("instructions", json!(false)),
                ("null_threshold", json!(0)),
                ("unit", json!([])),
                ("integer", json!(1)),
                ("decimals", json!(7)),
                ("items", json!("duration")),
                ("min", json!("not-a-date")),
                ("max", json!(null)),
                ("step", json!("day")),
                ("scale", json!("other")),
                ("horizon", json!(65)),
                ("interval", json!("seconds")),
            ] {
                let mut spec = json!({"type":kind.name(),"items":"text","min":0,"max":10});
                spec[setting] = value;
                rejects(spec, setting);
            }
        }
    }

    // T50: numeric, integer, distribution, and required-setting boundaries.
    #[test]
    fn rejects_invalid_setting_values() {
        for value in [
            json!(0),
            json!(-1),
            json!(1.0001),
            json!("0.5"),
            Value::Null,
        ] {
            rejects(
                json!({"type":"text","null_threshold":value}),
                "null_threshold",
            );
        }
        rejects(json!({"type":"text","instructions":false}), "instructions");
        rejects(json!({"type":"number","integer":1}), "integer");
        rejects(json!({"type":"number","unit":1}), "unit");
        for value in [json!(-1), json!(7), json!(1.5), json!("2"), Value::Null] {
            rejects(json!({"type":"number","decimals":value}), "decimals");
        }
        rejects(json!({"type":"list"}), "items");
        rejects(json!({"type":"list","items":"duration"}), "items");
        rejects(json!({"type":"estimate","max":10}), "min");
        rejects(json!({"type":"estimate","min":0}), "max");
        for (setting, value) in [
            ("min", json!("2025-02-29")),
            ("min", json!(null)),
            ("max", json!(true)),
            ("max", json!([])),
        ] {
            let mut spec = json!({"type":"estimate","min":0,"max":10});
            spec[setting] = value;
            rejects(spec, setting);
        }
        for step in [json!(0), json!(-1), Value::Null] {
            rejects(
                json!({"type":"estimate","min":0,"max":10,"step":step}),
                "step",
            );
        }
        for step in ["day", "quarter", "months"] {
            rejects(
                json!({"type":"estimate","min":"2024-01-01","max":"2025-01-01","step":step}),
                "step",
            );
        }
        accepts(json!({"type":"estimate","min":"2024-01-01","max":"2025-01-01","step":"year"}));
        accepts(json!({"type":"estimate","min":"2024-01-01","max":"2025-01-01","step":1}));
        rejects(
            json!({"type":"estimate","min":0,"max":10,"scale":"other"}),
            "scale",
        );
        for horizon in [json!(0), json!(65), json!(1.5), json!("1")] {
            rejects(
                json!({"type":"forecast","min":0,"max":10,"horizon":horizon}),
                "horizon",
            );
        }
        rejects(
            json!({"type":"forecast","min":0,"max":10,"horizon":1,"interval":"seconds"}),
            "interval",
        );
        rejects(json!({"type":"unknown"}), "type");
        rejects(json!({}), "type");
    }

    // T50: calendar validation is lexical, Gregorian, and independent of locale.
    #[test]
    fn calendar_boundaries() {
        for date in [
            "0000-02-29",
            "2000-02-29",
            "2024-02-29",
            "2026-04-30",
            "9999-12-31",
        ] {
            assert!(valid_date(date), "{date}");
        }
        for date in [
            "1900-02-29",
            "2025-02-29",
            "2026-04-31",
            "2026-00-01",
            "2026-13-01",
            "2026-01-00",
            "2026-01-32",
            "2026-1-01",
            "26-01-01",
            "2026-01-01T00:00",
            "２０２６-01-01",
            "2026-01-01\n",
        ] {
            assert!(!valid_date(date), "{date}");
            let mut body = request(json!({"type":"text"}));
            body["reference_date"] = json!(date);
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_err());
        }
    }

    // T50: closed top level, field names, options, and required shapes.
    #[test]
    fn request_shape_boundaries() {
        for body in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"model":"x","state":"x","fields":{}}),
        ] {
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_err());
        }
        for (key, value) in [
            ("model", json!("")),
            ("model", json!(1)),
            ("state", json!(null)),
            ("state", json!("")),
            ("fields", json!([])),
            ("options", json!(null)),
            ("reference_date", json!(12)),
            ("files", json!([])),
            ("questions", json!({})),
            ("stream", json!(false)),
        ] {
            let mut body = request(json!({"type":"text"}));
            body[key] = value;
            assert!(
                validate_extraction_request(&body, &FieldType::ALL).is_err(),
                "{key}"
            );
        }
        for name in ["a", "_", "a.b-c_1", &"a".repeat(64)] {
            let mut body = request(json!({"type":"text"}));
            body["fields"] = json!({name:{"type":"text"}});
            assert!(
                validate_extraction_request(&body, &FieldType::ALL).is_ok(),
                "{name}"
            );
        }
        for name in ["", "1a", "a b", "a/b", "é", &"a".repeat(65)] {
            let mut body = request(json!({"type":"text"}));
            body["fields"] = json!({name:{"type":"text"}});
            assert!(
                validate_extraction_request(&body, &FieldType::ALL).is_err(),
                "{name}"
            );
        }
        for options in [
            json!({}),
            json!({"include_quantiles":true,"return_state_text":false}),
        ] {
            let mut body = request(json!({"type":"text"}));
            body["options"] = options;
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_ok());
        }
        for options in [
            json!({"unknown":true}),
            json!({"include_quantiles":1}),
            json!({"return_state_text":null}),
        ] {
            let mut body = request(json!({"type":"text"}));
            body["options"] = options;
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_err());
        }
    }

    // T50: image bytes are not decoded; only the closed envelope is judged.
    #[test]
    fn image_boundaries() {
        for images in [
            json!(["DaTa:not-base64"]),
            json!([{"content_type":"","base64":""}]),
            json!(["data:", "DATA:", "data:", "data:"]),
        ] {
            let mut body = request(json!({"type":"text"}));
            body.as_object_mut().unwrap().remove("state");
            body["images"] = images;
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_ok());
            body["state"] = json!("");
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_ok());
        }
        for images in [
            json!(null),
            json!({}),
            json!(["data:", "data:", "data:", "data:", "data:"]),
            json!(["https://image"]),
            json!([1]),
            json!([{"content_type":"x"}]),
            json!([{"content_type":"x","base64":3}]),
            json!([{"content_type":"x","base64":"x","extra":true}]),
        ] {
            let mut body = request(json!({"type":"text"}));
            body["images"] = images;
            assert!(validate_extraction_request(&body, &FieldType::ALL).is_err());
        }
        let mut body = request(json!({"type":"text"}));
        body["images"] = json!([]);
        assert!(validate_extraction_request(&body, &FieldType::ALL).is_ok());
        body.as_object_mut().unwrap().remove("state");
        assert!(validate_extraction_request(&body, &FieldType::ALL).is_err());
    }

    // T50: unsupported declarations and API mismatch retain explicit identities.
    #[test]
    fn declared_types_and_other_api_key() {
        for kind in FieldType::ALL {
            let spec = match kind {
                FieldType::List => json!({"type":kind.name(),"items":"text"}),
                FieldType::Estimate => json!({"type":kind.name(),"min":0,"max":1}),
                FieldType::Forecast => json!({"type":kind.name(),"min":0,"max":1,"horizon":1}),
                _ => json!({"type":kind.name()}),
            };
            assert_eq!(
                validate_extraction_request(&request(spec), &[]),
                Err(ExtractionRequestRefusal::FieldTypeUnsupported {
                    field: "field".into(),
                    field_type: kind
                })
            );
        }
        let body = json!({"fields":null,"questions":null});
        assert_eq!(
            other_api_key(DeploymentApi::Decisions, &body),
            Some("fields")
        );
        assert_eq!(
            other_api_key(DeploymentApi::Extractions, &body),
            Some("questions")
        );
        assert_eq!(other_api_key(DeploymentApi::Chat, &body), None);
        assert_eq!(other_api_key(DeploymentApi::Embeddings, &body), None);
        assert_eq!(
            other_api_key(DeploymentApi::Extractions, &json!({"fields":{}})),
            None
        );
    }

    // T50: the shared completion envelope is nonempty and names each answer's type.
    #[test]
    fn completion_evidence() {
        for answer in [
            json!({"answers":{"x":{"type":"text"}}}),
            json!({"answers":{"x":{"type":"unknown","value":null}},"extra":true}),
        ] {
            assert!(extraction_answered(&answer));
        }
        for answer in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"answers":{}}),
            json!({"answers":{"x":null}}),
            json!({"answers":{"x":{}}}),
            json!({"answers":{"x":{"type":1}}}),
            json!({"answers":[]}),
            json!({"answers":null}),
        ] {
            assert!(!extraction_answered(&answer));
        }
    }
}
