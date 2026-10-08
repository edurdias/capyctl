//! SPEC §13.3 / T21: an engine log is redacted as it is written. Each test
//! launches a real child through the durable spawn with this test binary as
//! the redacting writer (`relay_fixture`), then greps the log for every secret.
use std::collections::BTreeMap;
use std::path::Path;

use capyctl_adapters::traits::RenderedCommand;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_launchers::engine_log_relay::{relay_main, LogRelay, LOG_VARIABLE};
use capyctl_launchers::{
    AssociationError, DurableSpawn, LaunchAssociation, ProtectedLaunchDescriptors,
};

/// The writer process: this binary re-run with only this test selected. It
/// does nothing in an ordinary test run, where the log variable is unset.
#[test]
fn relay_fixture() {
    if std::env::var_os(LOG_VARIABLE).is_some() {
        std::process::exit(relay_main());
    }
}

fn relay() -> LogRelay {
    LogRelay::new(
        std::env::current_exe().unwrap(),
        [
            "--exact",
            "relay_fixture",
            "--nocapture",
            "--test-threads=1",
        ],
    )
}

struct Accept;
impl LaunchAssociation for Accept {
    fn persist_api_identity(&self, _: &ProcessIdentity) -> Result<(), AssociationError> {
        Ok(())
    }
}

fn read_until_done(log: &Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains("engine-done") || std::time::Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

const INFERENCE_KEY: &str = "5e1f0c2b9a8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f";
const ADMIN_KEY: &str = "admin-key-not-hex-shaped-0001";
const OBSERVATION: &str = "observation-cred-0002";
const VLLM_KEY: &str = "vllm-inference-key-0003";
const VLLM_ADMIN: &str = "vllm-admin-key-0004";
const HF_TOKEN: &str = "hf_QwErTyUiOpAsDfGhJkLzXcVbNm0123456789";
const URL_PASSWORD: &str = "url-pass-0005";
const SIGNATURE: &str = "presigned-signature-0006";
const BEARER: &str = "bearer-token-0007";

/// Every owned secret form the engine might print: its keys read back from
/// the protected descriptors and its environment, a model source's token, URL
/// credentials, a presigned query and a bearer value.
fn noisy_engine(log: &Path, inference_fd: i32, admin_fd: i32) -> RenderedCommand {
    let script = format!(
        "echo \"server_args api_key=$(cat /proc/self/fd/{inference_fd}) admin=$(cat /proc/self/fd/{admin_fd})\"; \
         echo \"env $VLLM_API_KEY $CAPYCTL_VLLM_ADMIN_KEY\" >&2; \
         echo 'source token {HF_TOKEN}'; \
         echo 'fetch https://operator:{URL_PASSWORD}@models.example/org/m'; \
         echo 'GET https://bucket.example/w.safetensors?X-Amz-Signature={SIGNATURE}&X-Amz-Expires=60'; \
         printf 'Authorization: Bearer {BEARER}\\r'; \
         echo 'ordinary line kept'; echo engine-done"
    );
    RenderedCommand {
        argv: vec!["sh".into(), "-c".into(), script],
        env: BTreeMap::from([
            (LOG_VARIABLE.to_owned(), log.to_str().unwrap().to_owned()),
            ("VLLM_API_KEY".to_owned(), VLLM_KEY.to_owned()),
            ("CAPYCTL_VLLM_ADMIN_KEY".to_owned(), VLLM_ADMIN.to_owned()),
        ]),
    }
}

// T21: grep finds no secret in the log; the ordinary output is all there.
#[test]
fn a_launch_log_holds_no_owned_secret() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("logs").join("dep").join("inc.log");
    let descriptors =
        ProtectedLaunchDescriptors::new(b"{}", INFERENCE_KEY.as_bytes(), ADMIN_KEY.as_bytes())
            .unwrap();
    let [_, inference, admin] = descriptors.numbers()[..] else {
        panic!("three protected descriptors")
    };
    let launcher = DurableSpawn::new().with_log_relay(relay());
    launcher
        .spawn_protected(
            "relay-test",
            &noisy_engine(&log, inference, admin),
            &descriptors,
            &Accept,
        )
        .unwrap();
    let text = read_until_done(&log);
    assert!(
        text.contains("ordinary line kept") && text.contains("engine-done"),
        "{text}"
    );
    for secret in [
        INFERENCE_KEY,
        ADMIN_KEY,
        VLLM_KEY,
        VLLM_ADMIN,
        HF_TOKEN,
        URL_PASSWORD,
        SIGNATURE,
        BEARER,
    ] {
        assert!(
            !text.contains(secret),
            "{secret} written to the log:\n{text}"
        );
    }
    assert!(
        text.contains("https://<redacted>@models.example/org/m"),
        "{text}"
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

// T21: a group worker's observation credential is redacted the same way.
#[test]
fn a_worker_log_holds_no_observation_credential() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("worker.log");
    let descriptors =
        ProtectedLaunchDescriptors::for_worker(b"{}", Some(OBSERVATION.as_bytes())).unwrap();
    let [_, observation] = descriptors.numbers()[..] else {
        panic!("two protected descriptors")
    };
    let command = RenderedCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("echo \"observer $(cat /proc/self/fd/{observation})\"; echo engine-done"),
        ],
        env: BTreeMap::from([(LOG_VARIABLE.to_owned(), log.to_str().unwrap().to_owned())]),
    };
    DurableSpawn::new()
        .with_log_relay(relay())
        .spawn_protected("worker-relay", &command, &descriptors, &Accept)
        .unwrap();
    let text = read_until_done(&log);
    assert!(text.contains("observer <redacted>"), "{text}");
    assert!(!text.contains(OBSERVATION), "{text}");
}

// T21: the exec launcher writes through the same writer.
#[test]
fn the_exec_launcher_log_is_redacted_too() {
    use capyctl_adapters::traits::Launcher;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("exec.log");
    let command = RenderedCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "echo \"key $VLLM_API_KEY\"; echo engine-done".into(),
        ],
        env: BTreeMap::from([
            (LOG_VARIABLE.to_owned(), log.to_str().unwrap().to_owned()),
            ("VLLM_API_KEY".to_owned(), VLLM_KEY.to_owned()),
        ]),
    };
    let launcher = capyctl_launchers::ExecLauncher::new().with_log_relay(relay());
    launcher.spawn(&command).unwrap();
    let text = read_until_done(&log);
    assert!(text.contains("key <redacted>"), "{text}");
    assert!(!text.contains(VLLM_KEY), "{text}");
}
