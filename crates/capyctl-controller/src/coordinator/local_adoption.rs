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

/// An adopted runtime never spawns a launch: the launch it stands for already
/// ran, and a second spawn for the same incarnation would be a different
/// engine. ADR 0033 §7: only a restart-parked launch's wake starts its engine
/// again, recorded under its restore step through the store's association.
struct NoLaunch(StoreAssociation);

impl LaunchAssociation for NoLaunch {
    fn persist_api_identity(
        &self,
        _identity: &capyctl_domain::completion::ProcessIdentity,
    ) -> Result<(), AssociationError> {
        Err(AssociationError::Uncertain(
            "an adopted runtime never launches".into(),
        ))
    }

    fn persist_lmcache_identity(
        &self,
        _identity: &capyctl_domain::completion::ProcessIdentity,
    ) -> Result<(), AssociationError> {
        Err(AssociationError::Uncertain(
            "an adopted runtime never starts a server".into(),
        ))
    }

    fn persist_wake_api_identity(
        &self,
        wake: &str,
        identity: &capyctl_domain::completion::ProcessIdentity,
    ) -> Result<(), AssociationError> {
        self.0.persist_wake_api_identity(wake, identity)
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
                // ADR 0031 §7: a custom engine was given the key its launch
                // sealed, only when its manifest uses `{api_key}`; the fresh
                // one the spec carries is never what the engine holds.
                AdapterSpec::Custom { api_key, .. } => {
                    if api_key.is_some() {
                        *api_key = Some(recorded(SecretRole::Inference)?);
                    }
                }
                // ADR 0023 §3, ADR 0029 §4: TensorFold and llama.cpp hold no
                // key to recover.
                AdapterSpec::Tensorfold { .. } | AdapterSpec::Llamacpp { .. } => {}
            }
            // ADR 0033 §§3, 7: an adopted launch runs on the server its
            // Initialize started (recorded beside its engine), never a new
            // one; a launch that recorded no server runs without LMCache.
            if let AdapterSpec::Vllm { lmcache, .. } | AdapterSpec::Sglang { lmcache, .. } =
                &mut spec
            {
                let server = capyctl_domain::completion::lmcache_members(
                    &owner
                        .store()
                        .runtime_binding_identities(work.binding_id())
                        .map_err(|error| {
                            CoordinatorError::Service(format!(
                                "the launch's recorded processes are unreadable: {error}"
                            ))
                        })?,
                );
                *lmcache = match lmcache.as_ref().and_then(|state| state.launch()) {
                    Some(capyctl_adapters::lmcache::LmcacheLaunch::Start(plan))
                        if !server.is_empty() =>
                    {
                        Some(capyctl_adapters::lmcache::LmcacheState::adopted(server, plan))
                    }
                    _ => None,
                };
            }
        }
        let lmcache = spec_lmcache(&spec);
        // ADR 0033 §§7, 10: an adopted restart-parked launch's wake starts its
        // engine again on the running server, recorded under the restore step
        // and spawned under the launch's retention.
        let retention_zero = work.lmcache().is_some_and(|plan| plan.retention_zero);
        let tools = tools_factory(
            Arc::new(NoLaunch(StoreAssociation {
                owner: owner.clone(),
                fence: work.fence().clone(),
                binding_id: work.binding_id().to_owned(),
            })),
            capyctl_launchers::SpawnPolicy {
                no_core_dumps: retention_zero,
                memory_logs: retention_zero,
            },
        );
        let engine = bindings.adapter(declared, spec, tools.clone())?;
        // SPEC §§6.2, 9.1: an adopted launch (one started before the rule
        // reached this host) is refused its Park and Restore as a fresh one is.
        let engine = crate::capability_gate::CapabilityGate::wrap(engine, work.effective());
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
            lmcache,
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
