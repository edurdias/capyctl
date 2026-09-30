use capyctl_adapters::traits::{OwnedProcessLaunch, RenderedCommand, RuntimeError};
use capyctl_domain::completion::{Presence, ProcessIdentity};
use std::time::Duration;

/// A launcher that offers only the vLLM-era contract: no descriptor support.
struct Minimal;

impl OwnedProcessLaunch for Minimal {
    fn spawn_durable(&self, _: &str, _: &RenderedCommand) -> Result<ProcessIdentity, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
    fn present(&self, _: &ProcessIdentity) -> Presence {
        Presence::Unknown
    }
    fn observe_group(&self, _: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
    fn terminate_owned(&self, _: &[ProcessIdentity], _: Duration) -> Result<(), RuntimeError> {
        Err(RuntimeError::Unsupported)
    }
}

/// A launcher without descriptor support refuses the protected spawn rather
/// than faking it, so the vLLM path is untouched by the descriptor capability.
// T16
#[test]
fn default_spawn_durable_protected_is_unsupported() {
    let descriptors = capyctl_adapters::protected::ProtectedLaunchDescriptors::new(
        b"{}",
        b"inference-secret",
        b"admin-secret",
    )
    .unwrap();
    let command = RenderedCommand {
        argv: vec!["engine".into()],
        env: Default::default(),
    };
    assert!(matches!(
        Minimal.spawn_durable_protected("incarnation", &command, &descriptors),
        Err(RuntimeError::Unsupported)
    ));
}
