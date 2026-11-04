//! ADR 0041 §§4–5: the fixed extraction readiness probe and wake canary.
//! The request is sent to the launch's loopback endpoint, never through ingress.
use capyctl_config::extractions::FieldType;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::traits::AdapterError;

pub const PATH: &str = "/v1/systemone";
pub const KIND: &str = "extraction";
pub const VERSION: u32 = 1;
pub const STATE: &str = "Order A-88412 was delivered on 2026-09-28 at 14:30 with a cracked screen and a dented corner. The customer paid 649.00 USD and wrote on 2026-10-09. Returns logged on the last five days: 12, 15, 11, 14, 13.";

// ADR 0041 §4: this order and every request member are part of probe version 1.
const FIELDS: [(FieldType, &str); 9] = [
    (FieldType::Text, "order_id"),
    (FieldType::Number, "amount_paid"),
    (FieldType::Date, "delivered_on"),
    (FieldType::Time, "delivered_time"),
    (FieldType::Datetime, "delivered_at"),
    (FieldType::Duration, "days_waited"),
    (FieldType::List, "damage"),
    (FieldType::Estimate, "returns_today"),
    (FieldType::Forecast, "returns_next"),
];
const PROBABILITIES: [&str; 4] = ["p_null", "confidence", "p_below", "p_above"];

/// The declared set and complete answers, suitable for recording a wake reference.
/// Unrecognized answer members are retained but never affect readiness or matching.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtractionAnswer {
    pub declared: Vec<FieldType>,
    pub answers: Map<String, Value>,
}

/// ADR 0041 §4: exactly one fixed field per declared type, in canonical order.
pub fn request(served: &str, declared: &[FieldType]) -> Value {
    let mut fields = Map::new();
    for (kind, name) in &FIELDS {
        if !declared.contains(kind) {
            continue;
        }
        let spec = match kind {
            FieldType::Text => json!({"type": "text", "instructions": "The order number"}),
            FieldType::Number => {
                json!({"type": "number", "decimals": 2, "unit": "USD", "instructions": "The amount the customer paid"})
            }
            FieldType::Date => json!({"type": "date", "instructions": "The delivery date"}),
            FieldType::Time => json!({"type": "time", "instructions": "The time of the delivery"}),
            FieldType::Datetime => {
                json!({"type": "datetime", "instructions": "The date and time of the delivery"})
            }
            FieldType::Duration => {
                json!({"type": "duration", "unit": "days", "integer": true, "instructions": "Days from the delivery to the date the customer wrote"})
            }
            FieldType::List => {
                json!({"type": "list", "items": "text", "instructions": "Each kind of damage described"})
            }
            FieldType::Estimate => {
                json!({"type": "estimate", "min": 0, "max": 100, "step": 1, "instructions": "Returns logged on the day the customer wrote"})
            }
            FieldType::Forecast => {
                json!({"type": "forecast", "horizon": 2, "interval": "day", "min": 0, "max": 100, "step": 1, "instructions": "Returns logged on each of the next two days"})
            }
        };
        fields.insert((*name).into(), spec);
    }
    json!({"model": served, "state": STATE, "reference_date": "2026-10-09", "fields": fields})
}

fn failure(field: &str, member: &str) -> String {
    format!("extraction probe: invalid {field}.{member}")
}

fn finite(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn probability(value: &Value) -> bool {
    finite(value).is_some_and(|number| (0.0..=1.0).contains(&number))
}

fn valid_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 5 || bytes[2] != b':' {
        return false;
    }
    let pair = |part: &[u8], max: u8| {
        part.len() == 2
            && part.iter().all(u8::is_ascii_digit)
            && (part[0] - b'0') * 10 + part[1] - b'0' <= max
    };
    if !pair(&bytes[..2], 23) || !pair(&bytes[3..5], 59) {
        return false;
    }
    bytes.len() == 5
        || (bytes.len() >= 8
            && bytes[5] == b':'
            && pair(&bytes[6..8], 59)
            && (bytes.len() == 8
                || (bytes.len() > 9
                    && bytes[8] == b'.'
                    && bytes[9..].iter().all(u8::is_ascii_digit))))
}

fn valid_datetime(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    if !crate::extraction_request::valid_date(date) {
        return false;
    }
    if let Some(time) = time.strip_suffix('Z') {
        return valid_time(time);
    }
    if let Some(offset) = time.find(['+', '-']) {
        return valid_time(&time[..offset]) && valid_time_offset(&time[offset + 1..]);
    }
    valid_time(time)
}

