//! SPEC §10, ADR 0013 §8 (plan W10): the durable half of request-driven
//! switching.
//!
//! A request for a deployment with no READY instance first tries placement
//! without eviction (ADR 0013 §8 rule 2). When nothing fits, the coordinator
//! asks here for a plan: one host (the one needing the least eviction) and the
//! minimum set of READY instances on it whose release lets the waiting
//! instance fit, preferring instances whose deployment keeps serving
//! elsewhere, then the least recently used (rules 3–4). Nothing on another host
//! is touched.
//!
//! Releasing a victim is the SPEC §10 sequence, each step its own transaction
//! here: close the victim's dispatch gate (step 3), let its request leases
//! drain (step 4; the coordinator bounds the wait and reopens the gate on a
//! timeout), then park it at its declared tier or, when it does not park, stop
//! it ordinarily (step 5). A park or stop accepted here is the ordinary W5 or
//! cleanup operation: it completes only on its own evidence (`memory_released`
//! or gone), and the waiting instance is placed and armed afterwards against
//! the ledger as that evidence left it (step 6). The victim stays eligible for
//! on-demand activation (rule 6: no background refill).
use super::cleanup::{instance_scope, StopCommand};
use super::park::{footprints, journal, launch, parks};
use super::*;
use crate::events::{append_event, EventMetadata, SwitchPhase};
use crate::instances::instance_owner_id;
use mllm_config::instances::Placement;
use mllm_scheduler::device_choice::choose_device_with_eviction;
use mllm_scheduler::placement::{candidate_fits, fits};
use mllm_scheduler::switching::{choose_victims, order_victims, Release, Victim, VictimCandidate};
use std::collections::BTreeSet;

/// The default fairness window when a host publishes no queue policy
/// (`resource_policy.queue.admission_window`, SPEC §16.2).
const DEFAULT_ADMISSION_WINDOW_MS: i64 = 2_000;

/// One READY instance a switch releases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SwitchVictim {
    pub deployment_id: String,
    pub instance: u32,
    pub generation: i64,
    /// SPEC §6.2, ADR 0013 §8 rule 8: it parks at its declared tier; otherwise
    /// it is stopped with absence proof.
    pub parks: bool,
    /// Discrete GPU design §5: it parks at its declared tier, but its parked
    /// footprint (a `host_backed` weights copy in host RAM) does not fit the
    /// host after the switch, so it is stopped instead.
    pub park_does_not_fit: bool,
    /// ADR 0013 §8 rule 5: releasing it leaves its deployment with no READY
    /// instance, so the fairness window of SPEC §10 step 3 applies.
    pub last_ready: bool,
    /// Its deployment keeps another READY instance that is not released.
    pub serves_elsewhere: bool,
}

/// What a waiting deployment needs before its activation can be placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchPlan {
    /// Nothing needs releasing: it serves already, an activation is in
    /// flight, or an instance fits somewhere as the ledger stands.
    FitsNow,
    /// Release `victims` on `host`, then activate `instance` there.
    Evict {
        host: String,
        instance: u32,
        /// The instance is parked on `host` and wakes in place.
        wake: bool,
        victims: Vec<SwitchVictim>,
        /// The host's bounded, non-resetting admission window (SPEC §10).
        admission_window_ms: i64,
    },
    /// No host can take it even by releasing every eligible READY instance.
    Impossible(String),
}

/// Owner decision 2026-09-25: one instance of an evicting start that needs
/// victims released before it can activate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartStep {
    pub host: String,
    pub instance: u32,
    /// The instance is parked on `host` and wakes in place.
    pub wake: bool,
    pub victims: Vec<SwitchVictim>,
    /// The host's bounded, non-resetting admission window (SPEC §10).
    pub admission_window_ms: i64,
}

/// Owner decision 2026-09-25: what an evicting start of a whole deployment
/// needs before every instance it targets can be placed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartSwitchPlan {
    /// Release each step's victims on its host; an empty list means every
    /// targeted instance fits (or serves) as the ledger stands.
    Steps(Vec<StartStep>),
    /// Instance `instance` fits on no allowed host even after releasing every
    /// eligible READY instance and the earlier instances' victims. `code` is
    /// the closed placement diagnostic; `detail` names each host's shortfall.
    Impossible {
        instance: u32,
        code: String,
        detail: String,
    },
}

/// What the plan for earlier instances of the same evicting start already
/// assumed (owner decision 2026-09-25).
#[derive(Default)]
struct Assumed {
    /// (host, owner, steady footprint) of each instance planned so far.
    charges: Vec<(String, String, PhaseFootprint)>,
    /// Owners of the victims planned so far: out of the ledger.
    released: BTreeSet<String>,
}

/// One instance's plan (see [`plan_in`]).
enum Planned {
    /// It serves, its activation is in flight, or nothing is startable.
    Settled,
    /// It fits as the ledger stands, on the host (and, ADR 0019, the GPU)
    /// placement would choose.
    Fits {
        host: Option<String>,
        device: Option<String>,
    },
    /// Release victims first; on a multi-GPU host, the GPU they free.
    Evict(SwitchPlan, Option<String>),
    /// It fits nowhere even with eviction.
    Impossible { code: String, detail: String },
}

/// A victim's accepted release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchRelease {
    pub operation_id: String,
    pub parked: bool,
}

/// One switch transition, for the management event stream.
#[derive(Clone, Debug)]
pub struct SwitchRecord<'a> {
    pub phase: SwitchPhase,
    pub switch_id: &'a str,
    pub target: &'a str,
    pub host: Option<&'a str>,
    pub victims: &'a [SwitchVictim],
    pub detail: &'a str,
    /// Owner decision 2026-09-23: started by an explicit `start --evict`, not
    /// by a waiting request.
    pub explicit: bool,
}

fn open_runs_clause(alias: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id={alias}.deployment_id AND r.instance_index={alias}.instance_index AND r.state IN ('queued','running','uncertain'))"
    )
}

