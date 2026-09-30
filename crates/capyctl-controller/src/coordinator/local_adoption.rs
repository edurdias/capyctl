//! Re-attachment of embedded engines after a standalone role restart.
//!
//! SPEC §4.3 (owner decision P3, 2026-09-22): stopping the standalone role is a
//! service restart, so the engines it launched keep running and stay owned. The
//! restarted coordinator starts a new session; before this module existed the
//! Ready launches of the retired session were nobody's — dispatch closed, no
//! runtime retained, and a Stop could not reach them.
//!
//! SPEC §13.2: reconcile before dispatch. The restarted worker adopts each Ready
//! embedded launch the retired session left (the store transfers it after
//! revalidating it in one transaction) and rebuilds its runtime from the frozen
//! binding and the per-launch keys the store already sealed, so Stop can
//! terminate exactly the recorded group. Nothing here launches, releases or
//! reopens dispatch: `crate::local_readiness` reopens dispatch only on fresh
//! local evidence.

use super::*;
use capyctl_adapters::resolve::AdapterSpec;
use capyctl_store::secrets::SecretRole;

/// An adopted runtime never spawns: the launch it stands for already ran, and a
/// second spawn for the same incarnation would be a different engine.
struct NoLaunch;

impl LaunchAssociation for NoLaunch {
    fn persist_api_identity(
        &self,
        _identity: &capyctl_domain::completion::ProcessIdentity,
    ) -> Result<(), AssociationError> {
        Err(AssociationError::Uncertain(
            "an adopted runtime never launches".into(),
        ))
    }
}

/// The driver an embedded worker rebuilds for an adopted launch.
///
/// SPEC §13.3: the engine was given its per-launch keys when it launched and the
/// store sealed them before it ran; the rebuilt adapter uses exactly those, so it
/// can still authenticate against the engine that is running. A launch whose keys
/// are unreadable is not adopted.
pub(super) fn factory(
    owner: SharedCoordinatorState,
    bindings: Arc<dyn EngineBindings>,
    tools_factory: ToolsFactory,
    clock: ServiceClock,
    grace: Duration,
) -> DriverFactory {
    Arc::new(move |work: &InitializeWork| {
        let declared = work.effective().profile.engine;
        let mut spec = bindings.spec(work)?;
        {
            let owner = owner.lock().map_err(|error| {
                drop(error);
                CoordinatorError::Service("ownership mutex poisoned".into())
            })?;
            let recorded = |role: SecretRole| -> Result<String, CoordinatorError> {
                owner
                    .store()
                    .engine_key(work.binding_id(), work.incarnation(), role)
                    .map_err(|error| {
                        CoordinatorError::Service(format!(
                            "the recorded {role:?} engine key is unreadable: {error}"
                        ))
                    })?
                    .map(hex::encode)
                    .ok_or_else(|| {
                        CoordinatorError::Service(format!(
                            "the launch recorded no {role:?} engine key"
                        ))
                    })
            };
            match &mut spec {
                AdapterSpec::Vllm {
                    engine_key,
                    admin_key,
                    ..
                } => {
                    if engine_key.is_some() {
                        *engine_key = Some(recorded(SecretRole::Inference)?);
                    }
                    // ADR 0012 migration: a launch sealed before the admin role
                    // runs the single-key guard until it restarts, so it is
                    // rebuilt with that one key. Never mint an admin key for a
                    // running engine: it was not given one and would refuse it.
                    // The next launch seals both roles.
                    *admin_key = owner
                        .store()
                        .engine_key(work.binding_id(), work.incarnation(), SecretRole::Admin)
                        .map_err(|error| {
                            CoordinatorError::Service(format!(
                                "the recorded Admin engine key is unreadable: {error}"
                            ))
                        })?
                        .map(hex::encode);
                }
                AdapterSpec::Sglang {
                    inference,
                    admin,
                    session,
                    ..
                } => {
                    *inference = recorded(SecretRole::Inference)?;
                    *admin = recorded(SecretRole::Admin)?;
                    *session = Some(owner.session().id().to_owned());
                }
            }
        }
        let tools = tools_factory(Arc::new(NoLaunch));
        let engine = bindings.adapter(declared, spec, tools.clone())?;
        Ok(Arc::new(Driver {
            engine,
            cleanup: terminate_then_prove_gone(
                tools.clone(),
                clock.clone(),
                grace,
                bindings.clone(),
            ),
            tools: Some(tools),
            settle: None,
        }))
    })
}

/// SPEC §4.3, §13.2: adopt every Ready embedded launch a retired session left
/// and retain its rebuilt runtime so Stop can reach it. A launch whose runtime
/// cannot be rebuilt, or that no longer validates, stays with its retired
/// session: nothing of it is released or dispatched, and the refusal is
/// journaled for the operator.
pub(super) async fn adopt_retired_local_launches(
    shared: &Arc<Shared>,
    factory: &DriverFactory,
) -> Result<(), CoordinatorError> {
    let retired = shared
        .read(|owner, _| owner.store().retired_local_launches(owner.session()))
        .await?;
    for launch in retired {
        let work = launch.work;
        let deployment = work.fence().deployment_id.clone();
        let operation = work.operation_id().to_owned();
        let binding = work.binding_id().to_owned();
        let driver = match factory(&work) {
            Ok(driver) => driver,
            Err(error) => {
                refused(
                    shared,
                    &deployment,
                    &operation,
                    &format!("its embedded runtime could not be rebuilt: {error}"),
                )
                .await;
                continue;
            }
        };
        let step = work.step_id().to_owned();
        if let Err(error) = shared
            .read(move |owner, _| {
                owner
                    .store()
                    .adopt_retired_local_launch(owner.session(), &step)
            })
            .await
        {
            if !shared.accepting.load(Ordering::Acquire) {
                return Err(error);
            }
            refused(shared, &deployment, &operation, &format!("{error}")).await;
            continue;
        }
        let mut retained = shared
            .retained
            .lock()
            .map_err(|_| shared.fail("runtime registry poisoned"))?;
        if retained.contains_key(&binding) {
            return Err(shared.fail("immutable runtime binding already retained"));
        }
        retained.insert(binding, driver);
    }
    Ok(())
}

/// Journal why a launch was not adopted. SPEC §17: the journal is evidence only,
/// so a journal that cannot be written changes nothing about the launch.
async fn refused(shared: &Arc<Shared>, deployment: &str, operation: &str, reason: &str) {
    let entry = capyctl_adapters::vllm::args::redact_text(&format!(
        "deployment {deployment}: a restarted standalone role did not adopt its ready \
         launch: {reason}; the engine stays owned by the retired session and nothing \
         was released"
    ));
    let operation = operation.to_owned();
    let _ = shared
        .with_owner(move |owner| {
            owner
                .store()
                .record_journal(None, Some(&operation), Some("adoption_refused"), &entry)
                .map_err(|error| CoordinatorError::Service(error.to_string()))
        })
        .await;
}
