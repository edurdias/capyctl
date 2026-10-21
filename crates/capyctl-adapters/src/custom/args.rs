//! ADR 0031 §2: one custom engine command from a resolved plan. The manifest
//! is the whole command: the executable, then each `args` entry with its
//! placeholders filled, one argument per entry, never split and never passed
//! through a shell (SPEC §8.1). CapyCTL validates none of the program's
//! options (SPEC §8.2); what it owns is the listener address and port it
//! renders, the closed environment, the key and the log.
use std::collections::BTreeMap;

use capyctl_config::custom::{CustomManifest, PlaceholderValues, Unset};
use capyctl_config::engine_env::{is_owned, EnvSource, ResolvedEnv};

use crate::traits::RenderedCommand;

/// The values CapyCTL fixes for every custom engine (ADR 0031 §2): CapyCTL's
/// model store fetches models, the engine does not.
const OFFLINE: [(&str, &str); 2] = [("HF_HUB_OFFLINE", "1"), ("TRANSFORMERS_OFFLINE", "1")];
/// Variables taken from the agent's own environment, as for the other
/// engines.
const PASS_THROUGH: &[&str] = &["HOME", "CUDA_VISIBLE_DEVICES"];

/// One custom launch, resolved.
#[derive(Clone, PartialEq, Eq)]
pub struct PlanInputCustom {
    /// The profile's executable, absolute (ADR 0031 §1).
    pub executable: String,
    /// The manifest the profile carries, as it validated (ADR 0031 §2).
    pub manifest: CustomManifest,
    /// What the placeholders render: the leased port, the absolute checkpoint
    /// directory, the served name, the context length and the launch key.
    pub values: PlaceholderValues,
    /// ADR 0028 §2.1, ADR 0031 §2: the resolved engine environment. Entries
    /// whose source is the manifest are templates, rendered here; profile and
    /// deployment values are literal.
    pub engine_env: ResolvedEnv,
    /// The launch's private engine log (SPEC §13.3).
    pub engine_log: Option<String>,
    /// Discrete GPU design §7: the pinned device, when the host has several.
    pub cuda_namespace: Option<capyctl_config::effective::CudaNamespace>,
}

/// SPEC §13.3: the key and environment values stay out of every Debug
/// rendering; the manifest's own text holds placeholders, never the key.
impl std::fmt::Debug for PlanInputCustom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanInputCustom")
            .field("executable", &self.executable)
            .field("manifest", &self.manifest)
            .field("values", &self.values)
            .field(
                "engine_env",
                &self.engine_env.vars.keys().collect::<Vec<_>>(),
            )
            .field("engine_log", &self.engine_log)
            .field("cuda_namespace", &self.cuda_namespace)
            .finish()
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum CustomArgsError {
    /// ADR 0031 §2: a placeholder the launch cannot fill
    /// (`custom_placeholder_unset:<name>`).
    #[error("{0}")]
    Unset(Unset),
    /// ADR 0031 §2, ADR 0028 §2.1 rule 1: an environment name CapyCTL owns,
    /// refused again at launch.
    #[error("engine_env_reserved:{0}")]
    Reserved(String),
}

impl CustomArgsError {
    /// The closed reason a launch is refused with.
    pub fn code(&self) -> String {
        self.to_string()
    }
}

impl From<Unset> for CustomArgsError {
    fn from(unset: Unset) -> Self {
        Self::Unset(unset)
    }
}

/// ADR 0031 §2: the argument vector, `argv[0]` the executable, and the
/// engine's own environment (the manifest's rendered entries, the profile's
/// and the deployment's, from lowest to highest precedence; resolution
/// already layered them). A name CapyCTL owns is refused again here.
pub fn render_command(plan: &PlanInputCustom) -> Result<RenderedCommand, CustomArgsError> {
    let mut argv = vec![plan.executable.clone()];
    argv.extend(plan.values.render_args(&plan.manifest)?);
    let mut env = BTreeMap::new();
    for (name, (value, source)) in &plan.engine_env.vars {
        if is_owned(name) {
            return Err(CustomArgsError::Reserved(name.clone()));
        }
        let value = match source {
            EnvSource::Manifest => plan.values.render(value)?,
            EnvSource::Profile | EnvSource::Deployment => value.clone(),
        };
        env.insert(name.clone(), value);
    }
    Ok(RenderedCommand { argv, env })
}

