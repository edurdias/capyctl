//! The SGLang Initialize step (Spec §4). The builder does the whole step:
//! render the protected command, spawn through the director's tool with the
//! three protected descriptors, wait for readiness while watching the process,
//! probe once, enumerate the group, and report identities and facts.
//!
//! The builder never learns where identities are recorded: the tool the
//! director hands it makes the spawn durable, so a step that dies mid-launch
//! still leaves an owner behind an identity somebody else wrote down.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use mllm_domain::completion::{
    EffectObservation, ExecutionIdentities, Milestone, Presence, ProcessIdentity,
    StepExecutionContext,
};
use mllm_domain::launch::LaunchSettings;

use crate::protected::ProtectedLaunchDescriptors;
use crate::traits::{
    ChatForward, EngineAdapter, MemberRef, Readiness, RuntimeCommand, RuntimeError,
};
use crate::vllm::args::redact_text;

use super::adapter::SglangAdapter;
use super::args::ProtectedDescriptorFds;

/// How often the builder asks the engine whether it is serving the model.
/// SGLang's startup is minutes of weight staging on this hardware, so a
/// tighter poll only buys load on the engine's own event loop.
const READINESS_POLL: Duration = Duration::from_millis(500);

/// How far ahead of the coordinator's bound the builder gives up. The builder's
/// reason — deadline, dead process, unanswered probe — is worth more than a bare
/// timeout, so it must be the one that arrives first (Spec §4).
const BUILDER_MARGIN_MS: i64 = 2_000;

/// Lines of engine log quoted when a launch fails.
const LOG_TAIL_LINES: usize = 20;

/// The most log bytes read for a tail. An engine that logged a gigabyte before
/// dying must not be read into memory to explain itself.
const LOG_TAIL_BYTES: u64 = 64 * 1024;

/// The private descriptor the launcher reads on fd 3.
///
/// Wire contract with `runtime/sglang_entry.py`: exactly the schema version 2
/// shape the entry validates, whose `launch_scope` carries the same fields as
/// the controller's shared builder (`native_launch.rs::private_descriptor`),
/// cross-checked by the entry's `_validate_launch_scope` against the public
/// settings' binding id and incarnation. The session ULID is the coordinator
/// session the spawn factory threaded onto this adapter; the rest comes from
/// the step execution context the coordinator armed. Never log the output: it
/// carries the checkpoint root.
fn private_descriptor(
    session_id: &str,
    context: &StepExecutionContext,
    checkpoint_root: &str,
    public_settings: &serde_json::Value,
    placement_digest: Option<&str>,
) -> Result<Vec<u8>, RuntimeError> {
    let mut descriptor = json!({
        "schema_version": 2,
        "kind": "sglang_private_launch",
        "checkpoint_root": checkpoint_root,
        "public_settings": public_settings,
        "launch_scope": {
            "session_id": session_id,
            "deployment_id": context.token.deployment_id,
            "operation_id": context.token.operation_id,
            "step_id": context.token.step_id,
            "revision": context.token.revision,
            "generation": context.token.generation,
            "binding_id": context.binding_id,
            "incarnation": context.incarnation,
            "issued_at_ms": context.issued_at_ms,
            "deadline_ms": context.deadline_ms,
        },
    });
    if let Some(digest) = placement_digest {
        descriptor
            .as_object_mut()
            .ok_or_else(|| RuntimeError::Uncertain("descriptor encoding failed".into()))?
            .insert("placement_digest".into(), json!(digest));
    }
    serde_json::to_vec(&descriptor)
        .map_err(|_| RuntimeError::Uncertain("descriptor encoding failed".into()))
}

