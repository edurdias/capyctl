use harness::{f2_correctness, f2_streamed};
#[path = "../src/f2_sse.rs"]
mod f2_sse;
use f2_correctness::MarkerCase;
use f2_sse::MarkerSse;

fn decoder() -> MarkerSse {
    let case = MarkerCase::new(7).unwrap();
    assert!(case.prompt().contains("F2_MARKER_0007"));
    MarkerSse::new(case, "served").unwrap()
}
fn wire() -> String {
    concat!(": keepalive\n\ndata: {\"model\":\"served\",\"object\":\"chat.completion.chunk\",\n",
    "data: \"choices\":[{\"index\":0,\"delta\":{\"content\":\"F2_MARKER_0007\"},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n").into()
}
#[test]
fn accepts_every_network_split_and_crlf() {
    for wire in [
        wire(),
        wire().replace('\n', "\r\n"),
        wire().replacen('{', "{\"metadata\":\"€\",", 1),
    ] {
        for split in 0..=wire.len() {
            let mut s = decoder();
            s.push(&wire.as_bytes()[..split]).unwrap();
            s.push(&wire.as_bytes()[split..]).unwrap();
            s.complete().unwrap();
        }
    }
}

#[test]
fn bounds_multiline_events_and_rejects_data_after_terminal() {
    let mut s = decoder();
    s.push(format!("data: {}\n", " ".repeat(40_000)).as_bytes())
        .unwrap();
    assert!(s
        .push(format!("data: {}\n", " ".repeat(40_000)).as_bytes())
        .is_err());
    assert!(s.complete().is_err());
    let mut s = decoder();
    s.push(wire().as_bytes()).unwrap();
    assert!(s.push(b"data: [DONE]\n\n").is_err());
    assert!(s.complete().is_err());
}
#[test]
fn rejects_truncation_at_every_nonempty_suffix_of_terminal() {
    let wire = wire();
    for cut in 1..=14 {
        let mut s = decoder();
        let _ = s.push(&wire.as_bytes()[..wire.len() - cut]);
        assert!(s.complete().is_err());
    }
}
#[test]
fn rejects_invalid_utf8_unsupported_fields_and_lone_cr() {
    for bytes in [
        &b"data: \xff\n\n"[..],
        b"event: error\n\n",
        b"data: x\ry\n\n",
        b"data\n\n",
    ] {
        let mut s = decoder();
        assert!(s.push(bytes).is_err());
        assert!(s.push(wire().as_bytes()).is_err());
        assert!(s.complete().is_err());
    }
}
#[test]
fn bounds_lines_and_keepalive_only_streams() {
    for bytes in [vec![b'x'; 65_537], vec![b'\n'; 4097], vec![b' '; 1_048_577]] {
        assert!(decoder().push(&bytes).is_err());
    }
    let mut s = decoder();
    s.push(b": keepalive\n\n").unwrap();
    assert!(s.complete().is_err());
}