fn placement_spec(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
) -> Result<Placement, LifecycleError> {
    tx.query_row(
        "SELECT placement_json FROM deployment_revision_instances WHERE deployment_id=?1 AND revision=?2",
        params![deployment, revision],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|json| serde_json::from_str::<Placement>(&json))
    .transpose()
    .map_err(|_| LifecycleError::CorruptStoredData)
    .map(Option::unwrap_or_default)
}

/// Whether the park of this incarnation was refused before (it is stopped
/// instead, as the idle policy does).
/// SPEC §9.2, §10: whether a request lease on the instance is `uncertain`
/// (the router could not prove it ended). Such an instance may still be
/// released by a Stop, whose cleanup evidence settles the lease, but never
/// parked: unknown work is not proof of quiescence.
fn uncertain_leases(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2
                 AND disposition='uncertain')",
        params![deployment, instance],
        |r| r.get(0),
    )?)
}

fn park_refused(
    tx: &Transaction<'_>,
    deployment: &str,
    instance: u32,
    generation: i64,
) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations o JOIN lifecycle_runs r ON r.operation_id=o.id
          WHERE o.deployment_id=?1 AND r.instance_index=?2 AND r.generation=?3 AND o.kind='park'
            AND o.state='failed' AND o.error_code IN ('park_refused','park_parked_capacity'))",
        params![deployment, instance, generation],
        |r| r.get(0),
    )?)
}

/// SPEC §6.1 admission column: what a request arriving for a deployment
/// finds, to decide whether it dispatches, queues or is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestView {
    /// A READY instance with open admission and dispatch serves it.
    Serving,
    /// An instance is mid-transition (STARTING, WAKING, DRAINING, PARKING,
    /// STOPPING): the request queues behind this operation.
    InFlight { operation_id: String },
    /// An instance is READY but its dispatch is closed with nothing in
    /// flight (a host re-proving readiness, an exited engine): refused,
    /// retryable.
    Closed,
    /// Nothing runs or moves: an activation may be requested.
    Idle,
}