pub(super) async fn initialize(
    adapter: &SglangAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    // Unsupported when any part is missing: a builder without its launch, its
    // tools or its credentials cannot launch anything, and must not half-run
    // the step.
    let (launch, tools, inference, admin) = adapter.launch_parts()?;
    if !matches!(context.launch_settings, Some(LaunchSettings::Sglang(_)))
        || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    // One launch per incarnation: a repeat would start a second engine holding
    // the same device memory while the first is still recorded as owned.
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;
    let wrapper = adapter.wrapper_path()?.to_path_buf();
    let session = adapter.session()?;

    // SPEC §13.3: the private descriptor and the two credentials ride protected
    // descriptors the launcher hands the child; nothing enters argv or env.
    let private = private_descriptor(
        session,
        context,
        launch.frozen.checkpoint_root(),
        launch.rendered.public_metadata(),
        launch.frozen.metadata().placement_digest.as_deref(),
    )?;
    let descriptors =
        ProtectedLaunchDescriptors::new(&private, inference.as_bytes(), admin.as_bytes()).map_err(
            |e| RuntimeError::Uncertain(redact_text(&format!("protected descriptors: {e}"))),
        )?;
    let [launch_fd, inference_fd, admin_fd] = descriptors.numbers();
    let fds = ProtectedDescriptorFds::for_launcher(
        i64::from(launch_fd),
        i64::from(inference_fd),
        i64::from(admin_fd),
    )?;
    let mut cmd = launch.rendered.render_for_launcher(fds, &wrapper)?;
    // SPEC §13.3: tools come from the selected installation and fixed system
    // directories, never the caller's shell PATH. FlashInfer needs venv ninja.
    let engine_bin = std::path::Path::new(launch.frozen.executable())
        .parent()
        .ok_or(RuntimeError::Unsupported)?;
    // SPEC §13.3 as amended 2026-09-25: the profile's CUDA bin, when it names
    // one, follows the engine's own bin (engine_env.rs).
    let engine_bin = engine_bin.to_str().ok_or(RuntimeError::Unsupported)?;
    let tool_path = crate::engine_env::tool_path(
        (!engine_bin.is_empty()).then_some(engine_bin),
        launch.frozen.cuda_home(),
        "/usr/bin:/bin",
    );
    cmd.env.insert("PATH".into(), tool_path);
    // Owner decision 2026-09-25: JIT build jobs follow free memory at launch.
    let (toolchain, limits) = crate::engine_env::toolchain_environment(
        launch.frozen.cuda_home(),
        launch.frozen.build_env(),
        crate::engine_env::mem_available_bytes(),
        crate::engine_env::cpu_count(),
    );
    eprintln!("{limits} (binding {})", context.binding_id);
    cmd.env.extend(toolchain);
    // SPEC §9.1 / T21: neither the entry nor any engine child writes bytecode
    // beside mllm's checked runtime source.
    cmd.env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    // ADR 0014 §8, SPEC §8.2: the host's approvals for sensitive extras.
    if let Some(approvals) = adapter.extra_approvals() {
        cmd.env.insert(
            mllm_config::engine_policy::EXTRA_APPROVALS_ENV.into(),
            approvals.into(),
        );
    }
    let debug_logs = std::env::var("MLLM_DEBUG_ENGINE_LOGS").as_deref() == Ok("1");
    if debug_logs {
        cmd.env.insert("MLLM_DEBUG_ENGINE_LOGS".into(), "1".into());
    }
    if let Some(log) = adapter.engine_log() {
        cmd.env.insert("MLLM_ENGINE_LOG".into(), log.to_string());
    }
    // SPEC §9.2: a memory-saver launch enrolls its saver observation in the
    // host's private directory (the entry revalidates it and scopes it to this
    // binding); without one the launch serves and Park is refused unchanged.
    if let (true, Some(dir)) = (
        launch.frozen.settings().memory_saver,
        adapter.observation_dir(),
    ) {
        let dir = dir.to_str().ok_or(RuntimeError::Unsupported)?;
        cmd.env.insert("MLLM_OBSERVATION_DIR".into(), dir.into());
    }
    // SPEC §8.2 / T21: a host-named rendezvous directory, removed by the host
    // on gone evidence; without one the entry makes its own temporary one.
    if let Some(dir) = adapter.rendezvous_dir() {
        let dir = dir.to_str().ok_or(RuntimeError::Unsupported)?;
        cmd.env.insert("MLLM_RENDEZVOUS_DIR".into(), dir.into());
    }
    // SPEC §3: the profile env allowlist rejects `CUDA_VISIBLE_DEVICES` by name
    // (`engine_policy.rs::SAFE_ENV`), so the guarded launcher sets it here as a
    // launch parameter, from the host policy's service-frozen device mapping —
    // the same physical UUID the native placement gate corroborates against the
    // published inventory digest. Setting this environment before any CUDA
    // import is the guarded-service obligation `runtime/sglang_device.py`
    // names; the entry observes the namespace, it never invents it. Absent
    // when the host published no inventory UUID, which leaves the namespace
    // unset and placement fail-closed.
    let device = &launch.frozen.metadata().device;
    if let Some(uuid) = device.physical_gpu_uuid.as_deref() {
        cmd.env.insert("CUDA_VISIBLE_DEVICES".into(), uuid.into());
    } else if let Some(index) = device.cuda_pci_index {
        // Discrete GPU design §7 (controller ruling): a host with a choice of
        // GPU that published no UUID pins the selected one by its index, in
        // the PCI bus order `nvidia-smi` published it under.
        let namespace = mllm_config::effective::CudaNamespace::PciIndex(index);
        for (name, value) in namespace.environment() {
            cmd.env.insert(name.into(), value);
        }
    }

    // The tool is synchronous on purpose (mllm-launchers has no runtime), so every
    // call into it leaves the async threads free.
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    let api = tokio::task::spawn_blocking(move || {
        spawn_tools.spawn_durable_protected(&incarnation, &cmd, &descriptors)
    })
    .await
    .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;

    // Spec §4: the builder ends before the coordinator's bound so its own error wins.
    let stop_at = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
    let member = MemberRef {
        deployment_id: context.token.deployment_id.clone(),
        member_id: context.binding_id.clone(),
    };
    loop {
        // Each poll is bounded by the smaller of the model list's own timeout
        // and the builder's remaining budget: one stalled answer must become a
        // reported poll failure, never a hang past the coordinator's bound.
        let remaining_ms = (stop_at - now_ms()?).max(0) as u64;
        let poll_budget = Duration::from_millis(remaining_ms).min(super::http::MODELS_TIMEOUT);
        match tokio::time::timeout(poll_budget, adapter.check_readiness(&member)).await {
            // Spec §4: the builder's own bound ends first, so its reason, not a
            // bare coordinator timeout, is what gets recorded.
            Err(_elapsed) => {
                return Err(RuntimeError::Uncertain(
                    "readiness poll deadline reached with the engine alive".into(),
                ))
            }
            Ok(Ok(Readiness::Ready)) => break,
            Ok(Ok(Readiness::Initializing)) => {}
            // Spec §3: both exits from this step are journaled, so both pass
            // redaction. Display, not Debug, so the reason carries one prefix
            // rather than nesting this one inside the error's own.
            Ok(Err(e)) => {
                return Err(RuntimeError::Uncertain(redact_text(&format!(
                    "readiness: {e}"
                ))))
            }
        }
        // Spec §4 step 4: a process that left is the answer, and waiting out the
        // deadline would only delay it.
        let presence_tools = tools.clone();
        let watched = api.clone();
        match tokio::task::spawn_blocking(move || presence_tools.present(&watched))
            .await
            .map_err(|_| RuntimeError::Uncertain("presence task failed".into()))?
        {
            Presence::Alive => {}
            // SPEC §§6.4, 13.2: an engine that left before readiness is a
            // launch failure with its own reason, not ownership uncertainty.
            // The summary names refused options only, never values, so it is
            // read from the tail even when the full log is retained privately.
            Presence::Gone => {
                let tail = log_tail(adapter.engine_log(), LOG_TAIL_LINES);
                let summary = crate::launch_failure::summary(&tail, None);
                return Err(RuntimeError::LaunchFailed(format!(
                    "{summary}; log tail:\n{}",
                    if debug_logs {
                        "full native log retained privately; omitted from public diagnostics".into()
                    } else {
                        tail
                    }
                )));
            }
            // Unknown is retained, never absent: the step fails but says why.
            Presence::Unknown => {
                return Err(RuntimeError::Uncertain(
                    "engine presence could not be established during readiness".into(),
                ))
            }
        }
        if now_ms()? >= stop_at {
            return Err(RuntimeError::Uncertain(
                "readiness deadline reached with the engine alive".into(),
            ));
        }
        tokio::time::sleep(READINESS_POLL).await;
    }

    // SPEC §6.1: the served model in the list is not a model that answers. One
    // probe, through the same authenticated path inference will use.
    let body = json!({
        "model": adapter.served_name(),
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    // Spec §4: every wait is bounded by the context deadline, and the builder's own
    // waits end first so its reason, not a bare coordinator timeout, is what gets
    // recorded. The chat client carries its own much longer bounds, so a model that
    // lists itself and then stalls on its first completion would otherwise hand the
    // outcome to `drive`.
    let probe_budget = Duration::from_millis(u64::try_from(stop_at - now_ms()?).unwrap_or(0));
    let answer = tokio::time::timeout(probe_budget, adapter.forward_chat(&body))
        .await
        .map_err(|_| {
            RuntimeError::Uncertain("probe deadline reached with the engine alive".into())
        })?
        .map_err(|e| {
            RuntimeError::Uncertain(redact_text(&format!(
                "engine listed the model but did not answer: {e:?}"
            )))
        })?;
    if answer["choices"][0]["message"]["content"]
        .as_str()
        .is_none_or(str::is_empty)
    {
        return Err(RuntimeError::Uncertain(
            "engine answered with empty content".into(),
        ));
    }

    let group_tools = tools.clone();
    let led_by = api.clone();
    let identities = tokio::task::spawn_blocking(move || group_tools.observe_group(&led_by))
        .await
        .map_err(|_| RuntimeError::Uncertain("group task failed".into()))??;
    // A group without its API process, or without a single worker, is not the
    // deployment this step was asked to build; reporting it would record an
    // ownership set that does not cover the processes holding the device.
    if identities.first().map(|i| i.role.as_str()) != Some("api") || identities.len() < 2 {
        return Err(RuntimeError::Uncertain(format!(
            "engine group incomplete: {}",
            roles(&identities)
        )));
    }

    Ok(EffectObservation {
        token: context.token.clone(),
        binding_id: context.binding_id.clone(),
        incarnation: context.incarnation.clone(),
        identities,
        observed_at_ms: now_ms()?,
        // The receipt carries provenance, never a credential.
        receipt: format!(
            "sglang {} ready on {}; probe answered",
            adapter.fingerprint()?,
            adapter.endpoint()?
        ),
        facts: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
    })
}

fn roles(identities: &[ProcessIdentity]) -> String {
    identities
        .iter()
        .map(|i| format!("{}:{}", i.role, i.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The last `lines` lines of the engine's log, bounded and redacted. The tail is
/// the engine's own account of why it left, and it is quoted into an error that
/// reaches a journal, so it is passed through redaction first (Spec §3).
fn log_tail(path: Option<&str>, lines: usize) -> String {
    let Some(path) = path else {
        return "(no engine log was configured for this launch)".into();
    };
    let text = match read_tail_bytes(path) {
        Ok(text) => text,
        Err(e) => return format!("(engine log {path} could not be read: {e})"),
    };
    let tail: Vec<&str> = text
        .lines()
        .rev()
        .take(lines)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        return format!("(engine log {path} is empty)");
    }
    redact_text(&tail.join("\n"))
}

fn read_tail_bytes(path: &str) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > LOG_TAIL_BYTES {
        file.seek(SeekFrom::Start(len - LOG_TAIL_BYTES))?;
    }
    let mut buffer = Vec::new();
    file.take(LOG_TAIL_BYTES).read_to_end(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Wall-clock milliseconds. A clock that cannot answer leaves the step uncertain
/// rather than stamping evidence with a time nobody read.
fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}