/// The closed environment one launch starts with (SPEC §13.3, ADR 0031 §2):
/// the engine's own names (`rendered`), then what CapyCTL owns: `HOME` and
/// `CUDA_VISIBLE_DEVICES` from the agent (or the pinned device), `PATH` the
/// executable's directory then the fixed system directories, both offline
/// switches, and the log metadata. Nothing else of the agent's environment.
pub fn engine_environment(
    rendered: &BTreeMap<String, String>,
    plan: &PlanInputCustom,
    inherited: &dyn Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let mut env = rendered.clone();
    for name in PASS_THROUGH {
        match inherited(name) {
            Some(value) => env.insert((*name).to_owned(), value),
            None => env.remove(*name),
        };
    }
    if let Some(namespace) = &plan.cuda_namespace {
        for (name, value) in namespace.environment() {
            env.insert(name.into(), value);
        }
    }
    let directory = std::path::Path::new(&plan.executable)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_string_lossy().into_owned());
    env.insert(
        "PATH".into(),
        crate::engine_env::tool_path(directory.as_deref(), None, crate::engine_env::SYSTEM_PATH),
    );
    for (name, value) in OFFLINE {
        env.insert(name.into(), value.into());
    }
    if let Some(log) = &plan.engine_log {
        env.insert("CAPYCTL_ENGINE_LOG".into(), log.clone());
    }
    // ADR 0031 §7: the writer owns the key by value, wherever the manifest
    // put it (an argument, or a variable whose name says nothing).
    if let Some(key) = &plan.values.api_key {
        env.insert(
            crate::engine_log::OWNED_VALUES_VARIABLE.into(),
            crate::engine_log::encode_owned_values([key.as_str()]),
        );
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use capyctl_config::custom::CustomManifest;

    fn manifest(text: &str) -> CustomManifest {
        CustomManifest::parse(text).unwrap()
    }

    fn plan(manifest: CustomManifest, key: Option<&str>) -> PlanInputCustom {
        PlanInputCustom {
            executable: "/home/u/venv/bin/basal-serve".into(),
            manifest,
            values: PlaceholderValues {
                port: 40123,
                model_dir: "/srv/models/basal omni".into(),
                served_name: "basal".into(),
                context_length: Some(4096),
                api_key: key.map(str::to_owned),
            },
            engine_env: ResolvedEnv::default(),
            engine_log: Some("/var/log/capyctl/engines/i-1.log".into()),
            cuda_namespace: None,
        }
    }

    // T44 (ADR 0031 §2): each placeholder is one argument or a part of one,
    // never split; `{host}` is 127.0.0.1 and the port the leased one.
    #[test]
    fn each_placeholder_renders_into_one_argument() {
        let plan = plan(
            manifest(
                "kind: custom_engine\nschema_version: 1\napi: decisions\n\
                 args: [--mode, eager, --host, \"{host}\", --port, \"{port}\", --model, \"{model_dir}\", \"--ctx={context_length}\", \"--key={api_key}\"]\n",
            ),
            Some("k3y-0123456789"),
        );
        let command = render_command(&plan).unwrap();
        assert_eq!(
            command.argv,
            vec![
                "/home/u/venv/bin/basal-serve",
                "--mode",
                "eager",
                "--host",
                "127.0.0.1",
                "--port",
                "40123",
                "--model",
                "/srv/models/basal omni",
                "--ctx=4096",
                "--key=k3y-0123456789",
            ]
        );
    }

    // T44 (ADR 0031 §2, SPEC §13.3): the closed environment: the engine's
    // names below CapyCTL's, PATH the executable's directory then the
    // system directories, both offline switches, HOME and the device from
    // the agent, nothing else inherited.
    #[test]
    fn the_environment_is_closed_and_layered() {
        let mut plan = plan(
            manifest(
                "kind: custom_engine\nschema_version: 1\napi: chat\n\
                 args: [\"{host}\", \"{port}\", \"{model_dir}\"]\n\
                 env: {SERVE_AUTH: \"{api_key}\", SERVE_NAME: \"m-{served_name}\", SHARED: manifest}\n",
            ),
            Some("k3y-0123456789"),
        );
        plan.engine_env.vars = BTreeMap::from([
            (
                "SERVE_AUTH".into(),
                ("{api_key}".into(), EnvSource::Manifest),
            ),
            (
                "SERVE_NAME".into(),
                ("m-{served_name}".into(), EnvSource::Manifest),
            ),
            (
                "SHARED".into(),
                ("deployment".into(), EnvSource::Deployment),
            ),
            ("PROFILE_ONLY".into(), ("{port}".into(), EnvSource::Profile)),
        ]);
        let command = render_command(&plan).unwrap();
        let inherited = |name: &str| match name {
            "HOME" => Some("/home/capyctl".to_owned()),
            "CUDA_VISIBLE_DEVICES" => Some("0".to_owned()),
            _ => Some("leaked".to_owned()),
        };
        let env = engine_environment(&command.env, &plan, &inherited);
        assert_eq!(env["SERVE_AUTH"], "k3y-0123456789");
        assert_eq!(env["SERVE_NAME"], "m-basal");
        assert_eq!(env["SHARED"], "deployment", "the deployment's value wins");
        assert_eq!(env["PROFILE_ONLY"], "{port}", "a profile value is literal");
        assert_eq!(env["HOME"], "/home/capyctl");
        assert_eq!(env["CUDA_VISIBLE_DEVICES"], "0");
        assert_eq!(env["HF_HUB_OFFLINE"], "1");
        assert_eq!(env["TRANSFORMERS_OFFLINE"], "1");
        assert!(env["PATH"].starts_with("/home/u/venv/bin:"));
        assert!(env["PATH"].ends_with(crate::engine_env::SYSTEM_PATH));
        assert_eq!(
            env["CAPYCTL_ENGINE_LOG"],
            "/var/log/capyctl/engines/i-1.log"
        );
        assert_eq!(
            crate::engine_log::decode_owned_values(&env[crate::engine_log::OWNED_VALUES_VARIABLE]),
            vec!["k3y-0123456789"]
        );
        let names: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "CAPYCTL_ENGINE_LOG",
                "CAPYCTL_ENGINE_LOG_OWNS",
                "CUDA_VISIBLE_DEVICES",
                "HF_HUB_OFFLINE",
                "HOME",
                "PATH",
                "PROFILE_ONLY",
                "SERVE_AUTH",
                "SERVE_NAME",
                "SHARED",
                "TRANSFORMERS_OFFLINE",
            ]
        );
    }

    // T44 (ADR 0031 §2, §10): a reserved name is refused again at launch,
    // and an unset placeholder names itself.
    #[test]
    fn reserved_names_and_unset_placeholders_refuse_the_launch() {
        let mut reserved = plan(
            manifest(
                "kind: custom_engine\nschema_version: 1\napi: chat\n\
                 args: [\"{host}\", \"{port}\", \"{model_dir}\"]\n",
            ),
            None,
        );
        reserved.engine_env.vars =
            BTreeMap::from([("LD_PRELOAD".into(), ("/x.so".into(), EnvSource::Deployment))]);
        assert_eq!(
            render_command(&reserved).unwrap_err().code(),
            "engine_env_reserved:LD_PRELOAD"
        );
        let mut unset = plan(
            manifest(
                "kind: custom_engine\nschema_version: 1\napi: chat\n\
                 args: [\"{host}\", \"{port}\", \"{model_dir}\", \"{context_length}\"]\n",
            ),
            None,
        );
        unset.values.context_length = None;
        assert_eq!(
            render_command(&unset).unwrap_err().code(),
            "custom_placeholder_unset:context_length"
        );
    }

    // T44 (ADR 0031 §7, SPEC §13.3): neither the plan's nor the rendered
    // command's Debug shows the key, even when it sits in an argument.
    #[test]
    fn debug_redacts_the_key_from_the_argv() {
        let plan = plan(
            manifest(
                "kind: custom_engine\nschema_version: 1\napi: chat\n\
                 args: [\"{host}\", \"{port}\", \"{model_dir}\", \"--api-key={api_key}\"]\n",
            ),
            Some("k3y-0123456789"),
        );
        let mut command = render_command(&plan).unwrap();
        command.env = engine_environment(&command.env, &plan, &|_| None);
        let shown = format!("{command:?} {plan:?}");
        assert!(!shown.contains("k3y-0123456789"), "{shown}");
        assert!(shown.contains("--api-key=<redacted>"), "{shown}");
    }
}
