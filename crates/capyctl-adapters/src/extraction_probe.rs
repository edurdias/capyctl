//! ADR 0041 §§4–5: the fixed extraction readiness probe and wake canary.
//! The request is sent to the launch's loopback endpoint, never through ingress.
use capyctl_config::extractions::{FieldType, PROBE_FIELDS as FIELDS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::traits::AdapterError;

pub const PATH: &str = "/v1/systemone";
pub const KIND: &str = "extraction";
pub const VERSION: u32 = 1;
pub const STATE: &str = "Order A-88412 was delivered on 2026-09-28 at 14:30 with a cracked screen and a dented corner. The customer paid 649.00 USD and wrote on 2026-10-09. Returns logged on the last five days: 12, 15, 11, 14, 13.";

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

fn valid_clock(value: &str) -> bool {
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

fn valid_time(value: &str) -> bool {
    if let Some(clock) = value.strip_suffix('Z') {
        return valid_clock(clock);
    }
    if let Some(offset) = value.find(['+', '-']) {
        let zone = &value[offset + 1..];
        return valid_clock(&value[..offset]) && zone.len() == 5 && valid_clock(zone);
    }
    valid_clock(value)
}

fn valid_datetime(value: &str) -> bool {
    value
        .split_once('T')
        .is_some_and(|(date, time)| crate::extraction_request::valid_date(date) && valid_time(time))
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

const COMPONENTS: [&str; 4] = ["value", "median", "p10", "p90"];

fn validate_distribution(values: [&Value; 4], name: &str) -> Result<(), String> {
    for (member, value) in COMPONENTS.iter().zip(values) {
        if !finite(value)
            .is_some_and(|number| (0.0..=100.0).contains(&number) && number.fract() == 0.0)
        {
            return Err(failure(name, member));
        }
    }
    let [_, median, low, high] = values.map(|value| value.as_f64().unwrap());
    if low > median || median > high {
        return Err(failure(name, "interval"));
    }
    Ok(())
}

fn validate_estimate(answer: &Map<String, Value>, name: &str) -> Result<(), String> {
    let component = |member| answer.get(member).ok_or_else(|| failure(name, member));
    validate_distribution(
        [
            component("value")?,
            component("median")?,
            component("p10")?,
            component("p90")?,
        ],
        name,
    )
}

fn compare_probabilities(
    left: &Value,
    right: &Value,
    tolerance: f64,
    name: &str,
) -> Result<(), String> {
    for member in PROBABILITIES {
        match (left.get(member), right.get(member)) {
            (None, None) => {}
            (Some(left), Some(right)) if within(left, right, tolerance) => {}
            _ => return Err(failure(name, member)),
        }
    }
    Ok(())
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
                if answer.get("value").is_some_and(Value::is_null) {
                    if answer.get("spans").is_some_and(|spans| !spans.is_null()) {
                        return Err(failure(name, "spans"));
                    }
                    continue;
                }
                let values = answer
                    .get("value")
                    .and_then(Value::as_array)
                    .ok_or_else(|| failure(name, "value"))?;
                let spans = answer
                    .get("spans")
                    .and_then(Value::as_array)
                    .filter(|spans| spans.len() == values.len())
                    .ok_or_else(|| failure(name, "spans"))?;
                for (value, span) in values.iter().zip(spans) {
                    if !value.as_str().is_some_and(|text| valid_span(span, text)) {
                        return Err(failure(name, "spans"));
                    }
                }
            }
            FieldType::Estimate => {
                validate_estimate(answer, name)?;
                for member in ["p_below", "p_above"] {
                    if !answer.get(member).is_some_and(probability) {
                        return Err(failure(name, member));
                    }
                }
            }
            FieldType::Forecast => {
                // ADR 0041 §4: either representation suffices; every present
                // representation must be valid, even if the other one is valid.
                let arrays = COMPONENTS.iter().any(|member| answer.contains_key(*member));
                if arrays {
                    let component = |member| {
                        answer
                            .get(member)
                            .and_then(Value::as_array)
                            .filter(|values| values.len() == 2)
                            .ok_or_else(|| failure(name, member))
                    };
                    let values = [
                        component("value")?,
                        component("median")?,
                        component("p10")?,
                        component("p90")?,
                    ];
                    for step in 0..2 {
                        validate_distribution(values.map(|values| &values[step]), name)?;
                    }
                }
                if let Some(steps) = answer.get("steps") {
                    let steps = steps
                        .as_array()
                        .filter(|steps| steps.len() == 2)
                        .ok_or_else(|| failure(name, "steps"))?;
                    for step in steps {
                        let step = step.as_object().ok_or_else(|| failure(name, "steps"))?;
                        validate_estimate(step, name)?;
                        for member in PROBABILITIES {
                            if step.get(member).is_some_and(|value| !probability(value)) {
                                return Err(failure(name, member));
                            }
                        }
                    }
                } else if !arrays {
                    return Err(failure(name, "forecast representation"));
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
            difference <= tolerance
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
/// Borrow the stored reference directly, without copying its complete answers.
pub fn matches(
    reference_declared: &[FieldType],
    reference_answers: &Map<String, Value>,
    later: &ExtractionAnswer,
    tolerance: f64,
) -> Result<(), String> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return Err("extraction probe: invalid probability tolerance".into());
    }
    if reference_declared != later.declared.as_slice() {
        return Err("extraction probe: declared types changed".into());
    }
    validate_answers(reference_answers, reference_declared)?;
    validate_answers(&later.answers, &later.declared)?;
    for (kind, name) in &FIELDS {
        if !reference_declared.contains(kind) {
            continue;
        }
        let left = &reference_answers[*name];
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
                same_guess && (*kind != FieldType::Text || left["span"] == right["span"])
            }
            FieldType::List => left["value"] == right["value"] && left["spans"] == right["spans"],
            FieldType::Estimate => COMPONENTS
                .iter()
                .all(|member| within(&left[*member], &right[*member], 1.0)),
            FieldType::Forecast => {
                let arrays_match = !left.as_object().unwrap().contains_key("value")
                    || COMPONENTS
                        .iter()
                        .all(|member| within(&left[*member], &right[*member], 1.0));
                let steps_match = if let Some(steps) = left.get("steps").and_then(Value::as_array) {
                    if let Some(later) = right.get("steps").and_then(Value::as_array) {
                        steps.iter().zip(later).all(|(left, right)| {
                            COMPONENTS
                                .iter()
                                .all(|member| within(&left[*member], &right[*member], 1.0))
                        })
                    } else {
                        false
                    }
                } else {
                    true
                };
                arrays_match && steps_match
            }
        };
        if !same {
            return Err(failure(name, "wake_mismatch"));
        }
        compare_probabilities(left, right, tolerance, name)?;
        if *kind == FieldType::Forecast {
            if let Some(steps) = left.get("steps").and_then(Value::as_array) {
                for (step, later) in steps.iter().zip(right["steps"].as_array().unwrap()) {
                    compare_probabilities(step, later, tolerance, name)?;
                }
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
                vec![
                    "00:00",
                    "23:59:59",
                    "01:02:03.123456789",
                    "12:00Z",
                    "12:00+05:30",
                    "23:59:59.123-23:59",
                ],
                vec![
                    "24:00",
                    "12:60",
                    "12:00:60",
                    "1:02",
                    "12:00:00.",
                    "12:00+24:00",
                    "12:00-05:60",
                    "12:00+05:30Z",
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

    // T50: lists permit empty, overlapping and non-monotonic spans.
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
        assert!(well_formed(&reversed, &FieldType::ALL).is_ok());
        let reference = ready(&engine());
        assert!(matches(
            &reference.declared,
            &reference.answers,
            &ready(&reversed),
            0.05
        )
        .is_err());
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
        assert!(matches(
            &reference.declared,
            &reference.answers,
            &ready(&later),
            0.05
        )
        .is_ok());
    }

    // T50: scalar canaries compare guesses, not threshold-dependent value nullness.
    #[test]
    fn canary_detects_changed_scalar_evidence() {
        let reference = ready(&engine());
        for (_, field) in &FIELDS[..6] {
            assert!(matches(
                &reference.declared,
                &reference.answers,
                &ready(&changed(field, "value", Value::Null)),
                0.05
            )
            .is_ok());
        }
        for (field, guess) in [
            ("amount_paid", json!(650)),
            ("days_waited", json!(12)),
            ("delivered_on", json!("2026-09-29")),
            ("delivered_time", json!("14:31")),
            ("delivered_at", json!("2026-09-28T14:31")),
        ] {
            assert!(matches(
                &reference.declared,
                &reference.answers,
                &ready(&scalar_guess(field, guess)),
                0.05
            )
            .is_err());
        }
        let mut text = scalar_guess("order_id", json!("Order"));
        text["answers"]["order_id"]["span"] = json!([0, 5]);
        assert!(matches(&reference.declared, &reference.answers, &ready(&text), 0.05).is_err());
        let null = ready(&changed("amount_paid", "value", Value::Null));
        assert!(matches(&null.declared, &null.answers, &null, 0.05).is_ok());
        let large = ready(&scalar_guess("amount_paid", json!(1_000_000_000)));
        assert!(matches(
            &large.declared,
            &large.answers,
            &ready(&scalar_guess("amount_paid", json!(1_000_000_001))),
            0.05
        )
        .is_ok());
        assert!(matches(
            &large.declared,
            &large.answers,
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
        let reference = ready(&first);
        assert!(matches(
            &reference.declared,
            &reference.answers,
            &ready(&second),
            0.05
        )
        .is_err());
        let mut list = engine();
        list["answers"]["damage"]["value"] = json!(["cracked screen"]);
        list["answers"]["damage"]["spans"] = json!([span("cracked screen")]);
        let reference = ready(&engine());
        assert!(matches(&reference.declared, &reference.answers, &ready(&list), 0.05).is_err());
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
                        matches(
                            &reference.declared,
                            &reference.answers,
                            &ready(&changed(field, member, value)),
                            0.05
                        )
                        .is_ok(),
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
            // Compare from zero so the boundary is exactly the stored f64
            // tolerance, not a rounded decimal subtraction such as 0.55 - 0.5.
            let mut baseline = engine();
            baseline["answers"][field][member] = if field == "returns_next" {
                json!([0.0, 0.0])
            } else {
                json!(0.0)
            };
            let reference = ready(&baseline);
            for (delta, expected) in [
                (0.05, true),
                (f64::from_bits(0.05f64.to_bits() + 1), false),
                (0.051, false),
            ] {
                let mut later = baseline.clone();
                later["answers"][field][member] = if field == "returns_next" {
                    json!([0.0, delta])
                } else {
                    json!(delta)
                };
                assert_eq!(
                    matches(
                        &reference.declared,
                        &reference.answers,
                        &ready(&later),
                        0.05
                    )
                    .is_ok(),
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
        assert!(matches(
            &reference.declared,
            &reference.answers,
            &ready(&absent),
            0.05
        )
        .is_err());
        assert!(matches(&reference.declared, &reference.answers, &reference, 0.0).is_ok());
        assert!(matches(
            &reference.declared,
            &reference.answers,
            &ready(&changed("damage", "confidence", json!(0.500001))),
            0.0
        )
        .is_err());
    }

    // T50 / ADR 0041 §5: inclusive means the exact tolerance, not an epsilon wider.
    #[test]
    fn numeric_tolerances_are_exactly_inclusive() {
        for tolerance in [0.05_f64, 1.0] {
            let outside = f64::from_bits(tolerance.to_bits() + 1);
            assert!(within(&json!(0.0), &json!(tolerance), tolerance));
            assert!(!within(&json!(0.0), &json!(outside), tolerance));
            assert!(within(&json!([0.0]), &json!([tolerance]), tolerance));
            assert!(!within(&json!([0.0]), &json!([outside]), tolerance));
        }
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
        assert!(matches(&reference.declared, &reference.answers, &subset, 0.05).is_err());
        for tolerance in [-0.1, f64::NAN, f64::INFINITY] {
            assert!(matches(
                &reference.declared,
                &reference.answers,
                &reference,
                tolerance
            )
            .is_err());
        }
        let mut corrupt = reference.clone();
        corrupt.answers.get_mut("returns_next").unwrap()["median"] = json!([]);
        assert!(matches(&reference.declared, &reference.answers, &corrupt, 0.05).is_err());
        assert!(matches(&corrupt.declared, &corrupt.answers, &reference, 0.05).is_err());
    }

    fn forecast_steps() -> Value {
        json!({
            "type": "forecast",
            "steps": [
                {"value": 13, "median": 13, "p10": 10, "p90": 16, "p_below": 0.1, "p_above": 0.1},
                {"value": 14, "median": 14, "p10": 11, "p90": 17, "p_below": 0.2, "p_above": 0.2}
            ]
        })
    }

    fn with_forecast(forecast: Value) -> Value {
        let mut body = engine();
        body["answers"]["returns_next"] = forecast;
        body
    }

    // T50: null lists are ready with absent/null spans, never with an array.
    #[test]
    fn null_lists_are_well_formed() {
        let mut body = engine();
        body["answers"]["damage"] = json!({"type": "list", "value": null});
        let absent = ready(&body);
        body["answers"]["damage"]["spans"] = Value::Null;
        let null = ready(&body);
        assert!(matches(&absent.declared, &absent.answers, &null, 0.05).is_ok());
        assert!(matches(&null.declared, &null.answers, &absent, 0.05).is_ok());
        assert!(matches(&null.declared, &null.answers, &ready(&engine()), 0.05).is_err());
        body["answers"]["damage"]["spans"] = json!([]);
        assert!(well_formed(&body, &FieldType::ALL).is_err());
    }

    // T50: either forecast representation is ready; all present forms are judged.
    #[test]
    fn forecast_steps_and_dual_representations_are_checked() {
        assert!(well_formed(&with_forecast(forecast_steps()), &FieldType::ALL).is_ok());
        let mut without_probabilities = forecast_steps();
        for step in without_probabilities["steps"].as_array_mut().unwrap() {
            step.as_object_mut().unwrap().remove("p_below");
            step.as_object_mut().unwrap().remove("p_above");
        }
        assert!(well_formed(&with_forecast(without_probabilities), &FieldType::ALL).is_ok());
        let mut both = engine()["answers"]["returns_next"].clone();
        both["steps"] = forecast_steps()["steps"].clone();
        assert!(well_formed(&with_forecast(both.clone()), &FieldType::ALL).is_ok());
        for invalid in [json!([]), json!([{}]), json!([{}, {}, {}]), Value::Null] {
            let mut forecast = both.clone();
            forecast["steps"] = invalid;
            assert!(well_formed(&with_forecast(forecast), &FieldType::ALL).is_err());
        }
        for member in COMPONENTS {
            let mut forecast = both.clone();
            forecast[member] = json!([13]);
            assert!(well_formed(&with_forecast(forecast), &FieldType::ALL).is_err());
            let mut forecast = both.clone();
            forecast["steps"][1].as_object_mut().unwrap().remove(member);
            assert!(well_formed(&with_forecast(forecast), &FieldType::ALL).is_err());
        }
        for member in COMPONENTS {
            for invalid in [json!(-1), json!(101), json!(1.5), json!("14"), Value::Null] {
                let mut forecast = forecast_steps();
                forecast["steps"][1][member] = invalid;
                assert!(well_formed(&with_forecast(forecast), &FieldType::ALL).is_err());
            }
        }
        for (member, value) in [
            ("p10", json!(15)),
            ("p90", json!(13)),
            ("p_below", json!(-0.1)),
            ("p_above", json!(1.1)),
            ("confidence", Value::Null),
        ] {
            let mut forecast = forecast_steps();
            forecast["steps"][1][member] = value;
            assert!(well_formed(&with_forecast(forecast), &FieldType::ALL).is_err());
        }
        assert!(well_formed(&with_forecast(json!({"type": "forecast"})), &FieldType::ALL).is_err());
        let mut partial = forecast_steps();
        partial["value"] = json!([13, 14]);
        assert!(well_formed(&with_forecast(partial), &FieldType::ALL).is_err());
        for member in ["p_below", "p_above"] {
            let mut forecast = forecast_steps();
            forecast[member] = json!(0.0);
            let reference = ready(&with_forecast(forecast.clone()));
            forecast[member] = json!(0.05);
            let boundary = ready(&with_forecast(forecast.clone()));
            assert!(matches(&reference.declared, &reference.answers, &boundary, 0.05).is_ok());
            forecast[member] = json!(f64::from_bits(0.05f64.to_bits() + 1));
            assert!(matches(
                &reference.declared,
                &reference.answers,
                &ready(&with_forecast(forecast.clone())),
                0.05
            )
            .is_err());
            for invalid in [json!(-0.1), json!(1.1), Value::Null] {
                forecast[member] = invalid;
                assert!(well_formed(&with_forecast(forecast.clone()), &FieldType::ALL).is_err());
            }
        }
    }

    // T50: each step component has the same one-step tolerance as array forecasts.
    #[test]
    fn forecast_step_canary_checks_values_and_probabilities() {
        let reference = ready(&with_forecast(forecast_steps()));
        for member in COMPONENTS {
            for delta in [-1, 1, 2] {
                let mut forecast = forecast_steps();
                let previous = forecast["steps"][1][member].as_i64().unwrap();
                forecast["steps"][1][member] = json!(previous + delta);
                assert_eq!(
                    matches(
                        &reference.declared,
                        &reference.answers,
                        &ready(&with_forecast(forecast)),
                        0.05
                    )
                    .is_ok(),
                    delta != 2,
                    "{member} delta {delta}"
                );
            }
        }
        for member in ["p_below", "p_above"] {
            let mut baseline = forecast_steps();
            baseline["steps"][1][member] = json!(0.0);
            let reference = ready(&with_forecast(baseline.clone()));
            for (value, expected) in [(0.05, true), (f64::from_bits(0.05f64.to_bits() + 1), false)]
            {
                let mut forecast = baseline.clone();
                forecast["steps"][1][member] = json!(value);
                assert_eq!(
                    matches(
                        &reference.declared,
                        &reference.answers,
                        &ready(&with_forecast(forecast)),
                        0.05
                    )
                    .is_ok(),
                    expected,
                    "{member}"
                );
            }
            let mut forecast = forecast_steps();
            forecast["steps"][1].as_object_mut().unwrap().remove(member);
            assert!(matches(
                &reference.declared,
                &reference.answers,
                &ready(&with_forecast(forecast)),
                0.05
            )
            .is_err());
        }
        let mut extra_probability = forecast_steps();
        extra_probability["steps"][0]["confidence"] = json!(0.5);
        assert!(matches(
            &reference.declared,
            &reference.answers,
            &ready(&with_forecast(extra_probability)),
            0.05
        )
        .is_err());
    }

    // T50: later forecasts must retain every reference representation, while a newly
    // added valid form is not compared against a form the reference never recorded.
    #[test]
    fn forecast_canary_uses_each_reference_representation() {
        let arrays = ready(&engine());
        let mut steps_forecast = forecast_steps();
        steps_forecast["p_below"] = json!([0.1, 0.2]);
        steps_forecast["p_above"] = json!([0.1, 0.2]);
        let steps = ready(&with_forecast(steps_forecast));
        let mut dual = engine()["answers"]["returns_next"].clone();
        dual["steps"] = forecast_steps()["steps"].clone();
        let both = ready(&with_forecast(dual.clone()));
        assert!(matches(&arrays.declared, &arrays.answers, &both, 0.05).is_ok());
        assert!(matches(&steps.declared, &steps.answers, &both, 0.05).is_ok());
        assert!(matches(&both.declared, &both.answers, &arrays, 0.05).is_err());
        assert!(matches(&both.declared, &both.answers, &steps, 0.05).is_err());
        assert!(matches(&arrays.declared, &arrays.answers, &steps, 0.05).is_err());
        assert!(matches(&steps.declared, &steps.answers, &arrays, 0.05).is_err());
        dual["steps"][1]["value"] = json!(16);
        let changed_steps = ready(&with_forecast(dual.clone()));
        assert!(matches(&arrays.declared, &arrays.answers, &changed_steps, 0.05).is_ok());
        assert!(matches(&both.declared, &both.answers, &changed_steps, 0.05).is_err());
        dual["steps"] = forecast_steps()["steps"].clone();
        dual["value"] = json!([13, 16]);
        let changed_arrays = ready(&with_forecast(dual));
        assert!(matches(&steps.declared, &steps.answers, &changed_arrays, 0.05).is_ok());
        assert!(matches(&both.declared, &both.answers, &changed_arrays, 0.05).is_err());
    }
}
