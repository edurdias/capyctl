//! ADR 0035 §§5, 6: the readiness of an embedded launch on several GPUs of
//! one host.
//!
//! Once the engine is ready, and before its processes are recorded, the
//! launch's rank processes are named from fresh per-process, per-GPU samples
//! of its host (§6): for each GPU of its device list, the owned process
//! holding the most memory there. Readiness waits, bounded by the launch's
//! Initialize deadline, until one sample names a distinct rank process per
//! GPU; otherwise the launch fails `tp_rank_unobserved`. Then every owned
//! process's TCP listeners are read (§5, ADR 0031 §7's check): one off
//! loopback fails the launch `engine_listener_exposed`, naming its role,
//! address and port. Either failure returns an error, which the failed-launch
//! settlement (`settle_failed_launch`) stops with gone evidence required and
//! settles: nothing is released while any recorded process may live.
//!
//! A remote launch is named and checked by its host (the agent's readiness);
//! the controller reads the ranks from its result.
use super::*;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::local_tp::{name_ranks, ranks_receipt, RankNaming, RankRecord};

/// How often readiness samples the host while it waits for a rank process
/// per GPU.
pub(super) const RANK_POLL: Duration = Duration::from_millis(500);

/// ADR 0035 §6: sample (`sample`, `None` when no sample could be read) until
/// one names a distinct rank process per GPU of `devices` among
/// `identities`, or `until` passes. The renamed group and the ranks, or the
/// `tp_rank_unobserved` failure with the last reason no rank was named.
pub(super) async fn await_ranks<F, Fut>(
    identities: &[ProcessIdentity],
    devices: &[String],
    until: tokio::time::Instant,
    mut sample: F,
) -> Result<(Vec<ProcessIdentity>, Vec<RankRecord>), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Vec<capyctl_domain::resources::ProcessResident>>>,
{
    let mut last = "no per-GPU sample of the host was read".to_owned();
    loop {
        if let Some(sample) = sample().await {
            match name_ranks(identities, &sample, devices) {
                RankNaming::Named { identities, ranks } => return Ok((identities, ranks)),
                RankNaming::Unobserved(why) => last = why,
            }
        }
        if tokio::time::Instant::now() + RANK_POLL >= until {
            return Err(format!(
                "{}: no distinct rank process per GPU by the initialize timeout ({last})",
                capyctl_domain::local_tp::TP_RANK_UNOBSERVED
            ));
        }
        tokio::time::sleep(RANK_POLL).await;
    }
}

/// ADR 0035 §5: what the listener check's answer means for the launch: the
/// `engine_listener_exposed` failure naming the process, or a reading that
/// proves nothing (which fails the launch too: its listeners are unknown).
/// ADR 0039 §14: a launch of TensorFold's native line fails
/// `tensorfold_listener_exposed` naming the address instead.
pub(super) fn listener_verdict(
    check: Result<(), capyctl_adapters::custom::ListenerCheck>,
    tensorfold_native: bool,
) -> Result<(), String> {
    use capyctl_adapters::custom::ListenerCheck;
    match check {
        Ok(()) => Ok(()),
        Err(ListenerCheck::Exposed(listener)) => Err(format!(
            "{}; the launch is stopped",
            if tensorfold_native {
                crate::local_readiness::tensorfold_listener_exposed(&listener)
            } else {
                capyctl_adapters::custom::engine_listener_exposed(&listener)
            }
        )),
        Err(unread) => Err(unread.to_string()),
    }
}