impl crate::Store {
    /// SPEC §6.1, §10: what a request for `deployment` finds right now.
    pub fn switch_request_view(&self, deployment: &str) -> Result<RequestView, LifecycleError> {
        let serving: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployment_instances i JOIN deployments d ON d.id=i.deployment_id
                     WHERE i.deployment_id=?1 AND i.observed_state='ready' AND i.dispatch_enabled=1
                       AND i.admission_enabled=1 AND d.suspended=0)",
            [deployment],
            |r| r.get(0),
        )?;
        if serving {
            return Ok(RequestView::Serving);
        }
        let moving: Option<String> = self
            .conn
            .query_row(
                "SELECT r.operation_id FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id
                  WHERE r.deployment_id=?1 AND r.state IN ('queued','running','uncertain')
                  ORDER BY o.accepted_at,o.id LIMIT 1",
                [deployment],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(operation_id) = moving {
            return Ok(RequestView::InFlight { operation_id });
        }
        let ready: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND observed_state='ready')",
            [deployment],
            |r| r.get(0),
        )?;
        Ok(if ready {
            RequestView::Closed
        } else {
            RequestView::Idle
        })
    }

    /// ADR 0013 §8 rules 1–4: plan the release that lets one instance of
    /// `target` activate. Read only; nothing is closed, parked or reserved.
    ///
    /// `protected` names deployments serving a waiting group (targets of a
    /// switch in progress): never victims. `activity` is the router's last
    /// request time for an instance generation (LRU order).
    ///
    /// `only` names the one instance an explicit `start instance --evict`
    /// makes room for; `None` is the deployment's next instance (Q5).
    /// `explicit` is an operator's `--evict` start, which also plans for an
    /// instance the operator stopped (the start lifts that stop).
    #[allow(clippy::too_many_arguments)]
    pub fn plan_switch(
        &self,
        s: &CoordinatorSession,
        target: &str,
        only: Option<u32>,
        explicit: bool,
        eligible: placement::Eligible<'_>,
        protected: &BTreeSet<String>,
        activity: &dyn Fn(&str, i64) -> Option<i64>,
    ) -> Result<SwitchPlan, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let plan = match plan_in(
            &tx,
            target,
            only,
            explicit,
            eligible,
            protected,
            activity,
            &Assumed::default(),
        )? {
            Planned::Settled | Planned::Fits { .. } => SwitchPlan::FitsNow,
            Planned::Evict(plan, _) => plan,
            Planned::Impossible { code, .. } => SwitchPlan::Impossible(code),
        };
        tx.commit()?;
        Ok(plan)
    }

    /// Owner decision 2026-09-25 (`start deployment --evict`): plan the
    /// releases that let every instance the start targets activate, not only
    /// the first. Read only; nothing is closed, parked or reserved.
    ///
    /// Instances are planned in the order the start activates them (parked
    /// ones wake first, then cold ones start, lowest index first). Each is
    /// planned against the ledger as the earlier ones leave it: an earlier
    /// instance is charged its steady footprint on the host it goes to (as
    /// placement charges a start accepted but not yet armed), and an earlier
    /// instance's victims are already out of the ledger and never chosen
    /// twice. Each instance's own victim set is minimal, so the start never
    /// evicts beyond what placement needs. When any instance cannot be placed
    /// even with eviction the whole plan is `Impossible` and names it, so the
    /// caller refuses before releasing anyone.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_start_switch(
        &self,
        s: &CoordinatorSession,
        target: &str,
        eligible: placement::Eligible<'_>,
        protected: &BTreeSet<String>,
        activity: &dyn Fn(&str, i64) -> Option<i64>,
    ) -> Result<StartSwitchPlan, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let plan = plan_start_in(&tx, target, eligible, protected, activity)?;
        tx.commit()?;
        Ok(plan)
    }

    /// SPEC §10 step 3: close one READY instance's dispatch gate for a
    /// switch. New leases are refused from here on; granted ones drain.
    /// `false` when the instance is no longer that READY incarnation.
    pub fn close_for_switch(
        &self,
        s: &CoordinatorSession,
        deployment: &str,
        instance: u32,
        generation: i64,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let changed = tx.execute(
            "UPDATE deployment_instances SET dispatch_enabled=0
              WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3
                AND observed_state='ready' AND dispatch_enabled=1",
            params![deployment, instance, generation],
        )?;
        if changed == 1 {
            // W10 gap (a): the switch owns this closure and only this one.
            crate::switch_state::record_closure(
                &tx,
                deployment,
                instance,
                generation,
                crate::switch_state::ClosureReason::Switch,
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// SPEC §10 (drain timeout): the switch failed before any release was
    /// accepted, so the victim serves again. Reopens only a READY instance
    /// with no lifecycle run in flight whose deployment is not suspended, and
    /// only a gate this switch closed that nothing else has closed since: a
    /// host-loss, unresponsive-host or engine-exit closure recorded during the
    /// drain window keeps it closed (SPEC §13.2) until its own evidence clears.
    pub fn reopen_after_switch(
        &self,
        s: &CoordinatorSession,
        deployment: &str,
        instance: u32,
        generation: i64,
    ) -> Result<bool, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let ours = crate::switch_state::clear_closure(
            &tx,
            deployment,
            instance,
            generation,
            crate::switch_state::ClosureReason::Switch,
        )?;
        let changed = if ours {
            tx.execute(
                &format!(
                    "UPDATE deployment_instances AS i SET dispatch_enabled=1
                      WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.generation=?3
                        AND i.desired_state='ready' AND i.observed_state='ready' AND i.admission_enabled=1
                        AND i.dispatch_enabled=0
                        AND NOT EXISTS(SELECT 1 FROM deployments d WHERE d.id=i.deployment_id AND d.suspended=1)
                        AND NOT {} AND {}",
                    open_runs_clause("i"),
                    crate::switch_state::no_closure_clause("i")
                ),
                params![deployment, instance, generation],
            )?
        } else {
            0
        };
        tx.commit()?;
        Ok(changed == 1)
    }

    /// SPEC §10 step 4: request leases still in flight on one incarnation.
    /// Drained only at zero. An `uncertain` lease is not counted: the router
    /// could not prove that request ended, so waiting can never settle it
    /// (it would hold every switch to its drain timeout and leave the
    /// instance un-evictable). It stays charged; the victim is then released
    /// by a Stop, never a park (SPEC §9.2: unknown work is not proof of
    /// quiescence for parking), and only the Stop's cleanup evidence settles
    /// it (SPEC §10: conservative accounting until controlled cleanup).
    pub fn switch_outstanding_leases(
        &self,
        deployment: &str,
        instance: u32,
        generation: i64,
    ) -> Result<usize, LifecycleError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND disposition='inflight'",
            params![deployment, instance, generation],
            |r| r.get(0),
        )?;
        usize::try_from(n).map_err(|_| LifecycleError::CorruptStoredData)
    }

    /// SPEC §6.3 ("drain, and terminate engine workers"), §10: requests still
    /// in flight on the instance a runtime binding serves, of any generation (a
    /// Stop's fence moves the generation while accepted work is still
    /// completing). An `uncertain` lease is not counted: the router could not
    /// prove that request ended (its stream was cut, or its session was lost),
    /// so waiting cannot settle it; only the cleanup's evidence that the engine
    /// is gone does. `None` when the binding names no instance.
    pub fn binding_outstanding_leases(
        &self,
        binding_id: &str,
    ) -> Result<Option<usize>, LifecycleError> {
        let Some((deployment, instance)) = self.binding_lane(binding_id)? else {
            return Ok(None);
        };
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM request_leases WHERE deployment_id=?1 AND instance_index=?2 AND disposition='inflight'",
            params![deployment, instance],
            |r| r.get(0),
        )?;
        usize::try_from(n)
            .map(Some)
            .map_err(|_| LifecycleError::CorruptStoredData)
    }

    /// SPEC §10 step 5, ADR 0013 §8 rule 8: release a drained victim. It
    /// parks at its declared tier when it parks (and its park was not refused
    /// before), otherwise it is stopped ordinarily, leaving it eligible for
    /// on-demand activation. `may_park` is the plan's decision: false when its
    /// parked footprint does not fit the host after the switch (discrete GPU
    /// design §5), or when the waiting instance needs an empty host (a solo
    /// first start), where a parked residual would still occupy it, so the
    /// victim stops. Refused
    /// (`Conflict`) while a lease remains or the instance is not the closed
    /// READY incarnation the switch drained.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_switch_release(
        &self,
        s: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        instance: u32,
        generation: i64,
        key: &str,
        now: i64,
        may_park: bool,
    ) -> Result<SwitchRelease, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let (source, e, _) = launch(&tx, deployment, instance)?;
        if source.generation != generation {
            return Err(LifecycleError::Stale);
        }
        let drained: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2
                    AND generation=?3 AND observed_state='ready' AND dispatch_enabled=0)
                AND NOT EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2
                                  AND disposition='inflight')",
            params![deployment, instance, generation],
            |r| r.get(0),
        )?;
        if !drained {
            return Err(LifecycleError::Conflict);
        }
        // SPEC §9.2: an uncertain lease is unknown work, which is not proof of
        // quiescence for parking. The victim stops instead; the lease stays
        // charged until that cleanup's evidence settles it (SPEC §10).
        let unknown_work = uncertain_leases(&tx, deployment, instance)?;
        let may_park = may_park && !unknown_work;
        // The park or stop accepted below owns the gate from here on.
        crate::switch_state::clear_closure(
            &tx,
            deployment,
            instance,
            generation,
            crate::switch_state::ClosureReason::Switch,
        )?;
        let deadline = now.saturating_add(e.request_deadline_ms);
        let release = if may_park
            && parks(&e)
            && !park_refused(&tx, deployment, instance, generation)?
        {
            let receipt = Self::instance_park_in_transaction(
                &tx, s, principal, deployment, instance, key, now, deadline,
            )?;
            journal(
                &tx,
                &receipt.operation_id,
                "switch_park",
                &format!(
                    "deployment {deployment}: instance {instance} drained for a switch; it parks at its declared tier and stays eligible for on-demand activation"
                ),
            )?;
            SwitchRelease {
                operation_id: receipt.operation_id,
                parked: true,
            }
        } else {
            let revision: i64 = tx.query_row(
                "SELECT revision FROM deployments WHERE id=?1",
                [deployment],
                |r| r.get(0),
            )?;
            let receipt = Self::accept_instance_stop_in_transaction(
                &tx,
                s,
                principal,
                &source.fence(),
                key,
                now,
                deadline,
                &StopCommand {
                    scope: Some(instance_scope(deployment, instance)),
                    revision: Some(revision),
                },
            )?;
            journal(
                &tx,
                &receipt.operation_id,
                "switch_stop",
                &format!(
                    "deployment {deployment}: instance {instance} drained for a switch; {}, so it stops with absence proof and stays eligible for on-demand activation",
                    if unknown_work {
                        "a request on it is uncertain, which is not quiescence for a park"
                    } else if may_park {
                        "it does not park"
                    } else if parks(&e) {
                        // Discrete GPU design §5: the plan stops a victim
                        // whose parked footprint does not fit after the
                        // switch, as it does every victim of a solo start.
                        "the plan releases it by a stop (its parked copy does not fit the host after the switch, or the waiting instance needs an empty host)"
                    } else {
                        "it does not park"
                    }
                ),
            )?;
            SwitchRelease {
                operation_id: receipt.operation_id,
                parked: false,
            }
        };
        tx.commit()?;
        Ok(release)
    }

    /// Journal one switch transition on the management event stream.
    ///
    /// W10 gap (a), SPEC §10: a terminal record (completed or failed) also ends
    /// every closure this switch still holds on its victims, so none is left
    /// orphaned by a switch that ended without reopening one. A victim whose
    /// gate only this switch held serves again, under the same rule as
    /// [`Self::reopen_after_switch`].
    pub fn record_switch(
        &self,
        s: &CoordinatorSession,
        record: &SwitchRecord<'_>,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        if matches!(record.phase, SwitchPhase::Completed | SwitchPhase::Failed) {
            for v in record.victims {
                end_victim_closure(
                    &tx,
                    record.switch_id,
                    &v.deployment_id,
                    v.instance,
                    Some(v.generation),
                )?;
            }
        }
        // W10 gap (c): the switch in progress, for status.
        let phase = match record.phase {
            SwitchPhase::Planned => Some("planned"),
            SwitchPhase::AdmissionClosed => Some("admission_closed"),
            SwitchPhase::Released => Some("released"),
            SwitchPhase::Completed | SwitchPhase::Failed => None,
        };
        crate::switch_state::record_switch_phase(
            &tx,
            record.switch_id,
            record.target,
            record.host,
            &record
                .victims
                .iter()
                .map(|v| format!("{}/{}", v.deployment_id, v.instance))
                .collect::<Vec<_>>(),
            phase,
            record.explicit,
        )?;
        append_event(
            &tx,
            &EventMetadata::SwitchRecorded {
                phase: record.phase,
                switch_id: record.switch_id.to_owned(),
                target_deployment: record.target.to_owned(),
                host: record.host.map(str::to_owned),
                victims: record
                    .victims
                    .iter()
                    .map(|v| format!("{}/{}", v.deployment_id, v.instance))
                    .collect(),
                detail: record.detail.chars().take(512).collect(),
            },
        )
        .map_err(|error| match error {
            crate::events::EventWriteError::Sql(error) => LifecycleError::Sql(error),
            _ => LifecycleError::Invalid,
        })?;
        tx.commit()?;
        Ok(())
    }

    /// W10 gap (c): forget a switch that ended without a terminal record (its
    /// caller gave up). Status stops showing it, and (W10 gap (a)) every
    /// closure it still holds on a victim it listed ends, reopening a gate only
    /// it held exactly as [`Self::reopen_after_switch`] would.
    pub fn end_switch(
        &self,
        s: &CoordinatorSession,
        switch_id: &str,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        let listed: Option<String> = tx
            .query_row(
                "SELECT victims_json FROM active_switches WHERE switch_id=?1",
                [switch_id],
                |r| r.get(0),
            )
            .optional()?;
        let victims: Vec<String> = listed
            .map(|raw| serde_json::from_str(&raw).map_err(|_| LifecycleError::CorruptStoredData))
            .transpose()?
            .unwrap_or_default();
        for victim in victims {
            let (deployment, instance) = victim
                .rsplit_once('/')
                .and_then(|(d, i)| Some((d.to_owned(), i.parse::<u32>().ok()?)))
                .ok_or(LifecycleError::CorruptStoredData)?;
            end_victim_closure(&tx, switch_id, &deployment, instance, None)?;
        }
        crate::switch_state::record_switch_phase(&tx, switch_id, "", None, &[], None, false)?;
        tx.commit()?;
        Ok(())
    }
}

