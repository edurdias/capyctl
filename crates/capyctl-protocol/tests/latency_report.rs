//! SPEC §17 (M80): host latency deltas ride on W8 load reports as an additive,
//! bounded field. Series names come from a fixed set, so a report cannot grow
//! cardinality from request input.
use capyctl_domain::latency::Histogram;
use capyctl_protocol::{
    pb,
    reports::{LoadReport, LoadSample, SampleLatency, HOST_LATENCY_SERIES},
};

fn sample(latency: Option<SampleLatency>) -> LoadSample {
    LoadSample {
        deployment_id: "d".into(),
        generation: 3,
        owned_handle: "launch".into(),
        sampled_at_ms: 1_000,
        ingress_in_flight: 0,
        engine: None,
        latency,
    }
}
fn wire(latency: Option<SampleLatency>) -> pb::ReportLoad {
    LoadReport {
        host_id: "host".into(),
        samples: vec![sample(latency)],
    }
    .to_wire()
}
fn observed(values: &[f64]) -> Histogram {
    let mut h = Histogram::capyctl();
    for v in values {
        h.observe(*v);
    }
    h
}

// SPEC §17 T18: a latency delta round-trips; a report without one is unchanged.
#[test]
fn latency_round_trips_and_is_optional() {
    let latency = SampleLatency {
        engine: Some("sglang".into()),
        histograms: vec![
            ("ingress_time_to_first_byte".into(), observed(&[0.01, 0.2])),
            (
                "engine_queue_time".into(),
                Histogram::from_parts(vec![0.0, 0.001], vec![1, 0, 0], 0.0, 1).unwrap(),
            ),
        ],
    };
    let decoded = LoadReport::try_from(wire(Some(latency.clone()))).unwrap();
    assert_eq!(decoded.samples[0].latency.as_ref(), Some(&latency));
    let plain = LoadReport::try_from(wire(None)).unwrap();
    assert!(plain.samples[0].latency.is_none());
    assert_eq!(HOST_LATENCY_SERIES.len(), 9);
}

// SPEC §17: unknown or repeated series, an unknown engine, an empty delta and
// an inconsistent histogram each refuse the whole report.
#[test]
fn malformed_latency_refuses_the_report() {
    let good = || observed(&[0.1]);
    for histograms in [
        vec![("request_prompt".to_owned(), good())],
        vec![
            ("engine_queue_time".to_owned(), good()),
            ("engine_queue_time".to_owned(), good()),
        ],
        vec![("engine_queue_time".to_owned(), Histogram::capyctl())],
    ] {
        let report = wire(Some(SampleLatency {
            engine: None,
            histograms,
        }));
        assert!(LoadReport::try_from(report).is_err());
    }
    let mut unknown_engine = wire(Some(SampleLatency {
        engine: Some("vllm".into()),
        histograms: vec![],
    }));
    unknown_engine.samples[0].latency.as_mut().unwrap().engine = "trtllm".into();
    assert!(LoadReport::try_from(unknown_engine).is_err());
    let mut inconsistent = wire(Some(SampleLatency {
        engine: None,
        histograms: vec![("engine_queue_time".into(), good())],
    }));
    inconsistent.samples[0].latency.as_mut().unwrap().histograms[0].count = 7;
    assert!(LoadReport::try_from(inconsistent).is_err());
    let mut unbounded = wire(Some(SampleLatency {
        engine: None,
        histograms: vec![("engine_queue_time".into(), good())],
    }));
    let h = &mut unbounded.samples[0].latency.as_mut().unwrap().histograms[0];
    h.bounds = (0..65).map(f64::from).collect();
    h.counts = vec![0; 66];
    h.counts[0] = 1;
    assert!(LoadReport::try_from(unbounded).is_err());
}
