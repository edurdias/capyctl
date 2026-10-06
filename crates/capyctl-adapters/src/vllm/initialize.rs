//! The vLLM Initialize step (Spec §4). The builder does the whole step: render,
//! spawn through the director's tool, wait for readiness while watching the
//! process, probe once, enumerate the group, and report identities and facts.
//!
//! The builder never learns where identities are recorded: the tool the director
//! hands it makes the spawn durable, so a step that dies mid-launch still leaves
//! an owner behind an identity somebody else wrote down.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use capyctl_domain::completion::{
    EffectObservation, ExecutionIdentities, Milestone, Presence, ProcessIdentity,
};

use crate::engine_env::SYSTEM_PATH;
use crate::traits::{EngineAdapter, MemberRef, Readiness, RuntimeCommand, RuntimeError};
use crate::vllm::adapter::VllmAdapter;
use crate::vllm::args::{redact_text, render_command};

/// How often the builder asks the engine whether it is serving the model. vLLM's
/// startup is minutes of weight staging on this hardware, so a tighter poll only
/// buys load on the engine's own event loop.
const READINESS_POLL: Duration = Duration::from_millis(500);

/// How far ahead of the coordinator's bound the builder gives up. The builder's
/// reason — deadline, dead process, unanswered probe — is worth more than a bare
/// timeout, so it must be the one that arrives first (Spec §4).
const BUILDER_MARGIN_MS: i64 = 2_000;

/// SPEC §13.3 / T21: variables an engine may take from the agent's own
/// environment. Everything else it sees is named here by capyctl.
const PASS_THROUGH: &[&str] = &[
    "HOME",
    "CUDA_VISIBLE_DEVICES",
    "HF_HUB_OFFLINE",
    "TRANSFORMERS_OFFLINE",
];

/// Every variable a vLLM engine may be started with (SPEC §13.3 / T21). The
/// launcher clears the agent's environment, so this is all the engine sees.
pub const ENGINE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "CUDA_VISIBLE_DEVICES",
    // Discrete GPU design §7: set with an index-pinned GPU, never inherited.
    "CUDA_DEVICE_ORDER",
    "HF_HUB_OFFLINE",
    "TRANSFORMERS_OFFLINE",
    "VLLM_API_KEY",
    "CAPYCTL_VLLM_ADMIN_KEY",
    "VLLM_SERVER_DEV_MODE",
    "PYTHONPATH",
    "VLLM_PLUGINS",
    "PYTHONDONTWRITEBYTECODE",
    "CAPYCTL_ENGINE_LOG",
    "CAPYCTL_EXTRA_APPROVALS",
    // SPEC §13.3 amendment (owner decisions 2026-09-25): the toolchain and
    // the JIT build limits (engine_env.rs).
    "CUDA_HOME",
    "MAX_JOBS",
    "FLASHINFER_NVCC_THREADS",
];

/// ADR 0028 §10 (R11): the variables a group member's launch renders beyond
/// the single-rank allowlist. Admitted only for a group plan and only as
/// rendered: never inherited, never from the resolved engine env.
const GROUP_ENV: &[&str] = &[
    "VLLM_HOST_IP",
    crate::group::GLOO_SOCKET_IFNAME,
    crate::group::GROUP_MODE_ENV,
    crate::group::GROUP_EXPECTED_ENV,
];