/// W10 gap (a), SPEC §10, §13.2: end the switch closure `switch_id` holds on
/// one victim (of `generation`, or of any generation when `None`) and reopen
/// its gate when only that closure held it: a READY, admitted, unsuspended
/// incarnation with no lifecycle run in flight and no other closure reason. A
/// victim another switch in progress also lists keeps its closure.
fn end_victim_closure(
    tx: &Transaction<'_>,
    switch_id: &str,
    deployment: &str,
    instance: u32,
    generation: Option<i64>,
) -> Result<(), LifecycleError> {
    let name = format!("{deployment}/{instance}");
    let shared: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_switches a, json_each(a.victims_json) v
                        WHERE a.switch_id!=?1 AND v.value=?2)",
        params![switch_id, name],
        |r| r.get(0),
    )?;
    if shared {
        return Ok(());
    }
    let generations: Vec<i64> = tx
        .prepare(
            "SELECT generation FROM dispatch_closures
              WHERE deployment_id=?1 AND instance_index=?2 AND reason='switch' AND (?3 IS NULL OR generation=?3)",
        )?
        .query_map(params![deployment, instance, generation], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for generation in generations {
        crate::switch_state::clear_closure(
            tx,
            deployment,
            instance,
            generation,
            crate::switch_state::ClosureReason::Switch,
        )?;
        tx.execute(
            &format!(
                "UPDATE deployment_instances AS i SET dispatch_enabled=1
                  WHERE i.deployment_id=?1 AND i.instance_index=?2 AND i.generation=?3
                    AND i.desired_state='ready' AND i.observed_state='ready' AND i.admission_enabled=1
                    AND i.dispatch_enabled=0
                    AND NOT EXISTS(SELECT 1 FROM deployments d WHERE d.id=i.deployment_id AND d.suspended=1)
                    AND NOT {} AND {}",
                open_runs_clause("i"),
                crate::switch_state::no_closure_clause("i")
            ),
            params![deployment, instance, generation],
        )?;
    }
    Ok(())
}