/// ADR 0035 §§5, 6: an embedded launch on several GPUs, after the engine's
/// readiness: its ranks named and recorded in the receipt, its listeners
/// checked. ADR 0039 §14: an embedded launch of TensorFold's native line has
/// its listeners checked on any number of GPUs. Any other launch on one GPU,
/// or one without process tools (a remote launch, whose host does this), is
/// returned as it is.
pub(super) async fn at_readiness(
    shared: &Arc<Shared>,
    work: &InitializeWork,
    source: &dyn ServiceObservation,
    driver: &Driver,
    mut observation: capyctl_domain::completion::EffectObservation,
) -> Result<capyctl_domain::completion::EffectObservation, CoordinatorError> {
    let Some(tools) = driver.tools.clone() else {
        return Ok(observation);
    };
    let effective = work.effective();
    let devices = crate::remote_execution::local_devices(&effective.host.name, effective);
    let tensorfold_native = effective.profile.engine
        == capyctl_config::engine_policy::Engine::Tensorfold
        && effective.profile.line.is_native();
    if devices.len() < 2 && !tensorfold_native {
        return Ok(observation);
    }
    // ADR 0035 §6 and its evidence table: vLLM and SGLang run one process
    // per rank, named here. llama.cpp runs one process across every GPU of
    // its layer split, so it has no rank processes to name (naming would
    // always fail it `tp_rank_unobserved`); its one process is already the
    // engine's `api`, judged by the exit watchers as today.
    if devices.len() >= 2 && capyctl_domain::local_tp::names_ranks(effective.profile.engine.name())
    {
        let now = (shared.clock)()?;
        let left = Duration::from_millis(u64::try_from(work.deadline_ms() - now).unwrap_or(0))
            .min(shared.options.initialize_timeout);
        let until = tokio::time::Instant::now() + left;
        let host = effective.host.name.clone();
        let bound = shared.options.protocol_timeout;
        let (identities, ranks) = await_ranks(&observation.identities, &devices, until, || {
            let host = host.clone();
            async move {
                match tokio::time::timeout(bound, source.observe_with_residents(host)).await {
                    Ok(Ok((_, residents))) => Some(residents),
                    _ => None,
                }
            }
        })
        .await
        .map_err(CoordinatorError::Service)?;
        observation.identities = identities;
        observation.receipt = format!("{}; {}", observation.receipt, ranks_receipt(&ranks));
    }
    let checked = observation.identities.clone();
    let check = tokio::task::spawn_blocking(move || {
        capyctl_adapters::custom::check_engine_launch(
            std::path::Path::new("/proc"),
            &checked,
            &|identity| tools.present(identity),
        )
    })
    .await
    .map_err(|_| CoordinatorError::Service("the listener check did not run".into()))?;
    listener_verdict(check, tensorfold_native).map_err(CoordinatorError::Service)?;
    observation.receipt = format!(
        "{}; every owned listener is on loopback",
        observation.receipt
    );
    Ok(observation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_domain::resources::{DeviceBytes, ProcessResident};

    fn identity(role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid),
        }
    }

    fn resident(pid: u32, device: &str, bytes: u64) -> ProcessResident {
        ProcessResident {
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid),
            bytes: bytes as i64,
            device_bytes: bytes as i64,
            host_bytes: 0,
            per_device: vec![DeviceBytes {
                device: device.into(),
                bytes,
            }],
        }
    }

    // T46 (ADR 0035 §6): readiness waits for a sample that names a rank
    // process per GPU (here the second: the first saw only one GPU's), and
    // renames vLLM's workers `rank-N`.
    #[tokio::test]
    async fn readiness_waits_for_a_rank_process_per_gpu() {
        let group = vec![
            identity("api", 10),
            identity("worker-0", 11),
            identity("helper-0", 12),
            identity("helper-1", 13),
        ];
        let devices = vec!["gpu0".to_owned(), "gpu1".to_owned()];
        let mut samples = vec![
            vec![resident(12, "gpu0", 20 << 30)],
            vec![
                resident(12, "gpu0", 20 << 30),
                resident(13, "gpu1", 20 << 30),
            ],
        ]
        .into_iter();
        let until = tokio::time::Instant::now() + Duration::from_secs(60);
        let (identities, ranks) = await_ranks(&group, &devices, until, || {
            let next = samples.next();
            async move { next }
        })
        .await
        .unwrap();
        assert_eq!(
            identities
                .iter()
                .map(|p| p.role.as_str())
                .collect::<Vec<_>>(),
            ["api", "worker-0", "rank-0", "rank-1"]
        );
        assert_eq!(
            ranks_receipt(&ranks),
            "tp ranks: 0=rank-0:12@gpu0 1=rank-1:13@gpu1"
        );
    }

    // T46 (ADR 0035 §6, §13): a GPU no owned process holds by the initialize
    // timeout, or one process holding both GPUs most, fails the launch
    // `tp_rank_unobserved`.
    #[tokio::test]
    async fn no_rank_per_gpu_by_the_deadline_fails_tp_rank_unobserved() {
        let group = vec![identity("api", 10), identity("worker-0", 11)];
        let devices = vec!["gpu0".to_owned(), "gpu1".to_owned()];
        let until = tokio::time::Instant::now() + Duration::from_secs(1);
        let both = || async {
            Some(vec![ProcessResident {
                per_device: vec![
                    DeviceBytes {
                        device: "gpu0".into(),
                        bytes: 1 << 30,
                    },
                    DeviceBytes {
                        device: "gpu1".into(),
                        bytes: 1 << 30,
                    },
                ],
                ..resident(11, "gpu0", 2 << 30)
            }])
        };
        let failed = await_ranks(&group, &devices, until, both)
            .await
            .unwrap_err();
        assert!(
            failed.starts_with("tp_rank_unobserved: no distinct rank process per GPU"),
            "{failed}"
        );
        assert!(
            failed.contains("would be the rank of gpu0 and gpu1"),
            "{failed}"
        );
        let unread = await_ranks(&group, &devices, until, || async { None })
            .await
            .unwrap_err();
        assert!(unread.contains("no per-GPU sample"), "{unread}");
    }

    // T46 (ADR 0035 §5): an exposed listener fails the launch
    // `engine_listener_exposed` naming the process, address and port; a
    // reading that proves nothing fails it too, uncoded.
    #[test]
    fn the_listener_verdict_names_the_exposed_process() {
        use capyctl_adapters::custom::{ListenerCheck, OwnedListener};
        assert_eq!(listener_verdict(Ok(()), false), Ok(()));
        let exposed = listener_verdict(
            Err(ListenerCheck::Exposed(OwnedListener {
                address: "0.0.0.0:29500".parse().unwrap(),
                role: "rank-1".into(),
                pid: 13,
            })),
            false,
        )
        .unwrap_err();
        assert_eq!(
            exposed,
            "engine_listener_exposed: the rank-1 process 13 listens on address 0.0.0.0 port \
             29500, not on loopback; the launch is stopped"
        );
        let unread =
            listener_verdict(Err(ListenerCheck::Unreadable("no fd".into())), false).unwrap_err();
        assert!(!unread.contains("engine_listener_exposed"));
    }

    // T41 T21 (ADR 0039 §14): an embedded launch of TensorFold's native line
    // listening off loopback at its first readiness fails
    // `tensorfold_listener_exposed` naming the address (the failed-launch
    // settlement stops it with gone evidence), and the failure is recorded
    // under that code.
    #[test]
    fn a_native_launch_listening_off_loopback_fails_tensorfold_listener_exposed() {
        use capyctl_adapters::custom::{ListenerCheck, OwnedListener};
        assert_eq!(listener_verdict(Ok(()), true), Ok(()));
        let exposed = listener_verdict(
            Err(ListenerCheck::Exposed(OwnedListener {
                address: "0.0.0.0:8080".parse().unwrap(),
                role: "api".into(),
                pid: 13,
            })),
            true,
        )
        .unwrap_err();
        assert_eq!(
            exposed,
            "tensorfold_listener_exposed: an owned process listens on 0.0.0.0:8080, \
             not on loopback; the launch is stopped"
        );
        assert_eq!(
            super::super::native_failure::readiness_failure(&exposed).as_deref(),
            Some("tensorfold_listener_exposed")
        );
    }
}
