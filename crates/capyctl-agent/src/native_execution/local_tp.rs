//! ADR 0035 §§5, 6: a launch on several GPUs of this host, at readiness and
//! at every re-proof.
//!
//! The host's own observation names the rank processes (§6, "Remote
//! hosts"): once the engine is ready, and before the readiness is journaled,
//! fresh per-process, per-GPU samples of this host name, for each GPU of the
//! plan's device list, the owned process holding the most memory there.
//! Readiness waits, bounded by the launch's deadline, until one sample names
//! a distinct rank process per GPU, and fails `tp_rank_unobserved`
//! otherwise. Then every owned process's TCP listeners are read (§5): one off
//! loopback is stopped, with gone evidence, and the launch fails
//! `engine_listener_exposed` naming its role, address and port. The claim
//! stays until the controller's Terminate settles it on gone evidence.
use capyctl_adapters::custom::ListenerCheck;
use capyctl_domain::completion::ProcessIdentity;
use capyctl_domain::local_tp::{name_ranks, RankNaming, RankRecord, TP_RANK_UNOBSERVED};
use capyctl_domain::resources::ProcessResident;
use std::future::Future;
use std::time::Duration;

/// How often readiness samples this host while it waits for a rank process
/// per GPU.
pub(super) const RANK_POLL: Duration = Duration::from_millis(500);

/// ADR 0035 §6: sample (`sample`, `None` when no sample could be read) until
/// one names a distinct rank process per GPU of `devices` among
/// `identities`, or `until_ms` (Unix ms) is near. The renamed group and the
/// ranks, or the `tp_rank_unobserved` failure with the last reason.
pub(super) async fn await_ranks<F, Fut>(
    identities: &[ProcessIdentity],
    devices: &[String],
    until_ms: i64,
    mut sample: F,
) -> Result<(Vec<ProcessIdentity>, Vec<RankRecord>), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<Vec<ProcessResident>>>,
{
    let poll = i64::try_from(RANK_POLL.as_millis()).unwrap_or(i64::MAX);
    let mut last = "no per-GPU sample of this host was read".to_owned();
    loop {
        if let Some(sample) = sample().await {
            match name_ranks(identities, &sample, devices) {
                RankNaming::Named { identities, ranks } => return Ok((identities, ranks)),
                RankNaming::Unobserved(why) => last = why,
            }
        }
        if capyctl_protocol::now_unix_ms().saturating_add(poll) >= until_ms {
            return Err(format!(
                "{TP_RANK_UNOBSERVED}: no distinct rank process per GPU by the initialize \
                 timeout ({last})"
            ));
        }
        tokio::time::sleep(RANK_POLL).await;
    }
}

/// ADR 0035 §5: the loopback check of a built-in engine's launch on several
/// GPUs, over its live owned processes under `/proc` (a helper that exited
/// during the read is left out).
pub(super) fn check_listeners(identities: &[ProcessIdentity]) -> Result<(), ListenerCheck> {
    capyctl_adapters::custom::check_engine_launch(
        std::path::Path::new("/proc"),
        identities,
        &capyctl_launchers::process_absence::presence,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_domain::resources::DeviceBytes;

    fn identity(role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid),
        }
    }

    fn resident(pid: u32, device: &str) -> ProcessResident {
        ProcessResident {
            pid,
            boot_id: "boot".into(),
            start_ticks: u64::from(pid),
            bytes: 20 << 30,
            device_bytes: 20 << 30,
            host_bytes: 0,
            per_device: vec![DeviceBytes {
                device: device.into(),
                bytes: 20 << 30,
            }],
        }
    }

    // T46 (ADR 0035 §6): the host names its launch's ranks from its own
    // per-GPU samples, waiting for one that names a process per GPU;
    // SGLang's schedulers are workers and keep their roles.
    #[tokio::test]
    async fn the_host_names_a_rank_per_gpu_from_its_samples() {
        let group = vec![
            identity("api", 10),
            identity("worker-0", 11),
            identity("worker-1", 12),
        ];
        let devices = vec!["gpu0".to_owned(), "gpu1".to_owned()];
        let mut samples =
            vec![None, Some(vec![resident(11, "gpu0"), resident(12, "gpu1")])].into_iter();
        let until = capyctl_protocol::now_unix_ms() + 60_000;
        let (identities, ranks) = await_ranks(&group, &devices, until, || {
            let next = samples.next().flatten();
            async move { next }
        })
        .await
        .unwrap();
        assert_eq!(identities, group, "workers keep their roles");
        assert_eq!(
            capyctl_domain::local_tp::ranks_receipt(&ranks),
            "tp ranks: 0=worker-0:11@gpu0 1=worker-1:12@gpu1"
        );
    }

    // T46 (ADR 0035 §6, §13): no process on one GPU by the deadline fails
    // the launch `tp_rank_unobserved`.
    #[tokio::test]
    async fn a_gpu_without_a_rank_by_the_deadline_fails() {
        let group = vec![identity("api", 10), identity("worker-0", 11)];
        let devices = vec!["gpu0".to_owned(), "gpu1".to_owned()];
        let until = capyctl_protocol::now_unix_ms() + 600;
        let failed = await_ranks(&group, &devices, until, || async {
            Some(vec![resident(11, "gpu0")])
        })
        .await
        .unwrap_err();
        assert!(
            failed.starts_with("tp_rank_unobserved: ") && failed.contains("gpu1"),
            "{failed}"
        );
    }
}
