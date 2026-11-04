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

/// ADR 0041 §3: no per-answer judgement on the request path.
pub fn extraction_answered(answer: &Value) -> bool {
    answer
        .as_object()
        .and_then(|answer| answer.get("answers"))
        .is_some_and(Value::is_object)
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
    match body.get("state") {
        None if !images.is_empty() => {}
        Some(Value::String(state)) if !state.is_empty() || !images.is_empty() => {}
        _ => {
            return Err(invalid(
                "state must be a string, non-empty unless images is non-empty",
            ))
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
        invalid(format!("field `{name}` setting `{setting}` is not accepted for type `{}` or has an invalid value", kind.name()))
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
    if kind == Forecast && !spec.contains_key("horizon") {
        return Err(fail("horizon"));
    }
    let bounds = || -> Option<bool> {
        let (min, max) = (spec.get("min")?, spec.get("max")?);
        if let (Some(min), Some(max)) = (finite(min), finite(max)) {
            return (min < max).then_some(false);
        }
        let (min, max) = (min.as_str()?, max.as_str()?);
        (valid_date(min) && valid_date(max) && min < max).then_some(true)
    };
    for (setting, value) in spec {
        let valid = match setting.as_str() {
            "type" => true,
            "instructions" => value.is_string(),
            "null_threshold" => finite(value).is_some_and(|n| n > 0.0 && n <= 1.0),
            "unit" if matches!(kind, Number | Duration | Estimate | Forecast) => {
                value.as_str().is_some_and(|unit| {
                    kind != Duration
                        || [
                            "seconds", "minutes", "hours", "days", "weeks", "months", "years",
                        ]
                        .contains(&unit)
                })
            }
            "integer" if matches!(kind, Number | Duration) => value.is_boolean(),
            "decimals"
                if matches!(kind, Number | Duration)
                    || (kind == List
                        && spec.get("items").and_then(Value::as_str) == Some("number")) =>
            {
                value.as_u64().is_some_and(|n| n <= 6)
            }
            "items" if kind == List => value
                .as_str()
                .is_some_and(|s| ["text", "number", "date"].contains(&s)),
            "min" | "max" if matches!(kind, Estimate | Forecast) => bounds().is_some(),
            "step" if matches!(kind, Estimate | Forecast) => {
                finite(value).is_some_and(|n| n > 0.0)
                    || (bounds() == Some(true)
                        && value
                            .as_str()
                            .is_some_and(|s| ["month", "year"].contains(&s)))
            }
            "scale" if matches!(kind, Estimate | Forecast) => value.as_str().is_some_and(|s| {
                s == "linear"
                    || (s == "log"
                        && bounds() == Some(false)
                        && spec.get("min").and_then(finite).is_some_and(|n| n > 0.0))
            }),
            "horizon" if kind == Forecast => value.as_u64().is_some_and(|n| (1..=64).contains(&n)),
            "interval" if kind == Forecast => value.as_str().is_some_and(|s| {
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

    // T50: the setting/type matrix is closed, including otherwise valid values.
    #[test]
    fn rejects_wrong_type_settings() {
        let settings = [
            (
                "unit",
                json!("USD"),
                vec!["number", "duration", "estimate", "forecast"],
            ),
            ("integer", json!(true), vec!["number", "duration"]),
            ("decimals", json!(2), vec!["number", "duration", "list"]),
            ("items", json!("number"), vec!["list"]),
            ("min", json!(0), vec!["estimate", "forecast"]),
            ("max", json!(10), vec!["estimate", "forecast"]),
            ("step", json!(1), vec!["estimate", "forecast"]),
            ("scale", json!("linear"), vec!["estimate", "forecast"]),
            ("horizon", json!(2), vec!["forecast"]),
            ("interval", json!("day"), vec!["forecast"]),
            ("unknown", json!(true), vec![]),
        ];
        for kind in FieldType::ALL {
            for (key, value, allowed) in &settings {
                if allowed.contains(&kind.name()) {
                    continue;
                }
                let mut spec = match kind {
                    FieldType::List => json!({"type":kind.name(),"items":"text"}),
                    FieldType::Estimate => json!({"type":kind.name(),"min":0,"max":10}),
                    FieldType::Forecast => json!({"type":kind.name(),"min":0,"max":10,"horizon":1}),
                    _ => json!({"type":kind.name()}),
                };
                spec[*key] = value.clone();
                rejects(spec, key);
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
        rejects(json!({"type":"duration","unit":"day"}), "unit");
        for value in [json!(-1), json!(7), json!(1.5), json!("2"), Value::Null] {
            rejects(json!({"type":"number","decimals":value}), "decimals");
        }
        rejects(json!({"type":"list"}), "items");
        rejects(json!({"type":"list","items":"duration"}), "items");
        rejects(
            json!({"type":"list","items":"text","decimals":2}),
            "decimals",
        );
        rejects(json!({"type":"estimate","max":10}), "min");
        rejects(json!({"type":"estimate","min":0}), "max");
        for (min, max) in [
            (json!(1), json!(1)),
            (json!(2), json!(1)),
            (json!(0), json!("2026-01-01")),
            (json!("2026-01-01"), json!("2025-01-01")),
            (json!("2025-02-29"), json!("2026-01-01")),
        ] {
            rejects(json!({"type":"estimate","min":min,"max":max}), "max");
        }
        for step in [
            json!(0),
            json!(-1),
            json!("month"),
            json!("year"),
            Value::Null,
        ] {
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
        for min in [-1, 0] {
            rejects(
                json!({"type":"estimate","min":min,"max":10,"scale":"log"}),
                "scale",
            );
        }
        rejects(
            json!({"type":"estimate","min":"2024-01-01","max":"2025-01-01","scale":"log"}),
            "scale",
        );
        rejects(
            json!({"type":"estimate","min":0,"max":10,"scale":"other"}),
            "scale",
        );
        rejects(json!({"type":"forecast","min":0,"max":10}), "horizon");
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

    // T50: whole object with an answers object is sufficient, even empty.
    #[test]
    fn completion_evidence() {
        for answer in [
            json!({"answers":{}}),
            json!({"answers":{"x":null},"extra":true}),
        ] {
            assert!(extraction_answered(&answer));
        }
        for answer in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"answers":[]}),
            json!({"answers":null}),
        ] {
            assert!(!extraction_answered(&answer));
        }
    }
}