/// W10 gap (e): the resource owners whose retained launches make `host`
/// unable to take `target`'s instance `instance` (see `placement::candidates`,
/// `occupied`): every other retained launch on a host whose agent holds one
/// journal claim at a time, or the target's other instances on a host with
/// per-launch claims. Empty on a host fencing per instance or the embedded one.
fn host_occupants(
    tx: &Transaction<'_>,
    host: &str,
    target: &str,
    instance: u32,
) -> Result<Vec<String>, LifecycleError> {
    let mode: i64 = tx.query_row(
        "SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1) THEN 2
                ELSE COALESCE((SELECT MAX(CASE mode WHEN 'per_instance' THEN 2 WHEN 'per_launch' THEN 1 ELSE 0 END)
                                 FROM host_launch_claims WHERE host_id=?1),0) END",
        [host],
        |r| r.get(0),
    )?;
    if mode == 2 {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, u32)> = tx
        .prepare(
            "SELECT DISTINCT b.deployment_id,b.instance_index FROM runtime_bindings b
              WHERE b.state!='released' AND NOT (b.deployment_id=?2 AND b.instance_index=?3)
                AND (?4=0 OR b.deployment_id=?2)
                AND (EXISTS(SELECT 1 FROM remote_binding_ingress r WHERE r.binding_id=b.id AND r.host_id=?1)
                     OR EXISTS(SELECT 1 FROM deployment_instances i WHERE i.deployment_id=b.deployment_id
                                  AND i.instance_index=b.instance_index AND i.host_id=?1))
              ORDER BY b.deployment_id,b.instance_index",
        )?
        .query_map(params![host, target, instance, mode], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(deployment, index)| instance_owner_id(&deployment, index))
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn plan_in(
    tx: &Transaction<'_>,
    target: &str,
    only: Option<u32>,
    explicit: bool,
    eligible: placement::Eligible<'_>,
    protected: &BTreeSet<String>,
    activity: &dyn Fn(&str, i64) -> Option<i64>,
    assumed: &Assumed,
) -> Result<Planned, LifecycleError> {
    // Rule 1: a dispatch-open READY instance serves; an activation in flight
    // is joined, never planned twice (T15). `only` (an explicit `start
    // instance --evict`) narrows both to that instance.
    let settled: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployment_instances i JOIN deployments d ON d.id=i.deployment_id
                 WHERE i.deployment_id=?1 AND i.observed_state='ready' AND i.dispatch_enabled=1
                   AND i.admission_enabled=1 AND d.suspended=0 AND (?2 IS NULL OR i.instance_index=?2))
             OR EXISTS(SELECT 1 FROM lifecycle_runs r JOIN operations o ON o.id=r.operation_id
                 WHERE r.deployment_id=?1 AND o.kind IN ('initialize','restore')
                   AND r.state IN ('queued','running','uncertain') AND (?2 IS NULL OR r.instance_index=?2))",
        params![target, only],
        |r| r.get(0),
    )?;
    if settled {
        return Ok(Planned::Settled);
    }
    let revision: i64 = tx
        .query_row(
            "SELECT revision FROM deployments WHERE id=?1",
            [target],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(LifecycleError::NotFound)?;
    // Q5: a parked instance wakes in place (sticky placement); otherwise the
    // lowest stopped instance the operator left eligible starts. An explicit
    // start lifts the operator's per-instance stops (Q5, Q7), so it plans for
    // an instance the operator stopped too.
    let parked: Option<(u32, Option<String>)> = tx
        .query_row(
            &format!(
                "SELECT i.instance_index,i.host_id FROM deployment_instances i
                  WHERE i.deployment_id=?1 AND i.state='active' AND i.observed_state='parked'
                    AND (?3 OR i.operator_stopped=0) AND NOT {} AND (?2 IS NULL OR i.instance_index=?2)
                  ORDER BY i.instance_index LIMIT 1",
                open_runs_clause("i")
            ),
            params![target, only, explicit],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (instance, wake) = match parked {
        Some((k, _)) => (k, true),
        None => {
            let cold: Option<u32> = tx
                .query_row(
                    &format!(
                        "SELECT i.instance_index FROM deployment_instances i
                          WHERE i.deployment_id=?1 AND i.state='active' AND (?3 OR i.operator_stopped=0)
                            AND NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id
                                 AND b.instance_index=i.instance_index AND b.state!='released')
                            AND NOT {} AND (?2 IS NULL OR i.instance_index=?2)
                          ORDER BY i.instance_index LIMIT 1",
                        open_runs_clause("i")
                    ),
                    params![target, only, explicit],
                    |r| r.get(0),
                )
                .optional()?;
            // Nothing startable: the activation answers with its own refusal.
            let Some(k) = cold else {
                return Ok(Planned::Settled);
            };
            (k, false)
        }
    };
    let owner = instance_owner_id(target, instance);
    let spec = placement_spec(tx, target, revision)?;
    let mut hosts = placement::candidates(tx, target, revision, instance, eligible, true)?;
    if let Some((_, Some(parked_on))) = &parked {
        hosts.retain(|c| &c.host_id == parked_on);
        let (_, e, _) = launch(tx, target, instance)?;
        let peak = footprints(&e).wake;
        for c in &mut hosts {
            c.footprint = peak.clone();
            // ADR 0019: it wakes on the GPU it parked on, no other.
            c.device_options.clear();
        }
    }
    // Owner decision 2026-09-25: earlier instances of the same evicting start
    // are charged where they go, and their victims are already released.
    for c in &mut hosts {
        for released in &assumed.released {
            c.ledger.owners.remove(released);
        }
        for (on, charged, footprint) in &assumed.charges {
            if *on == c.host_id {
                c.ledger.owners.insert(charged.clone(), footprint.clone());
                c.instances_here += 1;
            }
        }
        if c.occupied && !assumed.released.is_empty() {
            let occupants = host_occupants(tx, &c.host_id, target, instance)?;
            if occupants.iter().all(|o| assumed.released.contains(o)) {
                c.occupied = false;
            }
        }
    }
    struct HostChoice {
        host: String,
        instances_here: u32,
        victims: Vec<String>,
        device: Option<String>,
    }
    let mut best: Option<(HostChoice, Vec<SwitchVictim>)> = None;
    let mut refusals: Vec<&'static str> = Vec::new();
    // Owner decision 2026-09-25: each host's reason, for the operator.
    let mut notes: Vec<String> = Vec::new();
    for c in &hosts {
        if !c.eligible {
            refusals.push("host_ineligible");
            notes.push(format!("host {} is not eligible for placement", c.host_id));
            continue;
        }
        if !wake && spec.max_per_host.is_some_and(|max| c.instances_here >= max) {
            refusals.push("max_per_host");
            notes.push(format!(
                "host {} already runs max_per_host instances",
                c.host_id
            ));
            continue;
        }
        // Owner decision 2026-09-23 (solo first start): a whole-host start
        // fits only where no other owner holds a charge.
        let alone = !c.whole_host || c.ledger.owners.keys().all(|other| *other == owner);
        // ADR 0019: on a multi-GPU host, on any of its GPUs.
        if candidate_fits(c, &owner).is_ok() && !c.occupied && alone {
            // Rule 2: it fits without eviction; placement takes it, on the
            // host placement's own order chooses.
            let last_host: Option<String> = tx
                .query_row(
                    "SELECT host_id FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                    params![target, instance],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let (chosen, device) = mllm_scheduler::placement::place(
                &hosts,
                &owner,
                placement::strategy(&spec),
                if wake { None } else { spec.max_per_host },
                last_host.as_deref(),
            )
            .map(|p| (p.host_id, p.device))
            .unwrap_or_else(|_| {
                (
                    c.host_id.clone(),
                    candidate_fits(c, &owner)
                        .ok()
                        .and_then(|(_, device)| device),
                )
            });
            return Ok(Planned::Fits {
                host: Some(chosen),
                device,
            });
        }
        let rows: Vec<(String, u32, i64)> = tx
            .prepare(&format!(
                "SELECT i.deployment_id,i.instance_index,i.generation FROM deployment_instances i
                   JOIN deployments d ON d.id=i.deployment_id
                  WHERE i.deployment_id!=?2 AND i.state='active' AND d.kind='model'
                    AND (i.host_id=?1 OR (i.host_id IS NULL AND EXISTS(SELECT 1 FROM host_effective_revisions h
                         WHERE h.deployment_id=i.deployment_id AND h.revision=i.revision AND h.host_id=?1 AND h.outcome='resolved')))
                    AND i.observed_state='ready' AND i.dispatch_enabled=1 AND i.admission_enabled=1
                    AND d.suspended=0 AND i.generation IS NOT NULL AND NOT {}
                    AND NOT EXISTS(SELECT 1 FROM lifecycle_claims c WHERE c.deployment_id=i.deployment_id AND c.instance_index=i.instance_index)
                    AND NOT {}
                  ORDER BY i.deployment_id,i.instance_index",
                open_runs_clause("i"),
                // SPEC §6.5: a warm-residency commitment is never a victim.
                crate::switch_state::warm_clause("i")
            ))?
            .query_map(params![c.host_id, target], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut offered = Vec::new();
        let mut by_owner = std::collections::BTreeMap::new();
        // Whether each offered victim may park at all: a victim of a solo
        // first start stops (a parked residual would still occupy the host it
        // empties), and SPEC §9.2: a victim with an uncertain request stops;
        // its release would refuse the park anyway.
        let mut parkable = std::collections::BTreeMap::new();
        for (deployment, index, generation) in rows {
            // Rule 4: never an instance serving a waiting group.
            if protected.contains(&deployment) {
                continue;
            }
            let victim_owner = instance_owner_id(&deployment, index);
            if !c.ledger.owners.contains_key(&victim_owner) {
                continue;
            }
            let elsewhere: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index!=?2
                         AND observed_state='ready' AND dispatch_enabled=1)",
                params![deployment, index],
                |r| r.get(0),
            )?;
            let (_, e, _) = launch(tx, &deployment, index)?;
            let may_park = !c.whole_host
                && parks(&e)
                && !park_refused(tx, &deployment, index, generation)?
                && !uncertain_leases(tx, &deployment, index)?;
            parkable.insert(victim_owner.clone(), may_park);
            offered.push(VictimCandidate {
                owner: victim_owner.clone(),
                serves_elsewhere: elsewhere,
                last_used_ms: activity(&deployment, generation)
                    .or_else(|| activity(&deployment, -1))
                    .unwrap_or(0),
                // Discrete GPU design §5: what it leaves charged once parked
                // (for `host_backed`, the weights copy on the system domain),
                // so the planner parks it only where that copy fits.
                parked: may_park.then(|| footprints(&e).parked),
            });
            by_owner.insert(victim_owner, (deployment, index, generation, elsewhere));
        }
        order_victims(&mut offered);
        // W10 gap (e): a host that cannot take another launch while it runs
        // these (a single-claim agent, or a per-launch one running another
        // instance of the target) is freed only by releasing every one of
        // them. Any that is not an eligible READY victim keeps it occupied.
        let occupants = if c.occupied {
            host_occupants(tx, &c.host_id, target, instance)?
        } else {
            Vec::new()
        };
        if occupants.iter().any(|o| !by_owner.contains_key(o)) {
            refusals.push("host_occupied");
            notes.push(format!(
                "host {} runs a launch that cannot be released for it",
                c.host_id
            ));
            continue;
        }
        let mut freed = c.ledger.clone();
        for occupant in &occupants {
            freed.owners.remove(occupant);
        }
        let rest: Vec<VictimCandidate> = offered
            .iter()
            .filter(|v| !occupants.contains(&v.owner))
            .cloned()
            .collect();
        let mut device = None;
        // W10 gap (e): an occupant is released at its tier as before; the
        // park it is accepted as is admitted against the ledger on its own.
        let occupant_releases = || {
            occupants
                .iter()
                .map(|o| Victim {
                    owner: o.clone(),
                    release: if parkable.get(o).copied().unwrap_or(false) {
                        Release::Park
                    } else {
                        Release::Stop
                    },
                })
                .collect::<Vec<_>>()
        };
        let decided = if c.whole_host {
            // Owner decision 2026-09-23, SPEC §10: a solo first start needs
            // the host empty, so the plan releases every other charge on it up
            // front (reclaimable parked instances are already left out of the
            // ledger and are reclaimed by the start itself). Any charge that
            // is not an eligible READY victim keeps the host from emptying.
            let others: Vec<String> = freed
                .owners
                .keys()
                .filter(|other| **other != owner)
                .cloned()
                .collect();
            let mut empty = freed.clone();
            empty.owners.retain(|other, _| *other == owner);
            if others.iter().all(|other| by_owner.contains_key(other))
                && fits(&empty, &owner, &c.footprint, &c.limits, c.max_parked).is_ok()
            {
                // Every victim of a solo first start stops.
                Ok(occupants
                    .iter()
                    .cloned()
                    .chain(
                        rest.iter()
                            .filter(|v| others.contains(&v.owner))
                            .map(|v| v.owner.clone()),
                    )
                    .map(|owner| Victim {
                        owner,
                        release: Release::Stop,
                    })
                    .collect::<Vec<_>>())
            } else {
                Err(mllm_scheduler::placement::HostRefusal::RequiresEmptyHost)
            }
        } else if !c.device_options.is_empty() {
            // ADR 0019 (discrete GPU design §7): eviction is per GPU; only
            // instances charged on a GPU's own domain can make room there.
            choose_device_with_eviction(
                &freed,
                &owner,
                &c.device_options,
                &c.limits,
                c.max_parked,
                &rest,
            )
            .map(|(on, more)| {
                device = Some(on);
                occupant_releases()
                    .into_iter()
                    .chain(more)
                    .collect::<Vec<_>>()
            })
        } else {
            choose_victims(&freed, &owner, &c.footprint, &c.limits, c.max_parked, &rest).map(
                |more| {
                    occupant_releases()
                        .into_iter()
                        .chain(more)
                        .collect::<Vec<_>>()
                },
            )
        };
        match decided {
            Ok(chosen) if chosen.is_empty() => {
                refusals.push("host_occupied");
                notes.push(format!("host {} cannot take another launch", c.host_id));
            }
            Ok(chosen) => {
                let mut victims = Vec::new();
                for victim in &chosen {
                    let (deployment, index, generation, elsewhere) =
                        by_owner[&victim.owner].clone();
                    let may_park = parkable.get(&victim.owner).copied().unwrap_or(false);
                    victims.push(SwitchVictim {
                        // Discrete GPU design §5: it parks only where its
                        // parked footprint fits the host after the switch.
                        parks: may_park && victim.release == Release::Park,
                        park_does_not_fit: may_park && victim.release == Release::Stop,
                        last_ready: false,
                        serves_elsewhere: elsewhere,
                        deployment_id: deployment,
                        instance: index,
                        generation,
                    });
                }
                let candidate = HostChoice {
                    host: c.host_id.clone(),
                    instances_here: c.instances_here,
                    victims: chosen.into_iter().map(|v| v.owner).collect(),
                    device: device.clone(),
                };
                // Rule 3: the host needing the least eviction; ties by fewer
                // last-READY victims, then the placement order (spread), then
                // host id.
                let key = |o: &HostChoice, v: &[SwitchVictim]| {
                    (
                        o.victims.len(),
                        v.iter().filter(|x| !x.serves_elsewhere).count(),
                        o.instances_here,
                        o.host.clone(),
                    )
                };
                let better = match &best {
                    None => true,
                    Some((o, v)) => key(&candidate, &victims) < key(o, v),
                };
                if better {
                    best = Some((candidate, victims));
                }
            }
            Err(refusal) => {
                refusals.push(refusal.code());
                notes.push(shortfall(
                    &c.host_id,
                    &freed,
                    &owner,
                    &c.footprint,
                    &c.limits,
                    &by_owner.keys().cloned().collect(),
                    refusal.code(),
                ));
            }
        }
    }
    let Some((choice, mut victims)) = best else {
        let code = match refusals.as_slice() {
            [] => "no_allowed_host",
            [first, rest @ ..] if rest.iter().all(|r| r == first) => *first,
            _ => "no_host_fits",
        };
        if notes.is_empty() {
            notes.push("no allowed host resolved the deployment's revision".into());
        }
        return Ok(Planned::Impossible {
            code: code.to_string(),
            detail: notes.join("; "),
        });
    };
    mark_last_ready(tx, &mut victims)?;
    let admission_window_ms = read_selected_policy(tx, &choice.host)
        .map_err(resource)?
        .map_or(DEFAULT_ADMISSION_WINDOW_MS, |p| {
            p.controls.queue.admission_window_ms
        });
    Ok(Planned::Evict(
        SwitchPlan::Evict {
            host: choice.host,
            instance,
            wake,
            victims,
            admission_window_ms,
        },
        choice.device,
    ))
}