fn valid_time_offset(value: &str) -> bool {
    value.len() == 5 && valid_time(value)
}

fn valid_span(value: &Value, text: &str) -> bool {
    let Some(span) = value.as_array().filter(|span| span.len() == 2) else {
        return false;
    };
    let (Some(start), Some(end)) = (span[0].as_u64(), span[1].as_u64()) else {
        return false;
    };
    // ADR 0041 §4: STATE is ASCII, so byte and character offsets coincide.
    start < end && end <= STATE.len() as u64 && &STATE[start as usize..end as usize] == text
}

fn equal_value(left: &Value, right: &Value) -> bool {
    match (finite(left), finite(right)) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

fn validate_answers(answers: &Map<String, Value>, declared: &[FieldType]) -> Result<(), String> {
    if declared.is_empty()
        || declared
            .iter()
            .enumerate()
            .any(|(index, kind)| declared[..index].contains(kind))
    {
        return Err("extraction probe: invalid declared types".into());
    }
    if answers.len() != declared.len() {
        return Err("extraction probe: answers do not match the declared fields".into());
    }
    for (kind, name) in &FIELDS {
        if !declared.contains(kind) {
            continue;
        }
        let answer = answers
            .get(*name)
            .and_then(Value::as_object)
            .ok_or_else(|| failure(name, "answer"))?;
        if answer.get("type").and_then(Value::as_str) != Some(kind.name()) {
            return Err(failure(name, "type"));
        }
        // ADR 0041 §5: every reported probability participates in the canary.
        for member in PROBABILITIES {
            if let Some(value) = answer.get(member) {
                let valid = if *kind == FieldType::Forecast {
                    value
                        .as_array()
                        .is_some_and(|values| values.len() == 2 && values.iter().all(probability))
                        || probability(value)
                } else {
                    probability(value)
                };
                if !valid {
                    return Err(failure(name, member));
                }
            }
        }
        match kind {
            FieldType::Text
            | FieldType::Number
            | FieldType::Date
            | FieldType::Time
            | FieldType::Datetime
            | FieldType::Duration => {
                for member in ["p_null", "confidence"] {
                    if !answer.get(member).is_some_and(probability) {
                        return Err(failure(name, member));
                    }
                }
                let guess = answer
                    .get("guess")
                    .filter(|guess| !guess.is_null())
                    .ok_or_else(|| failure(name, "guess"))?;
                let valid = match kind {
                    FieldType::Text => guess.as_str().is_some_and(|text| {
                        answer
                            .get("span")
                            .is_some_and(|span| valid_span(span, text))
                    }),
                    FieldType::Number => finite(guess).is_some_and(|number| {
                        let scaled = 100.0 * number;
                        scaled.is_finite() && (scaled - scaled.round()).abs() <= 1e-6
                    }),
                    FieldType::Date => guess
                        .as_str()
                        .is_some_and(crate::extraction_request::valid_date),
                    FieldType::Time => guess.as_str().is_some_and(valid_time),
                    FieldType::Datetime => guess.as_str().is_some_and(valid_datetime),
                    FieldType::Duration => {
                        finite(guess).is_some_and(|number| number.fract() == 0.0)
                    }
                    _ => unreachable!(),
                };
                if !valid {
                    return Err(failure(name, "guess"));
                }
                if !answer
                    .get("value")
                    .is_some_and(|value| value.is_null() || equal_value(value, guess))
                {
                    return Err(failure(name, "value"));
                }
            }
            FieldType::List => {
                let values = answer
                    .get("value")
                    .and_then(Value::as_array)
                    .ok_or_else(|| failure(name, "value"))?;
                let spans = answer
                    .get("spans")
                    .and_then(Value::as_array)
                    .filter(|spans| spans.len() == values.len())
                    .ok_or_else(|| failure(name, "spans"))?;
                let mut previous = 0;
                for (value, span) in values.iter().zip(spans) {
                    if !value.as_str().is_some_and(|text| valid_span(span, text)) {
                        return Err(failure(name, "spans"));
                    }
                    let start = span[0].as_u64().expect("validated span");
                    if start < previous {
                        return Err(failure(name, "spans"));
                    }
                    previous = start;
                }
            }
            FieldType::Estimate | FieldType::Forecast => {
                for step in 0..if *kind == FieldType::Forecast { 2 } else { 1 } {
                    let number = |member: &str| -> Result<f64, String> {
                        let value = answer.get(member).ok_or_else(|| failure(name, member))?;
                        let value = if *kind == FieldType::Forecast {
                            &value
                                .as_array()
                                .filter(|values| values.len() == 2)
                                .ok_or_else(|| failure(name, member))?[step]
                        } else {
                            value
                        };
                        finite(value)
                            .filter(|number| {
                                (0.0..=100.0).contains(number) && number.fract() == 0.0
                            })
                            .ok_or_else(|| failure(name, member))
                    };
                    number("value")?;
                    let (low, median, high) = (number("p10")?, number("median")?, number("p90")?);
                    if low > median || median > high {
                        return Err(failure(name, "interval"));
                    }
                }
                if *kind == FieldType::Estimate {
                    for member in ["p_below", "p_above"] {
                        if !answer.get(member).is_some_and(probability) {
                            return Err(failure(name, member));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// ADR 0041 §4: judge shape and types, not agreement with the state's facts.
pub fn well_formed(answer: &Value, declared: &[FieldType]) -> Result<ExtractionAnswer, String> {
    let answers = answer
        .as_object()
        .and_then(|answer| answer.get("answers"))
        .and_then(Value::as_object)
        .ok_or_else(|| "extraction probe: no answers object".to_owned())?;
    validate_answers(answers, declared)?;
    Ok(ExtractionAnswer {
        declared: FIELDS
            .iter()
            .filter(|(kind, _)| declared.contains(kind))
            .map(|(kind, _)| *kind)
            .collect(),
        answers: answers.clone(),
    })
}

fn within(left: &Value, right: &Value, tolerance: f64) -> bool {
    match (finite(left), finite(right)) {
        (Some(left), Some(right)) => {
            let difference = (left - right).abs();
            difference <= tolerance || (tolerance > 0.0 && difference - tolerance <= f64::EPSILON)
        }
        _ => match (left.as_array(), right.as_array()) {
            (Some(left), Some(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| within(left, right, tolerance))
            }
            _ => false,
        },
    }
}

/// ADR 0041 §5: compare only the named evidence, using one grid step for
/// distributions, relative 1e-9 for scalar numbers, and `tolerance` for probabilities.
pub fn matches(
    reference: &ExtractionAnswer,
    later: &ExtractionAnswer,
    tolerance: f64,
) -> Result<(), String> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("extraction probe: invalid probability tolerance".into());
    }
    if reference.declared != later.declared {
        return Err("extraction probe: declared types changed".into());
    }
    validate_answers(&reference.answers, &reference.declared)?;
    validate_answers(&later.answers, &later.declared)?;
    for (kind, name) in &FIELDS {
        if !reference.declared.contains(kind) {
            continue;
        }
        let left = &reference.answers[*name];
        let right = &later.answers[*name];
        let same = match kind {
            FieldType::Text
            | FieldType::Number
            | FieldType::Date
            | FieldType::Time
            | FieldType::Datetime
            | FieldType::Duration => {
                let same_guess = if matches!(kind, FieldType::Number | FieldType::Duration) {
                    let (a, b) = (
                        left["guess"].as_f64().unwrap(),
                        right["guess"].as_f64().unwrap(),
                    );
                    (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
                } else {
                    left["guess"] == right["guess"]
                };
                left["value"].is_null() == right["value"].is_null()
                    && same_guess
                    && (*kind != FieldType::Text || left["span"] == right["span"])
            }
            FieldType::List => left["value"] == right["value"] && left["spans"] == right["spans"],
            FieldType::Estimate | FieldType::Forecast => ["value", "median", "p10", "p90"]
                .iter()
                .all(|member| within(&left[*member], &right[*member], 1.0)),
        };
        if !same {
            return Err(failure(name, "wake_mismatch"));
        }
        for member in PROBABILITIES {
            match (left.get(member), right.get(member)) {
                (None, None) => {}
                (Some(left), Some(right)) if within(left, right, tolerance) => {}
                _ => return Err(failure(name, member)),
            }
        }
    }
    Ok(())
}

/// Send the fixed probe using the launch's own endpoint and sensitive key.
pub(crate) async fn probe(
    client: &reqwest::Client,
    base: &reqwest::Url,
    key: Option<&reqwest::header::HeaderValue>,
    served: &str,
    declared: &[FieldType],
    bound: std::time::Duration,
) -> Result<ExtractionAnswer, AdapterError> {
    let url = base
        .join(PATH)
        .map_err(|_| AdapterError::Uncertain("extraction probe: no endpoint".into()))?;
    let response = crate::completion_probe::post_json(
        client,
        url,
        key,
        &request(served, declared),
        bound,
        "extraction probe",
    )
    .await?;
    well_formed(&response, declared).map_err(AdapterError::Uncertain)
}

/// Re-prove an adopted launch, marking its bearer header sensitive.
pub async fn probe_keyed(
    client: &reqwest::Client,
    base: &reqwest::Url,
    key: Option<&str>,
    served: &str,
    declared: &[FieldType],
    bound: std::time::Duration,
) -> Result<ExtractionAnswer, AdapterError> {
    let header = match key {
        Some(key) => Some(crate::completion_probe::bearer(key).ok_or_else(|| {
            AdapterError::Uncertain("extraction probe: the key is not a header value".into())
        })?),
        None => None,
    };
    probe(client, base, header.as_ref(), served, declared, bound).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str) -> Value {
        let start = STATE.find(text).unwrap();
        json!([start, start + text.len()])
    }

    fn engine() -> Value {
        json!({
            "model": "served",
            "answers": {
                "order_id": {"type": "text", "value": "A-88412", "guess": "A-88412", "span": [6, 13], "p_null": 0.1, "confidence": 0.5},
                "amount_paid": {"type": "number", "value": 649.0, "guess": 649.0, "p_null": 0.1, "confidence": 0.5},
                "delivered_on": {"type": "date", "value": "2026-09-28", "guess": "2026-09-28", "p_null": 0.1, "confidence": 0.5},
                "delivered_time": {"type": "time", "value": "14:30", "guess": "14:30", "p_null": 0.1, "confidence": 0.5},
                "delivered_at": {"type": "datetime", "value": "2026-09-28T14:30", "guess": "2026-09-28T14:30", "p_null": 0.1, "confidence": 0.5},
                "days_waited": {"type": "duration", "value": 11, "guess": 11, "p_null": 0.1, "confidence": 0.5},
                "damage": {"type": "list", "value": ["cracked screen", "dented corner"], "spans": [span("cracked screen"), span("dented corner")], "confidence": 0.5},
                "returns_today": {"type": "estimate", "value": 13, "median": 13, "p10": 10, "p90": 16, "p_below": 0.1, "p_above": 0.1},
                "returns_next": {"type": "forecast", "value": [13, 14], "median": [13, 14], "p10": [10, 11], "p90": [16, 17], "p_below": [0.1, 0.2], "p_above": [0.1, 0.2]}
            },
            "usage": {"input_tokens": 100}
        })
    }

    fn ready(body: &Value) -> ExtractionAnswer {
        well_formed(body, &FieldType::ALL).unwrap()
    }

    fn changed(field: &str, member: &str, value: Value) -> Value {
        let mut body = engine();
        body["answers"][field][member] = value;
        body
    }

    fn scalar_guess(field: &str, guess: Value) -> Value {
        let mut body = changed(field, "guess", guess.clone());
        body["answers"][field]["value"] = guess;
        body
    }

    // T50: the versioned request is exactly ADR 0041 §4, not a generated prompt.
    #[test]
    fn exact_fixed_request() {
        assert_eq!(PATH, "/v1/systemone");
        assert_eq!(KIND, "extraction");
        assert_eq!(VERSION, 1);
        assert_eq!(
            request("served", &FieldType::ALL),
            json!({
                "model": "served",
                "state": "Order A-88412 was delivered on 2026-09-28 at 14:30 with a cracked screen and a dented corner. The customer paid 649.00 USD and wrote on 2026-10-09. Returns logged on the last five days: 12, 15, 11, 14, 13.",
                "reference_date": "2026-10-09",
                "fields": {
                    "order_id": {"type": "text", "instructions": "The order number"},
                    "amount_paid": {"type": "number", "decimals": 2, "unit": "USD", "instructions": "The amount the customer paid"},
                    "delivered_on": {"type": "date", "instructions": "The delivery date"},
                    "delivered_time": {"type": "time", "instructions": "The time of the delivery"},
                    "delivered_at": {"type": "datetime", "instructions": "The date and time of the delivery"},
                    "days_waited": {"type": "duration", "unit": "days", "integer": true, "instructions": "Days from the delivery to the date the customer wrote"},
                    "damage": {"type": "list", "items": "text", "instructions": "Each kind of damage described"},
                    "returns_today": {"type": "estimate", "min": 0, "max": 100, "step": 1, "instructions": "Returns logged on the day the customer wrote"},
                    "returns_next": {"type": "forecast", "horizon": 2, "interval": "day", "min": 0, "max": 100, "step": 1, "instructions": "Returns logged on each of the next two days"}
                }
            })
        );
    }

    // T50: subsets use the same fields and canonical declaration order.
    #[test]
    fn declared_types_choose_exactly_their_fields() {
        for (kind, field) in FIELDS {
            let request = request("served", &[kind]);
            let fields = request["fields"].as_object().unwrap();
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[field]["type"], kind.name());
            let answer = json!({"answers": {field: engine()["answers"][field].clone()}});
            assert_eq!(well_formed(&answer, &[kind]).unwrap().declared, [kind]);
        }
        let body = json!({"answers": {
            "returns_today": engine()["answers"]["returns_today"].clone(),
            "order_id": engine()["answers"]["order_id"].clone()
        }});
        assert_eq!(
            well_formed(&body, &[FieldType::Estimate, FieldType::Text])
                .unwrap()
                .declared,
            [FieldType::Text, FieldType::Estimate]
        );
        assert_eq!(
            request("served", &[FieldType::Text, FieldType::Text])["fields"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
    }

    // T50: missing, extra and wrongly typed answers are not readiness.
    #[test]
    fn exact_answer_shape_is_required() {
        for body in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"answers": []}),
            json!({"answers": {}}),
        ] {
            assert!(well_formed(&body, &FieldType::ALL).is_err());
        }
        for (_, field) in FIELDS {
            let mut missing = engine();
            missing["answers"].as_object_mut().unwrap().remove(field);
            assert!(well_formed(&missing, &FieldType::ALL).is_err());
            assert!(well_formed(&changed(field, "type", json!("other")), &FieldType::ALL).is_err());
            let mut wrong_name = engine();
            let value = wrong_name["answers"]
                .as_object_mut()
                .unwrap()
                .remove(field)
                .unwrap();
            wrong_name["answers"]["unexpected"] = value;
            assert!(well_formed(&wrong_name, &FieldType::ALL).is_err());
        }
        assert!(well_formed(&engine(), &[]).is_err());
        assert!(well_formed(&engine(), &[FieldType::Text, FieldType::Text]).is_err());
    }

    // T50: readiness proves well-formed answers, not their correctness.
    #[test]
    fn null_values_and_incorrect_but_well_formed_guesses_are_ready() {
        let mut body = engine();
        for (_, field) in &FIELDS[..6] {
            body["answers"][*field]["value"] = Value::Null;
        }
        body["answers"]["amount_paid"]["guess"] = json!(-42.25);
        body["answers"]["days_waited"]["guess"] = json!(-99);
        body["answers"]["delivered_on"]["guess"] = json!("2000-02-29");
        body["answers"]["delivered_time"]["guess"] = json!("00:00:00.123456");
        body["answers"]["delivered_at"]["guess"] = json!("2000-02-29T23:59:59.1-23:59");
        assert!(well_formed(&body, &FieldType::ALL).is_ok());
        let encoded = serde_json::to_value(ready(&body)).unwrap();
        assert_eq!(
            serde_json::from_value::<ExtractionAnswer>(encoded).unwrap(),
            ready(&body)
        );
    }

    // T50: each scalar requires a nonnull guess, value and valid probabilities.
    #[test]
    fn scalar_members_and_probabilities_are_checked() {
        for (_, field) in &FIELDS[..6] {
            for member in ["value", "guess", "p_null", "confidence"] {
                let mut body = engine();
                body["answers"][*field]
                    .as_object_mut()
                    .unwrap()
                    .remove(member);
                assert!(
                    well_formed(&body, &FieldType::ALL).is_err(),
                    "{field}.{member}"
                );
            }
            assert!(well_formed(&changed(field, "guess", Value::Null), &FieldType::ALL).is_err());
            assert!(well_formed(
                &changed(field, "value", json!("not the guess")),
                &FieldType::ALL
            )
            .is_err());
            for member in ["p_null", "confidence"] {
                for value in [json!(-0.001), json!(1.001), json!("0.5"), Value::Null] {
                    assert!(well_formed(&changed(field, member, value), &FieldType::ALL).is_err());
                }
                for value in [json!(0), json!(1)] {
                    assert!(well_formed(&changed(field, member, value), &FieldType::ALL).is_ok());
                }
            }
        }
    }

    // T50: numeric guesses obey the cents precision and integer-duration rules.
    #[test]
    fn numeric_precision_is_checked() {
        for guess in [json!(0), json!(-1.25), json!(1.230000001), json!(1e20)] {
            assert!(well_formed(&scalar_guess("amount_paid", guess), &FieldType::ALL).is_ok());
        }
        for guess in [
            json!(1.234),
            json!(1.2300001),
            json!("1.23"),
            json!(true),
            json!(1e308),
        ] {
            assert!(well_formed(&scalar_guess("amount_paid", guess), &FieldType::ALL).is_err());
        }
        assert!(well_formed(&scalar_guess("days_waited", json!(1.5)), &FieldType::ALL).is_err());
        assert!(well_formed(&scalar_guess("days_waited", json!("11")), &FieldType::ALL).is_err());
        // Integer and floating-point spellings denote the same numeric value.
        assert!(well_formed(
            &changed("amount_paid", "value", json!(649)),
            &FieldType::ALL
        )
        .is_ok());
    }

    // T50: calendar and clock syntax is strict, including leap days and offsets.
    #[test]
    fn dates_times_and_datetimes_are_checked() {
        for (field, valid, invalid) in [
            (
                "delivered_on",
                vec!["2000-02-29", "2026-12-31"],
                vec![
                    "1900-02-29",
                    "2026-04-31",
                    "2026-2-03",
                    "2026-00-01",
                    "2026-01-00",
                    "2026-01-01Z",
                ],
            ),
            (
                "delivered_time",
                vec!["00:00", "23:59:59", "01:02:03.123456789"],
                vec![
                    "24:00",
                    "12:60",
                    "12:00:60",
                    "1:02",
                    "12:00:00.",
                    "12:00Z",
                    "12:00:00.x",
                ],
            ),
            (
                "delivered_at",
                vec![
                    "2000-02-29T00:00",
                    "2026-09-28T23:59:59.99Z",
                    "2026-09-28T14:30+05:30",
                ],
                vec![
                    "1900-02-29T00:00",
                    "2026-09-28 14:30",
                    "2026-09-28T24:00",
                    "2026-09-28T14:30+24:00",
                    "2026-09-28T14:30+05:60",
                    "2026-09-28T14:30+05:30:00",
                    "2026-09-28T14:30z",
                ],
            ),
        ] {
            for guess in valid {
                assert!(
                    well_formed(&scalar_guess(field, json!(guess)), &FieldType::ALL).is_ok(),
                    "{guess}"
                );
            }
            for guess in invalid {
                assert!(
                    well_formed(&scalar_guess(field, json!(guess)), &FieldType::ALL).is_err(),
                    "{guess}"
                );
            }
        }
    }

    // T50: text spans must be nonempty in-bounds integer spans of STATE.
    #[test]
    fn text_spans_are_checked() {
        for span in [
            json!([6, 6]),
            json!([13, 6]),
            json!([-1, 13]),
            json!([6, STATE.len() + 1]),
            json!([6.5, 13]),
            json!([6]),
            json!([6, 13, 14]),
            json!([0, 7]),
        ] {
            assert!(well_formed(&changed("order_id", "span", span), &FieldType::ALL).is_err());
        }
        assert!(well_formed(
            &scalar_guess("order_id", json!("not in state")),
            &FieldType::ALL
        )
        .is_err());
        let mut body = scalar_guess("order_id", json!(STATE));
        body["answers"]["order_id"]["span"] = json!([0, STATE.len()]);
        assert!(well_formed(&body, &FieldType::ALL).is_ok());
    }

    // T50: lists permit empty and overlapping spans, but not decreasing starts.
    #[test]
    fn list_items_spans_and_optional_confidence_are_checked() {
        let mut empty = engine();
        empty["answers"]["damage"] = json!({"type": "list", "value": [], "spans": []});
        assert!(well_formed(&empty, &FieldType::ALL).is_ok());
        let mut repeated = engine();
        repeated["answers"]["damage"]["value"] = json!(["cracked screen", "cracked screen"]);
        repeated["answers"]["damage"]["spans"] =
            json!([span("cracked screen"), span("cracked screen")]);
        assert!(well_formed(&repeated, &FieldType::ALL).is_ok());
        for (member, value) in [
            ("value", Value::Null),
            ("value", json!([1, 2])),
            ("spans", json!([])),
            ("spans", json!([[0, 1], [1, 2]])),
            ("confidence", json!(1.1)),
            ("confidence", Value::Null),
        ] {
            assert!(well_formed(&changed("damage", member, value), &FieldType::ALL).is_err());
        }
        let mut reversed = engine();
        reversed["answers"]["damage"]["value"] = json!(["dented corner", "cracked screen"]);
        reversed["answers"]["damage"]["spans"] =
            json!([span("dented corner"), span("cracked screen")]);
        assert!(well_formed(&reversed, &FieldType::ALL).is_err());
    }

    // T50: all estimate components and every forecast step obey bounds and grid.
    #[test]
    fn estimates_and_forecasts_are_checked() {
        for field in ["returns_today", "returns_next"] {
            for member in ["value", "median", "p10", "p90"] {
                for invalid in [json!(-1), json!(101), json!(13.5), json!("13"), Value::Null] {
                    let value = if field == "returns_next" {
                        json!([13, invalid])
                    } else {
                        invalid
                    };
                    assert!(
                        well_formed(&changed(field, member, value), &FieldType::ALL).is_err(),
                        "{field}.{member}"
                    );
                }
            }
        }
        for (field, member, value) in [
            ("returns_today", "p10", json!(14)),
            ("returns_today", "p90", json!(12)),
            ("returns_next", "p10", json!([10, 15])),
            ("returns_next", "p90", json!([16, 13])),
            ("returns_today", "p_below", json!(-0.1)),
            ("returns_today", "p_above", json!(1.1)),
            ("returns_next", "p_below", json!([0.1, 1.1])),
            ("returns_next", "p_above", json!([0.1])),
        ] {
            assert!(well_formed(&changed(field, member, value), &FieldType::ALL).is_err());
        }
        for member in ["value", "median", "p10", "p90"] {
            for invalid in [json!([]), json!([13]), json!([13, 14, 15]), json!(13)] {
                assert!(
                    well_formed(&changed("returns_next", member, invalid), &FieldType::ALL)
                        .is_err()
                );
            }
        }
        let mut endpoints = engine();
        endpoints["answers"]["returns_today"] = json!({"type": "estimate", "value": 100, "median": 50, "p10": 0, "p90": 100, "p_below": 0, "p_above": 1});
        // Forecast probabilities are optional in the readiness table (§4).
        endpoints["answers"]["returns_next"]
            .as_object_mut()
            .unwrap()
            .remove("p_below");
        endpoints["answers"]["returns_next"]
            .as_object_mut()
            .unwrap()
            .remove("p_above");
        assert!(well_formed(&endpoints, &FieldType::ALL).is_ok());
    }

    // T50: undocumented metadata is ignored for readiness and the canary.
    #[test]
    fn extra_members_do_not_affect_evidence() {
        let reference = ready(&engine());
        let mut later = engine();
        later["usage"] = json!({"other": true});
        for (_, field) in FIELDS {
            later["answers"][field]["mean"] = json!("ignored");
            later["answers"][field]["quantiles"] = Value::Null;
            later["answers"][field]["unit"] = json!(42);
        }
        assert!(matches(&reference, &ready(&later), 0.05).is_ok());
    }

    // T50: scalar values retain nullness and exact guesses except numeric relative error.
    #[test]
    fn canary_detects_changed_scalar_evidence() {
        let reference = ready(&engine());
        for (_, field) in &FIELDS[..6] {
            assert!(matches(
                &reference,
                &ready(&changed(field, "value", Value::Null)),
                0.05
            )
            .is_err());
        }
        for (field, guess) in [
            ("amount_paid", json!(650)),
            ("days_waited", json!(12)),
            ("delivered_on", json!("2026-09-29")),
            ("delivered_time", json!("14:31")),
            ("delivered_at", json!("2026-09-28T14:31")),
        ] {
            assert!(matches(&reference, &ready(&scalar_guess(field, guess)), 0.05).is_err());
        }
        let mut text = scalar_guess("order_id", json!("Order"));
        text["answers"]["order_id"]["span"] = json!([0, 5]);
        assert!(matches(&reference, &ready(&text), 0.05).is_err());
        let null = ready(&changed("amount_paid", "value", Value::Null));
        assert!(matches(&null, &null, 0.05).is_ok());
        let large = ready(&scalar_guess("amount_paid", json!(1_000_000_000)));
        assert!(matches(
            &large,
            &ready(&scalar_guess("amount_paid", json!(1_000_000_001))),
            0.05
        )
        .is_ok());
        assert!(matches(
            &large,
            &ready(&scalar_guess("amount_paid", json!(1_000_000_002))),
            0.05
        )
        .is_err());
    }

    // T50: a text's span and list order remain part of the reference.
    #[test]
    fn canary_compares_spans_and_list_order() {
        let mut first = scalar_guess("order_id", json!("on"));
        first["answers"]["order_id"]["span"] = span("on");
        let mut second = first.clone();
        let offset = STATE.rfind("on").unwrap();
        second["answers"]["order_id"]["span"] = json!([offset, offset + 2]);
        assert!(matches(&ready(&first), &ready(&second), 0.05).is_err());
        let mut list = engine();
        list["answers"]["damage"]["value"] = json!(["cracked screen"]);
        list["answers"]["damage"]["spans"] = json!([span("cracked screen")]);
        assert!(matches(&ready(&engine()), &ready(&list), 0.05).is_err());
    }

    // T50: each distribution component independently allows exactly one probe step.
    #[test]
    fn canary_allows_one_step_but_not_two() {
        let reference = ready(&engine());
        for member in ["value", "median", "p10", "p90"] {
            for field in ["returns_today", "returns_next"] {
                let original = engine()["answers"][field][member].clone();
                for delta in [-1, 1, 2] {
                    let value = if field == "returns_next" {
                        json!([original[0], original[1].as_i64().unwrap() + delta])
                    } else {
                        json!(original.as_i64().unwrap() + delta)
                    };
                    assert_eq!(
                        matches(&reference, &ready(&changed(field, member, value)), 0.05).is_ok(),
                        delta != 2,
                        "{field}.{member} delta {delta}"
                    );
                }
            }
        }
    }

    // T50: every probability, including each forecast step and optional confidence,
    // participates in the tolerance; changing presence is not the same evidence.
    #[test]
    fn canary_compares_every_probability() {
        let reference = ready(&engine());
        for (field, member) in [
            ("order_id", "p_null"),
            ("amount_paid", "confidence"),
            ("damage", "confidence"),
            ("returns_today", "p_below"),
            ("returns_today", "p_above"),
            ("returns_next", "p_below"),
            ("returns_next", "p_above"),
        ] {
            let original = engine()["answers"][field][member].clone();
            for (delta, expected) in [(0.05, true), (0.051, false)] {
                let value = if field == "returns_next" {
                    json!([original[0], original[1].as_f64().unwrap() + delta])
                } else {
                    json!(original.as_f64().unwrap() + delta)
                };
                assert_eq!(
                    matches(&reference, &ready(&changed(field, member, value)), 0.05).is_ok(),
                    expected,
                    "{field}.{member} delta {delta}"
                );
            }
        }
        let mut absent = engine();
        absent["answers"]["damage"]
            .as_object_mut()
            .unwrap()
            .remove("confidence");
        assert!(matches(&reference, &ready(&absent), 0.05).is_err());
        assert!(matches(&reference, &reference, 0.0).is_ok());
        assert!(matches(
            &reference,
            &ready(&changed("damage", "confidence", json!(0.500001))),
            0.0
        )
        .is_err());
    }

    // T50: a different declared set or corrupted relayed reference is never compared.
    #[test]
    fn canary_rejects_incompatible_and_malformed_references() {
        let reference = ready(&engine());
        let subset = well_formed(
            &json!({"answers": {"order_id": engine()["answers"]["order_id"].clone()}}),
            &[FieldType::Text],
        )
        .unwrap();
        assert!(matches(&reference, &subset, 0.05).is_err());
        for tolerance in [-0.1, f64::NAN, f64::INFINITY] {
            assert!(matches(&reference, &reference, tolerance).is_err());
        }
        let mut corrupt = reference.clone();
        corrupt.answers.get_mut("returns_next").unwrap()["median"] = json!([]);
        assert!(matches(&reference, &corrupt, 0.05).is_err());
        assert!(matches(&corrupt, &reference, 0.05).is_err());
    }
}
