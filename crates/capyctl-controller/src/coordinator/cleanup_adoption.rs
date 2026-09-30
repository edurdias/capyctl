//! Resumption of Stops a retired coordinator session accepted (W12).
//!
//! SPEC §13.2: reconcile before dispatch. A crash between accepting a Stop and
//! proving its engine gone left the cleanup with the dead session, so nothing
//! could finish it and every later Stop was refused: the deployment stayed
//! charged forever. The restarted worker adopts each such cleanup (the store
//! transfers it, with the launch it stops, in one revalidated transaction) and
//! retains a rebuilt runtime for the launch, so the ordinary cleanup path arms
//! it and completes it only on gone evidence for exactly the recorded group.
//! Nothing here sends a control or releases anything.
use super::*;

/// Adopt every retired cleanup this worker can drive: remote ones with the
/// remote factory, embedded ones with the local adoption factory. A cleanup
/// whose runtime cannot be rebuilt, or that no longer validates, stays with its
/// retired session and the refusal is journaled.
pub(super) async fn adopt_retired_cleanups(
    shared: &Arc<Shared>,
    remote: Option<&DriverFactory>,
    local: Option<&DriverFactory>,
) -> Result<(), CoordinatorError> {
    let retired = shared
        .read(|owner, now| owner.store().retired_cleanups(owner.session(), now))
        .await?;
    for cleanup in retired {
        let Some(factory) = (if cleanup.remote { remote } else { local }) else {
            continue;
        };
        let deployment = cleanup.work.fence().deployment_id.clone();
        let operation = cleanup.receipt.operation_id.clone();
        let binding = cleanup.receipt.binding_id.clone();
        let driver = match factory(&cleanup.work) {
            Ok(driver) => driver,
            Err(error) => {
                refused(
                    shared,
                    &deployment,
                    &operation,
                    &format!("its runtime could not be rebuilt: {error}"),
                )
                .await;
                continue;
            }
        };
        let step = cleanup.receipt.step_id.clone();
        if let Err(error) = shared
            .read(move |owner, now| {
                owner
                    .store()
                    .adopt_retired_cleanup(owner.session(), &step, now)
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

/// Journal why a Stop was not resumed. SPEC §17: the journal is evidence only,
/// so a journal that cannot be written changes nothing about the Stop.
async fn refused(shared: &Arc<Shared>, deployment: &str, operation: &str, reason: &str) {
    let entry = capyctl_adapters::vllm::args::redact_text(&format!(
        "deployment {deployment}: a restarted controller did not resume its accepted \
         Stop: {reason}; the engine stays owned and charged and nothing was released"
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