/// Rule 5: a victim is its deployment's last READY instance when no READY,
/// dispatch-open instance of that deployment outside this release remains.
fn mark_last_ready(
    tx: &Transaction<'_>,
    victims: &mut [SwitchVictim],
) -> Result<(), LifecycleError> {
    let released: BTreeSet<(String, u32)> = victims
        .iter()
        .map(|v| (v.deployment_id.clone(), v.instance))
        .collect();
    for victim in victims.iter_mut() {
        let others: Vec<u32> = tx
            .prepare(
                "SELECT instance_index FROM deployment_instances WHERE deployment_id=?1
                   AND observed_state='ready' AND dispatch_enabled=1",
            )?
            .query_map([&victim.deployment_id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        victim.last_ready = others
            .iter()
            .all(|k| released.contains(&(victim.deployment_id.clone(), *k)));
    }
    Ok(())
}

/// Bytes shown to the operator, in GiB with one decimal.
fn gib(bytes: i64) -> String {
    format!("{:.1} GiB", bytes.max(0) as f64 / (1u64 << 30) as f64)
}

/// Owner decision 2026-09-25: why one host cannot take the instance even with
/// eviction, in the operator's terms: on the domain with the least slack, what
/// the instance needs, what is free as the ledger stands and what releasing
/// every eligible READY instance there would add.
fn shortfall(
    host: &str,
    ledger: &mllm_domain::resources::LedgerSnapshot,
    owner: &str,
    footprint: &PhaseFootprint,
    limits: &[MemoryLimit],
    evictable: &BTreeSet<String>,
    code: &str,
) -> String {
    let bytes = |f: &PhaseFootprint, domain: &str| -> i64 {
        f.allocations
            .iter()
            .filter(|a| a.domain == domain)
            .map(|a| a.bytes)
            .sum()
    };
    let worst = limits
        .iter()
        .filter(|l| bytes(footprint, &l.domain) > 0)
        .map(|l| {
            let need = bytes(footprint, &l.domain);
            let used: i64 = ledger
                .owners
                .iter()
                .filter(|(id, _)| id.as_str() != owner)
                .map(|(_, f)| bytes(f, &l.domain))
                .sum();
            let free = (l.managed_bytes - used).max(0);
            let released: i64 = ledger
                .owners
                .iter()
                .filter(|(id, _)| evictable.contains(id.as_str()))
                .map(|(_, f)| bytes(f, &l.domain))
                .sum();
            (free + released - need, need, free, released)
        })
        .min();
    match worst {
        Some((_, need, free, released)) => format!(
            "host {host} needs {}, {} free and {} evictable ({code})",
            gib(need),
            gib(free),
            gib(released)
        ),
        None => format!("host {host}: {code}"),
    }
}

/// See [`crate::Store::plan_start_switch`].
fn plan_start_in(
    tx: &Transaction<'_>,
    target: &str,
    eligible: placement::Eligible<'_>,
    protected: &BTreeSet<String>,
    activity: &dyn Fn(&str, i64) -> Option<i64>,
) -> Result<StartSwitchPlan, LifecycleError> {
    let revision: i64 = tx
        .query_row(
            "SELECT revision FROM deployments WHERE id=?1",
            [target],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(LifecycleError::NotFound)?;
    // SPEC §6.3 "Restore if parked, initialize if stopped": the start wakes
    // every parked instance first, then starts the cold ones in index order.
    let order: Vec<u32> = tx
        .prepare(&format!(
            "SELECT i.instance_index FROM deployment_instances i
              WHERE i.deployment_id=?1 AND i.state='active' AND NOT {}
                AND (i.observed_state='parked'
                     OR NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id
                                    AND b.instance_index=i.instance_index AND b.state!='released'))
              ORDER BY i.observed_state!='parked', i.instance_index",
            open_runs_clause("i")
        ))?
        .query_map([target], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut assumed = Assumed::default();
    let mut steps: Vec<StartStep> = Vec::new();
    for k in order {
        let host = match plan_in(
            tx,
            target,
            Some(k),
            true,
            eligible,
            protected,
            activity,
            &assumed,
        )? {
            Planned::Settled => continue,
            Planned::Fits { host, device } => host.map(|host| (host, device)),
            Planned::Evict(
                SwitchPlan::Evict {
                    host,
                    instance,
                    wake,
                    victims,
                    admission_window_ms,
                },
                device,
            ) => {
                for v in &victims {
                    assumed
                        .released
                        .insert(instance_owner_id(&v.deployment_id, v.instance));
                }
                steps.push(StartStep {
                    host: host.clone(),
                    instance,
                    wake,
                    victims,
                    admission_window_ms,
                });
                Some((host, device))
            }
            Planned::Evict(..) => continue,
            Planned::Impossible { code, detail } => {
                return Ok(StartSwitchPlan::Impossible {
                    instance: k,
                    code,
                    detail,
                })
            }
        };
        // Charged as placement charges a start accepted but not yet armed.
        // ADR 0019: on the GPU it goes to, on a multi-GPU host.
        if let Some((host, device)) = host {
            let (raw, _) = frozen_on_device(tx, target, revision, Some(&host), device.as_deref())?;
            let e =
                decode_effective_snapshot(&raw).map_err(|_| LifecycleError::CorruptStoredData)?;
            assumed.charges.push((
                host,
                instance_owner_id(target, k),
                super::startup::steady(&e),
            ));
        }
    }
    // Rule 5 over the whole start: a deployment losing several instances to
    // it keeps its fairness window on the last of them.
    let mut all: Vec<SwitchVictim> = steps.iter().flat_map(|s| s.victims.clone()).collect();
    mark_last_ready(tx, &mut all)?;
    let mut marked = all.into_iter();
    for step in &mut steps {
        for victim in &mut step.victims {
            if let Some(m) = marked.next() {
                victim.last_ready = m.last_ready;
            }
        }
    }
    Ok(StartSwitchPlan::Steps(steps))
}
