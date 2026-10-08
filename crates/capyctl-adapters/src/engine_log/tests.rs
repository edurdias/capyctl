use super::*;

const ENGINE_KEY: &str = "0f3c9a5e7b1d2c4f6a8e0b2d4f6a8c0e1f3a5b7c9d0e2f4a6b8c0d1e3f5a7b9c";
const ADMIN_KEY: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

fn redactor() -> LogRedactor {
    let mut redactor = LogRedactor::new();
    redactor.own(ENGINE_KEY);
    redactor.own(ADMIN_KEY);
    redactor
}

// T21 / SPEC §13.3: a launch secret is redacted by value, whatever its shape,
// and the text around it survives.
#[test]
fn an_owned_secret_is_redacted_by_value_in_any_shape() {
    let mut redactor = LogRedactor::new();
    redactor.own("short-observer-cred");
    let line = "observer joined with short-observer-cred at port 9";
    let out = redactor.redact(line);
    assert!(!out.contains("short-observer-cred"), "{out}");
    assert_eq!(out, "observer joined with <redacted> at port 9");
}

// T21: the engine echoes its arguments, keys included, at its default level.
#[test]
fn an_argument_echo_carries_no_key() {
    let line = format!(
        "server_args={{'api_key': '{ENGINE_KEY}', 'admin_api_key': '{ADMIN_KEY}', 'model_path': '/m'}}"
    );
    let out = redactor().redact(&line);
    assert!(
        !out.contains(ENGINE_KEY) && !out.contains(ADMIN_KEY),
        "{out}"
    );
    assert!(out.contains("'model_path': '/m'"), "{out}");
}

// T21: every owned secret form named by SPEC §13.3 leaves no trace.
#[test]
fn every_secret_form_is_redacted() {
    let forms = [
        // A model source's token, by shape and by name.
        (
            "hf_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            "token hf_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 accepted",
        ),
        ("s3cr3t-pass", "HF_TOKEN=s3cr3t-pass"),
        (
            "p4ssw0rd",
            "fetch https://operator:p4ssw0rd@models.example/org/model failed",
        ),
        (
            "deadbeefsig",
            "GET https://bucket.example/w.safetensors?X-Amz-Signature=deadbeefsig&X-Amz-Expires=60",
        ),
        ("tok3n-value-1", "Authorization: Bearer tok3n-value-1"),
        ("dXNlcjpwYXNz", "authorization: Basic dXNlcjpwYXNz"),
        ("swordfish99", "\"password\": \"swordfish99\""),
        ("abcdefgh12345", "SGLANG_API_KEY=abcdefgh12345 set"),
        (
            "cGFzc3dvcmQtc2hhcGVkLWNyZWRlbnRpYWwtdGhhdC1pcy1sb25n",
            "observation cGFzc3dvcmQtc2hhcGVkLWNyZWRlbnRpYWwtdGhhdC1pcy1sb25n",
        ),
    ];
    let redactor = LogRedactor::new();
    for (secret, line) in forms {
        let out = redactor.redact(line);
        assert!(!out.contains(secret), "{line} -> {out}");
        assert!(out.contains(REDACTED), "{line} -> {out}");
    }
}

// Ordinary engine output stays legible: names, paths, revisions, None values.
#[test]
fn ordinary_output_is_kept() {
    let redactor = redactor();
    for line in [
        "INFO: 127.0.0.1:5000 - \"POST /v1/chat/completions HTTP/1.1\" 200",
        "Loading weights from /var/lib/capyctl/models/org/model/snapshots/0123456789abcdef0123456789abcdef01234567",
        "api_key=None ssl_keyfile=None",
        "eos_token='</s>' max_tokens=128",
        "Missing key: model.layers.0.weight",
    ] {
        assert_eq!(redactor.redact(line), line);
    }
}

// The writer process receives the owned values encoded; nothing is lost.
#[test]
fn owned_values_survive_encoding() {
    let mut original = LogRedactor::new();
    original.own(ENGINE_KEY);
    original.own("short-observer-cred");
    original.own("tiny");
    let decoded = LogRedactor::decode(&original.encode());
    let line = format!("{ENGINE_KEY} short-observer-cred tiny");
    assert_eq!(decoded.redact(&line), "<redacted> <redacted> tiny");
}

#[test]
fn secret_named_variables_are_owned() {
    let env = BTreeMap::from([
        ("VLLM_API_KEY".to_owned(), "engine-key-value".to_owned()),
        (
            "CAPYCTL_VLLM_ADMIN_KEY".to_owned(),
            "admin-key-value".to_owned(),
        ),
        (
            "CAPYCTL_ENGINE_LOG".to_owned(),
            "/var/log/engine.log".to_owned(),
        ),
    ]);
    let mut redactor = LogRedactor::new();
    redactor.own_environment(&env);
    let out = redactor.redact("engine-key-value admin-key-value /var/log/engine.log");
    assert_eq!(out, "<redacted> <redacted> /var/log/engine.log");
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

// T21: the tail is bounded, starts at a whole line and is redacted again.
#[test]
fn a_tail_is_bounded_and_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("launch.log");
    let mut text = String::new();
    for n in 0..4000 {
        text.push_str(&format!("line {n} of ordinary output\n"));
    }
    text.push_str(&format!(
        "echo {ENGINE_KEY}\nfetch https://u:p@h.example/x?sig=1\n"
    ));
    write(&log, &text);
    let tail = read_tail(&log, 1024).unwrap();
    assert!(tail.truncated);
    assert!(tail.text.len() <= 1024, "{}", tail.text.len());
    assert!(tail.text.starts_with("line "), "{}", tail.text);
    assert!(!tail.text.contains(ENGINE_KEY));
    assert!(!tail.text.contains("u:p@") && !tail.text.contains("sig=1"));
    assert!(tail
        .text
        .ends_with("fetch https://<redacted>@h.example/x?<redacted>\n"));
    // A request beyond the bound is clamped to it.
    let tail = read_tail(&log, usize::MAX).unwrap();
    assert!(tail.text.len() <= TAIL_MAX_BYTES);
}

#[test]
fn a_short_tail_continues_into_the_rotated_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("launch.log");
    write(&rotated(&log, 1), "older one\nolder two\n");
    write(&log, "newer\n");
    let tail = read_tail(&log, TAIL_DEFAULT_BYTES).unwrap();
    assert_eq!(tail.text, "older one\nolder two\nnewer\n");
    assert!(!tail.truncated);
}

// SPEC §13.3: raw development logs are never served; an absent log is named.
#[test]
fn a_raw_or_missing_log_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("launch.log");
    assert_eq!(read_tail(&log, 1024), Err(TailError::Missing));
    write(&log, "full debug output\n");
    write(&raw_marker(&log), "");
    assert_eq!(read_tail(&log, 1024), Err(TailError::Raw));
}

// T21: a tail never exceeds its bound, even where redaction lengthened the
// lines; what remains is whole lines, and the cut is reported.
#[test]
fn a_redaction_never_lengthens_the_tail_past_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("launch.log");
    write(&log, &"api_key=a\n".repeat(64));
    let tail = read_tail(&log, 256).unwrap();
    assert!(tail.text.len() <= 256, "{}", tail.text.len());
    assert!(tail.truncated);
    assert!(
        tail.text.lines().all(|line| line == "api_key=<redacted>"),
        "{}",
        tail.text
    );
    write(&log, "short\n");
    let whole = read_tail(&log, 256).unwrap();
    assert_eq!((whole.text.as_str(), whole.truncated), ("short\n", false));
}