/// The closed environment one vLLM launch starts with: the rendered variables,
/// the keys, the engine's tool path, a few named pass-throughs, and the pins
/// (no plugins, no bytecode). Anything outside the allowlist is dropped.
fn engine_environment(
    rendered: &std::collections::BTreeMap<String, String>,
    plan: &crate::vllm::args::PlanInputVllm,
    key: &str,
    admin: Option<&str>,
    inherited: &dyn Fn(&str) -> Option<String>,
    toolchain: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    for name in PASS_THROUGH {
        if let Some(value) = inherited(name) {
            env.insert((*name).to_string(), value);
        }
    }
    // ADR 0028 §2.1: the resolved engine env and the toolchain come before
    // everything CapyCTL renders, so a rendered or fixed value is never replaced.
    env.extend(toolchain.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.extend(rendered.iter().map(|(k, v)| (k.clone(), v.clone())));
    // Spec §3: the keys ride the environment, never argv. ADR 0028 §10 /
    // ADR 0012: the key-guarded surfaces are the head's alone; a headless
    // worker has no listener and gets no credential.
    if plan.group.as_ref().is_none_or(|group| group.is_head()) {
        env.insert("VLLM_API_KEY".into(), key.to_string());
        if let Some(admin) = admin {
            env.insert("CAPYCTL_VLLM_ADMIN_KEY".into(), admin.to_string());
        }
    }
    // The engine's runtime PATH carries its own venv bin (the JIT compile step
    // needs the venv's tools), the profile's CUDA bin when it names one, then
    // fixed system directories only (SPEC §13.3 as amended 2026-09-25).
    let path = crate::engine_env::tool_path(
        plan.engine_path_extra.as_deref(),
        plan.cuda_home.as_deref(),
        SYSTEM_PATH,
    );
    env.insert("PATH".into(), path);
    // ADR 0012 / T21: no vLLM plugin loads; SPEC §9.1: no bytecode is
    // written beside capyctl's checked runtime source.
    env.insert("VLLM_PLUGINS".into(), String::new());
    env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    if let Some(log) = &plan.engine_log {
        env.insert("CAPYCTL_ENGINE_LOG".into(), log.clone());
    }
    // ADR 0028 §2.1: the resolved engine env's names join the fixed list.
    // ADR 0028 §10: a group member also keeps exactly the group names
    // `render_command` rendered (none exist for a single rank, T39).
    env.retain(|name, _| {
        ENGINE_ENV_ALLOWLIST.contains(&name.as_str())
            || plan.build_env.contains_key(name)
            || (plan.group.is_some()
                && GROUP_ENV.contains(&name.as_str())
                && rendered.contains_key(name))
    });
    env
}

pub(super) async fn initialize(
    adapter: &VllmAdapter,
    command: &RuntimeCommand,
) -> Result<EffectObservation, RuntimeError> {
    let context = &command.context;
    // Unsupported when any part is missing: a builder without its plan, its tools
    // or its key cannot launch anything, and must not half-run the step.
    let (mut plan, tools, key) = adapter.launch_parts()?;
    if !matches!(
        context.launch_settings,
        Some(capyctl_domain::launch::LaunchSettings::Vllm(_))
    ) || !matches!(context.identities, ExecutionIdentities::OwnedLaunch)
    {
        return Err(RuntimeError::Unsupported);
    }
    // One launch per incarnation: a repeat would start a second engine holding the
    // same device memory while the first is still recorded as owned.
    adapter.claim_incarnation(&context.binding_id, &context.incarnation)?;

    // Spec §3: the key rides the environment, never argv.
    plan.api_key = None;
    let mut cmd =
        render_command(&plan).map_err(|e| RuntimeError::Uncertain(format!("render: {e}")))?;
    // Owner decision 2026-09-25: JIT build jobs follow free memory at launch.
    let (toolchain, limits) = crate::engine_env::launch_environment_noted(
        &plan.build_env,
        Some(&plan.engine_bin),
        plan.cuda_home.as_deref(),
        crate::engine_env::mem_available_bytes(),
        crate::engine_env::cpu_count(),
    );
    capyctl_domain::role_log::notice(
        capyctl_domain::role_log::Level::Notice,
        &format!("{limits} (binding {})", context.binding_id),
    );
    cmd.env = engine_environment(
        &cmd.env,
        &plan,
        &key,
        adapter.admin_key(),
        &|name| std::env::var(name).ok(),
        &toolchain,
    );

    // The tool is synchronous on purpose (capyctl-launchers has no runtime), so every
    // call into it leaves the async threads free.
    let incarnation = context.incarnation.clone();
    let spawn_tools = tools.clone();
    let api = tokio::task::spawn_blocking(move || spawn_tools.spawn_durable(&incarnation, &cmd))
        .await
        .map_err(|_| RuntimeError::Uncertain("spawn task failed".into()))??;
    // ADR 0014 amendment A12: note any kernel build until the step ends.
    let builds = crate::kernel_builds::BuildWatch::start(tools.clone(), api.clone());

    // ADR 0028 §9, §10: a headless worker serves nothing; readiness is the
    // head's. Its step ends once its process is recorded and present.
    if plan.group.as_ref().is_some_and(|group| !group.is_head()) {
        let log = plan.engine_log.clone();
        return crate::group::worker_spawned(
            context,
            tools,
            api,
            builds,
            || {
                let tail = crate::launch_failure::log_tail(log.as_deref());
                RuntimeError::LaunchFailed(format!(
                    "{}; log tail:\n{tail}",
                    crate::launch_failure::summary(&tail, None)
                ))
            },
            format!(
                "vllm {} group worker spawned; readiness is the head's",
                adapter.fingerprint()
            ),
        )
        .await;
    }

    // Spec §4: the builder ends before the coordinator's bound so its own error wins.
    let stop_at = context.deadline_ms.saturating_sub(BUILDER_MARGIN_MS);
    let member = MemberRef {
        deployment_id: context.token.deployment_id.clone(),
        member_id: context.binding_id.clone(),
    };
    loop {
        match adapter.check_readiness(&member).await {
            Ok(Readiness::Ready) => break,
            Ok(Readiness::Initializing) => {}
            // Spec §3: both exits from this step are journaled, so both pass
            // redaction. Display, not Debug, so the reason carries one prefix
            // rather than nesting this one inside the error's own.
            Err(e) => {
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
            Presence::Gone => {
                let tail = crate::launch_failure::log_tail(plan.engine_log.as_deref());
                return Err(RuntimeError::LaunchFailed(format!(
                    "{}; log tail:\n{tail}",
                    crate::launch_failure::summary(&tail, None)
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
        "model": plan.served_model_name,
        "messages": [{"role": "user", "content": "Say ready."}],
        "max_tokens": 8,
        "temperature": 0,
    });
    // Spec §4: every wait is bounded by the context deadline, and the builder's own
    // waits end first so its reason, not a bare coordinator timeout, is what gets
    // recorded. The probe is bounded by the remaining budget alone, never by the
    // transport's shorter read bound (a slow first completion is still startup).
    let probe_budget = Duration::from_millis(u64::try_from(stop_at - now_ms()?).unwrap_or(0));
    let answer = crate::forward::startup_probe(adapter, &body, probe_budget)
        .await
        .ok_or_else(|| {
            RuntimeError::Uncertain("probe deadline reached with the engine alive".into())
        })?
        .map_err(|e| {
            RuntimeError::Uncertain(redact_text(&format!(
                "engine listed the model but did not answer: {e:?}"
            )))
        })?;
    if !crate::forward::probe_answered(&answer) {
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
            "vllm {} ready on {}; probe answered",
            adapter.fingerprint(),
            adapter.endpoint()
        ),
        facts: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
        kernel_builds: builds.finish(),
    })
}

fn roles(identities: &[ProcessIdentity]) -> String {
    identities
        .iter()
        .map(|i| format!("{}:{}", i.role, i.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Wall-clock milliseconds. A clock that cannot answer leaves the step uncertain
/// rather than stamping evidence with a time nobody read.
fn now_ms() -> Result<i64, RuntimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RuntimeError::Uncertain("system clock is before the epoch".into()))
        .map(|d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::engine_environment;
    use crate::vllm::args::{render_command, PlanInputVllm};
    use capyctl_domain::group::GroupMemberArgs;
    use std::collections::BTreeMap;

    fn input(group: Option<(u32, Option<&str>)>) -> PlanInputVllm {
        PlanInputVllm {
            engine_bin: "/opt/venv/bin/vllm".into(),
            model_path: "/models/m".into(),
            port: 8100,
            served_model_name: "m".into(),
            tensor_parallel_size: if group.is_some() { 2 } else { 1 },
            pipeline_parallel_size: 1,
            runtime_dir: Some("/opt/capyctl/runtime".into()),
            sleep_flags: vec!["--enable-sleep-mode".into()],
            group: group.map(|(rank, interface)| GroupMemberArgs {
                tensor_parallel: 2,
                pipeline_parallel: 1,
                nnodes: 2,
                node_rank: rank,
                head_address: "192.0.2.10".parse().unwrap(),
                rendezvous_port: 25000,
                own_address: format!("192.0.2.{}", 10 + rank).parse().unwrap(),
                worker_port: None,
                own_interface: interface.map(str::to_owned),
            }),
            ..PlanInputVllm::default()
        }
    }

    /// The final spawned environment, as `initialize` builds it, with an agent
    /// environment that tries to leak every transport family.
    fn spawned(plan: &PlanInputVllm) -> BTreeMap<String, String> {
        let rendered = render_command(plan).unwrap().env;
        let inherited = |name: &str| {
            (name.starts_with("NCCL_")
                || name.starts_with("GLOO_")
                || name.starts_with("MASTER_")
                || name == "VLLM_HOST_IP"
                || name == "HOME")
                .then(|| "inherited".to_string())
        };
        engine_environment(
            &rendered,
            plan,
            "engine-key",
            Some("admin-key"),
            &inherited,
            &BTreeMap::new(),
        )
    }

    fn assert_no_transport(env: &BTreeMap<String, String>, ifname: Option<&str>) {
        assert!(!env
            .keys()
            .any(|k| k.starts_with("NCCL_") || k.starts_with("MASTER_")));
        let gloo: Vec<_> = env.keys().filter(|k| k.starts_with("GLOO_")).collect();
        match ifname {
            Some(i) => {
                assert_eq!(gloo, ["GLOO_SOCKET_IFNAME"]);
                assert_eq!(env["GLOO_SOCKET_IFNAME"], i);
            }
            None => assert!(gloo.is_empty()),
        }
    }

    // T14, T21, R11 (ADR 0028 §10): every member's spawned environment carries
    // the four rendered group names and no other transport variable.
    #[test]
    fn group_members_spawn_with_the_rendered_group_environment() {
        for rank in [0, 1] {
            for ifname in [Some("eth9"), None] {
                let plan = input(Some((rank, ifname)));
                let rendered = render_command(&plan).unwrap().env;
                let env = spawned(&plan);
                assert_eq!(env["VLLM_HOST_IP"], format!("192.0.2.{}", 10 + rank));
                assert_eq!(env["CAPYCTL_GROUP_MODE"], "1");
                assert_eq!(
                    env["CAPYCTL_GROUP_EXPECTED"],
                    rendered["CAPYCTL_GROUP_EXPECTED"]
                );
                assert_no_transport(&env, ifname);
                // Sleep-enabled workers keep the development switch.
                assert_eq!(env["VLLM_SERVER_DEV_MODE"], "1");
            }
        }
    }

    // T21, T37 (ADR 0012): the key-guarded surfaces are the head's; a headless
    // worker gets no credential.
    #[test]
    fn credentials_reach_the_head_and_single_rank_only() {
        for plan in [input(None), input(Some((0, Some("eth9"))))] {
            let env = spawned(&plan);
            assert_eq!(env["VLLM_API_KEY"], "engine-key");
            assert_eq!(env["CAPYCTL_VLLM_ADMIN_KEY"], "admin-key");
        }
        let env = spawned(&input(Some((1, Some("eth9")))));
        assert!(!env.contains_key("VLLM_API_KEY"));
        assert!(!env.contains_key("CAPYCTL_VLLM_ADMIN_KEY"));
    }

    // T39: a single-rank launch spawns with none of the group names.
    #[test]
    fn single_rank_spawn_has_no_group_names() {
        let env = spawned(&input(None));
        for name in super::GROUP_ENV {
            assert!(!env.contains_key(*name), "{name}");
        }
        assert!(!env
            .keys()
            .any(|k| k.starts_with("NCCL_") || k.starts_with("GLOO_") || k.starts_with("MASTER_")));
    }
}
