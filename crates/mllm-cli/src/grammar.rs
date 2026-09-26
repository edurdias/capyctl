//! Action-first CLI grammar: `mllm <action> <resource> [identifier] [options]`.
//!
//! Clap-derive backed parsing that maps onto the stable `Command` shape
//! consumed by the role wiring. `parse` accepts both `&str` arrays and
//! `&[OsString]` (argv including the program name).

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CliError {
    #[error(transparent)]
    Clap(#[from] clap::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum Role {
    Server,
    Host,
    Standalone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum InitTarget {
    Server,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Start,
    Park,
    Stop,
    Preinitialize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListResource {
    Hosts,
    Deployments,
    /// ADR 0018: every host's published runtime profiles, from the server.
    Engines,
}

/// ADR 0018 §1: `--deep-park` on `engine add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DeepParkChoice {
    Enabled,
    Disabled,
}

/// ADR 0018 §1: `--drift` on `engine add` (ADR 0008 `installation_drift`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum DriftChoice {
    #[default]
    Warn,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Host,
    Deployment,
    Config,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Start(Role),
    Init(InitTarget),
    /// SPEC §4.1: a short-lived, single-use host invitation. ADR 0016: with
    /// `recover`, the invitation re-enrolls the revoked host `name` (its name
    /// or id) under its same identity instead of enrolling a new host.
    Invite {
        name: String,
        recover: bool,
    },
    /// SPEC §4.1: enroll this host with an invitation. ADR 0016: `recover`
    /// redeems a recovery invitation, keeping the host's state and journal.
    Join {
        join_file: PathBuf,
        recover: bool,
    },
    List {
        resource: ListResource,
    },
    Inspect {
        resource: Resource,
        id: Option<String>,
        effective: bool,
    },
    Doctor {
        host: String,
    },
    Deploy {
        file: Option<PathBuf>,
        activate: bool,
        wait: bool,
        /// SPEC §14: an update of an existing deployment names the revision it
        /// replaces (the management API's `expected_revision`).
        revision: Option<i64>,
    },
    Status {
        deployment: String,
        watch: bool,
    },
    Lifecycle {
        action: LifecycleAction,
        deployment: String,
    },
    /// Owner decision Q7 (ADR 0013 as amended): `start instance` and `stop
    /// instance` act on one instance, named `<deployment>/<index>`.
    InstanceLifecycle {
        action: LifecycleAction,
        deployment: String,
        instance: u32,
    },
    /// SPEC §6.3 (W6, owner decision 2026-09-23): `delete deployment` removes
    /// the deployment and its routes after verified cleanup. `stop` first stops
    /// every instance and waits for that cleanup; without it the command is
    /// refused while anything is still held.
    Delete {
        deployment: String,
        stop: bool,
    },
    /// SPEC §14 / §15.3: offline validation of one configuration document.
    /// `host` is the host role document a deployment is resolved against.
    Validate {
        file: PathBuf,
        host: Option<PathBuf>,
    },
    /// SPEC §4.3: explicit draining of a host's deployments, distinct from an
    /// ordinary role restart. `None` is the standalone role's embedded host.
    /// Owner decision 4 (2026-09-22): an offline host's drain returns at once
    /// with its Stops pending unless `wait` asks to wait for them.
    Drain {
        host: Option<String>,
        wait: bool,
    },
    /// SPEC §§4.1, 13.3: revoke an enrolled host's identity, by name or id.
    /// Its control session closes, it takes no new commands or placements,
    /// and dispatch to its engines closes; nothing it owns is released.
    Revoke {
        host: String,
    },
    /// SPEC §6.3, ADR 0008: remove materialized model sources in a host's
    /// model store that no deployment references. Host-side and explicit:
    /// without `apply` it only reports.
    PruneSources {
        host_config: PathBuf,
        apply: bool,
        referenced_file: Option<PathBuf>,
    },
    /// ADR 0018 §1: installations found on this machine; executes nothing.
    EngineDetect {
        paths: Vec<PathBuf>,
    },
    /// ADR 0018 §1: register an installation as a runtime profile.
    EngineAdd {
        path: Option<PathBuf>,
        name: Option<String>,
        deep_park: Option<DeepParkChoice>,
        drift: DriftChoice,
        args: Vec<String>,
    },
    /// ADR 0018 §1: this machine's runtime profiles.
    EngineList,
    /// ADR 0018 §4: remove a runtime profile, stopping its deployments with `drain`.
    EngineRemove {
        name: String,
        drain: bool,
    },
}

impl Command {
    pub fn label(&self) -> String {
        match self {
            Command::Start(role) => format!("start {role:?}").to_lowercase(),
            Command::Init(target) => format!("init {target:?}").to_lowercase(),
            Command::Invite { name, recover } => {
                format!(
                    "invite host {name}{}",
                    if *recover { " --recover" } else { "" }
                )
            }
            Command::Join { join_file, recover } => format!(
                "join host --join-file {}{}",
                join_file.display(),
                if *recover { " --recover" } else { "" }
            ),
            Command::List { resource } => format!("list {resource:?}").to_lowercase(),
            Command::Inspect {
                resource,
                id,
                effective,
            } => {
                let tail = if *effective { " --effective" } else { "" };
                match (resource, id) {
                    (Resource::Host, Some(id)) => format!("inspect host {id}"),
                    (Resource::Deployment, Some(id)) => format!("inspect deployment {id}"),
                    (Resource::Config, role) => match role {
                        Some(role) => format!("inspect config --role {role}{tail}"),
                        None => format!("inspect config{tail}"),
                    },
                    _ => "inspect".to_string(),
                }
            }
            Command::Doctor { host } => format!("doctor host {host}"),
            Command::Deploy { .. } => "deploy model".to_string(),
            Command::Status { deployment, watch } => {
                let tail = if *watch { " --watch" } else { "" };
                format!("status deployment {deployment}{tail}")
            }
            Command::Lifecycle { action, deployment } => {
                let (action, resource) = match action {
                    LifecycleAction::Start => ("start", "deployment"),
                    LifecycleAction::Park => ("park", "deployment"),
                    LifecycleAction::Stop => ("stop", "deployment"),
                    LifecycleAction::Preinitialize => ("preinitialize", "deployment"),
                };
                format!("{action} {resource} {deployment}")
            }
            Command::InstanceLifecycle {
                action,
                deployment,
                instance,
            } => {
                let action = match action {
                    LifecycleAction::Start => "start",
                    _ => "stop",
                };
                format!("{action} instance {deployment}/{instance}")
            }
            Command::Delete { deployment, stop } => {
                let tail = if *stop { " --stop" } else { "" };
                format!("delete deployment {deployment}{tail}")
            }
            Command::Validate { file, .. } => format!("validate config --file {}", file.display()),
            Command::Drain {
                host: Some(host), ..
            } => format!("drain host {host}"),
            Command::Drain { host: None, .. } => "drain standalone".to_string(),
            Command::Revoke { host } => format!("revoke host {host}"),
            Command::PruneSources { apply, .. } => {
                format!("prune sources{}", if *apply { " --apply" } else { "" })
            }
            Command::EngineDetect { .. } => "engine detect".into(),
            Command::EngineAdd { .. } => "engine add".into(),
            Command::EngineList => "engine list".into(),
            Command::EngineRemove { name, .. } => format!("engine remove {name}"),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "mllm",
    version,
    about = "mllm control-plane CLI",
    disable_help_subcommand = true
)]
struct Cli {
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(long, global = true, value_name = "TARGET")]
    output: Option<String>,
    /// How a command that reads records prints them: an aligned table (the
    /// default, terminal or not) or the JSON result, as scripts need it.
    #[arg(long, global = true, value_name = "FORMAT", value_parser = ["table", "json"])]
    format: Option<String>,
    /// Short for `--format json`.
    #[arg(long, global = true, conflicts_with = "format")]
    json: bool,
    #[arg(long, global = true, value_name = "ID")]
    request_id: Option<String>,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum CliCommand {
    Start {
        #[command(subcommand)]
        target: StartTarget,
    },
    Init {
        #[command(subcommand)]
        target: InitTarget,
    },
    /// Create a short-lived, single-use host invitation. With `--recover`,
    /// the invitation lets a revoked host (named by name or id) re-enroll
    /// under its same identity; its old certificate stays revoked.
    Invite {
        resource: HostWord,
        /// The host: a new host's name, or with `--recover` a revoked host's
        /// name or id. The same as `--name`.
        #[arg(conflicts_with = "name", required_unless_present = "name")]
        host: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        recover: bool,
    },
    /// Enroll this host with an invitation file. With `--recover`, redeem a
    /// recovery invitation: the host keeps its state and journal (or starts
    /// with fresh identity files if they were lost) and gets a new
    /// certificate for its same host id.
    Join {
        resource: HostWord,
        #[arg(long)]
        join_file: PathBuf,
        #[arg(long)]
        recover: bool,
    },
    List {
        #[command(subcommand)]
        resource: ListArgs,
    },
    Inspect {
        #[command(subcommand)]
        resource: InspectArgs,
    },
    Doctor {
        resource: HostWord,
        host: String,
    },
    Deploy {
        #[command(subcommand)]
        resource: DeployArgs,
    },
    Status {
        #[command(subcommand)]
        resource: StatusArgs,
    },
    Park {
        resource: DeploymentWord,
        deployment: String,
    },
    Stop {
        #[command(subcommand)]
        target: StopTarget,
    },
    Preinitialize {
        resource: DeploymentWord,
        deployment: String,
    },
    /// Remove a deployment and its routes after verified cleanup. Model files
    /// and caches on hosts are never touched.
    Delete {
        #[command(subcommand)]
        resource: DeleteArgs,
    },
    Validate {
        #[command(subcommand)]
        resource: ValidateArgs,
    },
    /// Stop every engine on a host with verified cleanup; its deployments stay
    /// eligible for on-demand activation. Signalling a role never does this.
    Drain {
        #[command(subcommand)]
        resource: DrainArgs,
    },
    /// Revoke an enrolled host's identity. Its control session closes at once,
    /// it can no longer reconnect, take commands or placements, and dispatch
    /// to its engines closes. The host role is told so and exits (code 14). Engines it runs are not stopped and their
    /// accounting is kept until an operator settles them with evidence. The
    /// host comes back only through `invite host <name|id> --recover` and
    /// `join host --recover`, under the same identity with a new certificate.
    Revoke {
        #[command(subcommand)]
        resource: RevokeArgs,
    },
    /// Remove materialized model sources that no deployment references, from
    /// this machine's model store. Deleting a deployment never does this.
    Prune {
        #[command(subcommand)]
        resource: PruneArgs,
    },
    /// Register vLLM and SGLang installations on this machine as runtime
    /// profiles, list them, or remove one. Acts on this machine's role.
    Engine {
        #[command(subcommand)]
        action: EngineArgs,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum EngineArgs {
    /// List vLLM and SGLang installations on this machine (reads metadata only).
    Detect {
        /// Also scan this directory (repeatable).
        #[arg(long = "path")]
        paths: Vec<PathBuf>,
    },
    /// Register an installation as a runtime profile and publish it.
    Add {
        /// A venv directory, its bin/vllm, or its bin/python3. Omit to pick interactively.
        path: Option<PathBuf>,
        /// The profile name (default: the engine's name).
        #[arg(long)]
        name: Option<String>,
        /// Record deep parking enabled or disabled (default: enabled unless
        /// the capability probe reports it missing).
        #[arg(long, value_enum)]
        deep_park: Option<DeepParkChoice>,
        /// What a launch does when the installation changed since registration.
        #[arg(long, value_enum, default_value_t = DriftChoice::Warn)]
        drift: DriftChoice,
        /// A host-fixed engine argument (repeatable).
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// This machine's runtime profiles and whether the server accepted them.
    List,
    /// Remove a runtime profile.
    Remove {
        name: String,
        /// Stop the deployments on this machine that use it first.
        #[arg(long)]
        drain: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum PruneArgs {
    /// Copies under `<model store>/sources` that no existing deployment
    /// declares. The referenced set comes from the server's management API
    /// (or `--referenced-file`); without it nothing is removed. Lists only,
    /// unless `--apply` is given. A download in progress is never touched.
    Sources {
        /// The host document naming the model store to prune.
        #[arg(long, value_name = "FILE")]
        host_config: PathBuf,
        /// Remove the unreferenced copies instead of listing them.
        #[arg(long)]
        apply: bool,
        /// The server's `GET /management/v1/model-sources` answer, saved to a
        /// file, for a host that cannot reach the management API itself.
        #[arg(long, value_name = "FILE")]
        referenced_file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum RevokeArgs {
    /// An enrolled host, by name or id. Revoking a revoked host changes
    /// nothing and reports it (`newly_revoked: false`).
    Host { host: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum DeleteArgs {
    /// A deployment, by name or id. Refused while any instance still holds a
    /// runtime, reservation, lease or open operation, unless `--stop` is given.
    Deployment {
        deployment: String,
        /// Stop every instance first, wait for verified cleanup, then delete.
        /// Reports `pending` with the Stops' operation ids when cleanup cannot
        /// be proven yet; rerun with the same --request-id to resume.
        #[arg(long)]
        stop: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum DrainArgs {
    /// An enrolled host, by name or id, through the server's management API.
    Host {
        host: String,
        /// Wait for the Stops of an offline host until it reconnects, up to
        /// the drain window, instead of returning with them pending.
        #[arg(long)]
        wait: bool,
    },
    /// The standalone role's embedded host, through its management API.
    Standalone,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum StartTarget {
    Server {
        /// Serve inference on this address for this run instead of the
        /// document's `listeners.inference.bind` (default 0.0.0.0:8443), for
        /// example 127.0.0.1:8443 or a Tailscale address. Wins over
        /// MLLM_INFERENCE_ADDR.
        #[arg(long, value_name = "ADDR:PORT", value_parser = parse_listen)]
        listen: Option<SocketAddr>,
        /// Serve inference without the API key for this run (design §9).
        /// Every client that can reach the address can use the models; a
        /// non-loopback address prints a warning. Wins over
        /// MLLM_INFERENCE_AUTH and listeners.inference.authentication.
        #[arg(long)]
        no_inference_auth: bool,
    },
    Host {
        /// Retain full native engine logs in private files (may contain secrets).
        #[arg(long)]
        debug_engine_logs: bool,
        /// The models directory a relative model path resolves under for this
        /// run (default ~/models). Wins over MLLM_MODELS_ROOT and
        /// model_store.path.
        #[arg(long, value_name = "DIR", value_parser = parse_models_root)]
        models_root: Option<PathBuf>,
        /// Allow or disable Hugging Face and HTTP model downloads for this
        /// run (default allowed). Wins over MLLM_MODEL_SOURCES and
        /// model_sources in the document.
        #[arg(long, value_name = "allowed|disabled", value_parser = parse_model_sources)]
        model_sources: Option<mllm_config::model_source::SourceSwitch>,
        /// The most bytes downloaded models may take (default 500GiB). Wins
        /// over MLLM_MODEL_SOURCES_MAX and model_sources.max_bytes.
        #[arg(long, value_name = "SIZE", value_parser = parse_model_sources_max)]
        model_sources_max: Option<String>,
    },
    Standalone {
        /// Retain full native engine logs in private files (may contain secrets).
        #[arg(long)]
        debug_engine_logs: bool,
        /// Serve inference on this address for this run instead of the
        /// document's `listeners.inference.bind` (default 0.0.0.0:8443), for
        /// example 127.0.0.1:8443 or a Tailscale address. Wins over
        /// MLLM_INFERENCE_ADDR.
        #[arg(long, value_name = "ADDR:PORT", value_parser = parse_listen)]
        listen: Option<SocketAddr>,
        /// Serve inference without the API key for this run (design §9).
        /// Every client that can reach the address can use the models; a
        /// non-loopback address prints a warning. Wins over
        /// MLLM_INFERENCE_AUTH and listeners.inference.authentication.
        #[arg(long)]
        no_inference_auth: bool,
        /// The models directory a relative model path resolves under for this
        /// run (default ~/models). Wins over MLLM_MODELS_ROOT and
        /// model_store.path.
        #[arg(long, value_name = "DIR", value_parser = parse_models_root)]
        models_root: Option<PathBuf>,
        /// Allow or disable Hugging Face and HTTP model downloads for this
        /// run (default allowed). Wins over MLLM_MODEL_SOURCES and
        /// model_sources in the document.
        #[arg(long, value_name = "allowed|disabled", value_parser = parse_model_sources)]
        model_sources: Option<mllm_config::model_source::SourceSwitch>,
        /// The most bytes downloaded models may take (default 500GiB). Wins
        /// over MLLM_MODEL_SOURCES_MAX and model_sources.max_bytes.
        #[arg(long, value_name = "SIZE", value_parser = parse_model_sources_max)]
        model_sources_max: Option<String>,
    },
    Deployment {
        deployment: String,
        /// Bound this start's Initialize instead of the deployment's
        /// `timeouts.initialize` (for example `20m`); at most its request deadline.
        #[arg(long, value_name = "DURATION")]
        initialize_timeout: Option<String>,
        /// Make room for every instance of the deployment by releasing other
        /// engines with the same switch plan a waiting request uses (drain
        /// within the switch drain timeout, then park or stop), and report
        /// them. The whole start is planned first and only what placement
        /// needs is released; if any instance cannot be placed even with
        /// eviction, nothing is released and the start is refused
        /// (capacity_blocked, exit code 4). Without it a start never evicts
        /// anything.
        #[arg(long)]
        evict: bool,
        /// SPEC §6.4: wait until every instance of the deployment is ready.
        /// Exits 0 only then; a partial start is never a success. An instance
        /// not placed before the start's deadline exits 4
        /// (insufficient_resources), a failed launch 13 (operation_failed),
        /// any other wait expiry 10 (activation_timeout); no allowed host
        /// eligible for placement exits 15 (host_ineligible). The failure
        /// prints the reason status shows.
        #[arg(long)]
        wait: bool,
    },
    /// Owner decision Q7: start one instance, `<deployment>/<index>`.
    Instance {
        #[arg(value_parser = parse_instance)]
        instance: (String, u32),
        /// Bound this start's Initialize instead of the deployment's
        /// `timeouts.initialize`; at most its request deadline.
        #[arg(long, value_name = "DURATION")]
        initialize_timeout: Option<String>,
        /// Make room for this instance by releasing other engines on one host
        /// with the same switch plan a waiting request uses, and report them.
        #[arg(long)]
        evict: bool,
        /// SPEC §6.4: wait for the start's operation to finish; a failure
        /// prints the reason and hint status shows for it.
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum StopTarget {
    Deployment {
        deployment: String,
    },
    /// Owner decision Q7: stop one instance, `<deployment>/<index>`.
    Instance {
        #[arg(value_parser = parse_instance)]
        instance: (String, u32),
    },
}

/// `<deployment>/<index>`: the deployment name or id, then a decimal index.
fn parse_instance(value: &str) -> Result<(String, u32), String> {
    let (deployment, index) = value
        .rsplit_once('/')
        .ok_or_else(|| "expected <deployment>/<index>".to_string())?;
    let parsed = index
        .parse::<u32>()
        .ok()
        .filter(|n| index == n.to_string() && *n < mllm_config::instances::MAX_INSTANCES)
        .ok_or_else(|| {
            "the instance index must be a decimal below the instance bound".to_string()
        })?;
    if deployment.is_empty() {
        return Err("expected <deployment>/<index>".into());
    }
    Ok((deployment.to_owned(), parsed))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
enum ListArgs {
    Hosts,
    Deployments,
    Engines,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum InspectArgs {
    Host {
        host: String,
    },
    Deployment {
        deployment: String,
        #[arg(long)]
        effective_config: bool,
    },
    Config {
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        effective: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum DeployArgs {
    Model {
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        #[arg(long)]
        activate: bool,
        #[arg(long)]
        wait: bool,
        /// With `--activate`: bound the start's Initialize instead of the
        /// deployment's `timeouts.initialize`; at most its request deadline.
        #[arg(long, value_name = "DURATION", requires = "activate")]
        initialize_timeout: Option<String>,
        /// Revise the existing deployment the file names, replacing exactly
        /// this revision (SPEC §14: an update is explicit and revision-aware).
        /// A count-only change keeps running instances; any other change
        /// stops and restarts them on the new revision (ADR 0013 §7).
        #[arg(long, value_name = "N", conflicts_with = "activate",
              value_parser = clap::value_parser!(i64).range(1..))]
        revision: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum StatusArgs {
    Deployment {
        deployment: String,
        #[arg(long)]
        watch: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum ValidateArgs {
    Config {
        #[arg(long, value_name = "FILE")]
        file: PathBuf,
        /// The host document a deployment file is resolved against.
        #[arg(long, value_name = "FILE")]
        host: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum HostWord {
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DeploymentWord {
    Deployment,
}

impl From<CliCommand> for Command {
    fn from(cli: CliCommand) -> Self {
        match cli {
            CliCommand::Start { target } => match target {
                StartTarget::Server { .. } => Command::Start(Role::Server),
                StartTarget::Host { .. } => Command::Start(Role::Host),
                StartTarget::Standalone { .. } => Command::Start(Role::Standalone),
                StartTarget::Deployment { deployment, .. } => Command::Lifecycle {
                    action: LifecycleAction::Start,
                    deployment,
                },
                StartTarget::Instance {
                    instance: (deployment, instance),
                    ..
                } => Command::InstanceLifecycle {
                    action: LifecycleAction::Start,
                    deployment,
                    instance,
                },
            },
            CliCommand::Init { target } => Command::Init(target),
            CliCommand::Invite {
                host,
                name,
                recover,
                ..
            } => Command::Invite {
                // clap requires exactly one of the two.
                name: host.or(name).unwrap_or_default(),
                recover,
            },
            CliCommand::Join {
                join_file, recover, ..
            } => Command::Join { join_file, recover },
            CliCommand::List { resource } => match resource {
                ListArgs::Hosts => Command::List {
                    resource: ListResource::Hosts,
                },
                ListArgs::Deployments => Command::List {
                    resource: ListResource::Deployments,
                },
                ListArgs::Engines => Command::List {
                    resource: ListResource::Engines,
                },
            },
            CliCommand::Inspect { resource } => match resource {
                InspectArgs::Host { host } => Command::Inspect {
                    resource: Resource::Host,
                    id: Some(host),
                    effective: false,
                },
                InspectArgs::Deployment {
                    deployment,
                    effective_config,
                } => Command::Inspect {
                    resource: Resource::Deployment,
                    id: Some(deployment),
                    effective: effective_config,
                },
                InspectArgs::Config { role, effective } => Command::Inspect {
                    resource: Resource::Config,
                    id: role,
                    effective,
                },
            },
            CliCommand::Doctor { host, .. } => Command::Doctor { host },
            CliCommand::Deploy { resource } => match resource {
                DeployArgs::Model {
                    file,
                    activate,
                    wait,
                    revision,
                    ..
                } => Command::Deploy {
                    file,
                    activate,
                    wait,
                    revision,
                },
            },
            CliCommand::Status { resource } => match resource {
                StatusArgs::Deployment { deployment, watch } => {
                    Command::Status { deployment, watch }
                }
            },
            CliCommand::Park { deployment, .. } => Command::Lifecycle {
                action: LifecycleAction::Park,
                deployment,
            },
            CliCommand::Stop { target } => match target {
                StopTarget::Deployment { deployment } => Command::Lifecycle {
                    action: LifecycleAction::Stop,
                    deployment,
                },
                StopTarget::Instance {
                    instance: (deployment, instance),
                } => Command::InstanceLifecycle {
                    action: LifecycleAction::Stop,
                    deployment,
                    instance,
                },
            },
            CliCommand::Preinitialize { deployment, .. } => Command::Lifecycle {
                action: LifecycleAction::Preinitialize,
                deployment,
            },
            CliCommand::Delete { resource } => match resource {
                DeleteArgs::Deployment { deployment, stop } => Command::Delete { deployment, stop },
            },
            CliCommand::Validate { resource } => match resource {
                ValidateArgs::Config { file, host } => Command::Validate { file, host },
            },
            CliCommand::Drain { resource } => match resource {
                DrainArgs::Host { host, wait } => Command::Drain {
                    host: Some(host),
                    wait,
                },
                DrainArgs::Standalone => Command::Drain {
                    host: None,
                    wait: false,
                },
            },
            CliCommand::Revoke {
                resource: RevokeArgs::Host { host },
            } => Command::Revoke { host },
            CliCommand::Prune {
                resource:
                    PruneArgs::Sources {
                        host_config,
                        apply,
                        referenced_file,
                    },
            } => Command::PruneSources {
                host_config,
                apply,
                referenced_file,
            },
            CliCommand::Engine { action } => match action {
                EngineArgs::Detect { paths } => Command::EngineDetect { paths },
                EngineArgs::Add {
                    path,
                    name,
                    deep_park,
                    drift,
                    args,
                } => Command::EngineAdd {
                    path,
                    name,
                    deep_park,
                    drift,
                    args,
                },
                EngineArgs::List => Command::EngineList,
                EngineArgs::Remove { name, drain } => Command::EngineRemove { name, drain },
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    pub config: Option<PathBuf>,
    pub output: Option<String>,
    /// Owner decision 2026-09-25: `--format table|json` (`--json` is
    /// `--format json`). `None` is the default, a table for record views.
    pub format: Option<String>,
    pub debug_engine_logs: bool,
    pub request_id: Option<String>,
    /// ADR 0014 amendment A1: `--initialize-timeout` on `deploy model
    /// --activate`, `start deployment` and `start instance`, in milliseconds.
    /// It wins over the deployment's `timeouts.initialize`.
    pub initialize_timeout_ms: Option<i64>,
    /// Owner decision 2026-09-23: `--evict` on `start deployment` and `start
    /// instance`: make room with the switch plan and report the victims.
    pub evict: bool,
    /// SPEC §6.4: `--wait` on `start deployment` and `start instance`.
    pub wait: bool,
    /// Design §9: `--listen <addr:port>` on `start standalone` and `start
    /// server`: the inference bind for this run.
    pub listen: Option<SocketAddr>,
    /// Design §9: `--no-inference-auth` on `start standalone` and `start
    /// server`: the inference key is off for this run.
    pub no_inference_auth: bool,
    /// Owner decision 2026-09-25: `--models-root`, `--model-sources` and
    /// `--model-sources-max` on `start standalone` and `start host`.
    pub model_overrides: mllm_config::model_settings::ModelOverrides,
}

pub fn parse_invocation<I, T>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut cli = Cli::try_parse_from(args)?;
    // SPEC §6.4 / §13: one request identity, one journal and one idempotency
    // key. ULID text is case-insensitive, so a retry typed in another case (or
    // any other spelling of the same value) is carried in canonical form.
    if let Some(id) = cli.request_id.take() {
        let parsed = id.parse::<ulid::Ulid>().map_err(|_| {
            CliError::Clap(clap::Error::raw(
                clap::error::ErrorKind::InvalidValue,
                "--request-id must be a ULID",
            ))
        })?;
        cli.request_id = Some(parsed.to_string());
    }
    let debug_engine_logs = matches!(
        &cli.command,
        CliCommand::Start {
            target: StartTarget::Standalone {
                debug_engine_logs: true,
                ..
            } | StartTarget::Host {
                debug_engine_logs: true,
                ..
            }
        }
    );
    let initialize_timeout_ms = match &cli.command {
        CliCommand::Start {
            target:
                StartTarget::Deployment {
                    initialize_timeout, ..
                }
                | StartTarget::Instance {
                    initialize_timeout, ..
                },
        }
        | CliCommand::Deploy {
            resource: DeployArgs::Model {
                initialize_timeout, ..
            },
        } => initialize_timeout
            .as_deref()
            .map(parse_timeout)
            .transpose()?,
        _ => None,
    };
    // Design §9: `--listen` on `start standalone` and `start server` only.
    let listen = match &cli.command {
        CliCommand::Start {
            target: StartTarget::Server { listen, .. } | StartTarget::Standalone { listen, .. },
        } => *listen,
        _ => None,
    };
    // Design §9: `--no-inference-auth` on the same two starts.
    let no_inference_auth = matches!(
        &cli.command,
        CliCommand::Start {
            target: StartTarget::Server {
                no_inference_auth: true,
                ..
            } | StartTarget::Standalone {
                no_inference_auth: true,
                ..
            },
        }
    );
    // Owner decision 2026-09-25: the model flags on `start standalone` and
    // `start host`, the two roles that hold a model store.
    let model_overrides = match &cli.command {
        CliCommand::Start {
            target:
                StartTarget::Standalone {
                    models_root,
                    model_sources,
                    model_sources_max,
                    ..
                }
                | StartTarget::Host {
                    models_root,
                    model_sources,
                    model_sources_max,
                    ..
                },
        } => mllm_config::model_settings::ModelOverrides {
            models_root: models_root.clone(),
            sources: *model_sources,
            sources_max: model_sources_max.clone(),
        },
        _ => Default::default(),
    };
    let evict = matches!(
        &cli.command,
        CliCommand::Start {
            target: StartTarget::Deployment { evict: true, .. }
                | StartTarget::Instance { evict: true, .. },
        }
    );
    let wait = matches!(
        &cli.command,
        CliCommand::Start {
            target: StartTarget::Deployment { wait: true, .. }
                | StartTarget::Instance { wait: true, .. },
        }
    );
    // SPEC §6.4: `--wait` observes the accepted target operation. Without
    // `--activate` a deploy's only operation is its durable acceptance, which
    // the command already returns after, so there is nothing to wait for and
    // the flag would be silently ignored. Refuse it instead.
    if matches!(
        &cli.command,
        CliCommand::Deploy {
            resource: DeployArgs::Model {
                wait: true,
                activate: false,
                ..
            }
        }
    ) {
        return Err(CliError::Clap(clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            "deploy model --wait requires --activate: without it the deployment is accepted durably and the command returns its id at once; there is no activation to wait for (use status deployment <id> to observe it)\n",
        )));
    }
    let command: Command = cli.command.into();
    if cli.request_id.is_some()
        && !matches!(
            command,
            Command::Deploy { .. }
                | Command::Drain { .. }
                | Command::Revoke { .. }
                | Command::InstanceLifecycle { .. }
                | Command::Delete { .. }
                | Command::Lifecycle {
                    action: LifecycleAction::Start
                        | LifecycleAction::Stop
                        | LifecycleAction::Park
                        | LifecycleAction::Preinitialize,
                    ..
                }
        )
    {
        return Err(CliError::Clap(clap::Error::raw(clap::error::ErrorKind::ArgumentConflict,"--request-id applies to deploy model, start, stop, park or preinitialize deployment, start or stop instance, delete deployment, drain and revoke host")));
    }
    Ok(Invocation {
        debug_engine_logs,
        command,
        config: cli.config,
        output: cli.output,
        format: if cli.json {
            Some("json".to_owned())
        } else {
            cli.format
        },
        request_id: cli.request_id,
        initialize_timeout_ms,
        evict,
        wait,
        listen,
        no_inference_auth,
        model_overrides,
    })
}

/// Owner decision 2026-09-25: `--models-root`, made absolute against the
/// working directory.
fn parse_models_root(text: &str) -> Result<PathBuf, String> {
    mllm_config::model_settings::absolute("--models-root", text).map_err(|error| error.detail)
}

/// Owner decision 2026-09-25: `--model-sources allowed|disabled`.
fn parse_model_sources(text: &str) -> Result<mllm_config::model_source::SourceSwitch, String> {
    mllm_config::model_settings::switch("--model-sources", text).map_err(|error| error.detail)
}

/// Owner decision 2026-09-25: `--model-sources-max <size>`, e.g. `500GiB`.
fn parse_model_sources_max(text: &str) -> Result<String, String> {
    mllm_config::model_settings::max_bytes("--model-sources-max", text)
        .map(|_| text.to_owned())
        .map_err(|error| error.detail)
}

/// Design §9: an inference address for `--listen`: a socket address with a
/// non-zero port that is not multicast (the document's rule).
fn parse_listen(text: &str) -> Result<SocketAddr, String> {
    mllm_config::standalone::inference_address(text).ok_or_else(|| {
        "must be an address and port such as 0.0.0.0:8443, 127.0.0.1:8443 or [::]:8443 \
         (non-zero port, not multicast)"
            .to_owned()
    })
}

/// A positive duration in the configuration's units (`90s`, `20m`, `1h`).
fn parse_timeout(text: &str) -> Result<i64, CliError> {
    mllm_config::effective::parse_duration_ms(text)
        .ok()
        .filter(|ms| *ms > 0)
        .ok_or_else(|| {
            CliError::Clap(clap::Error::raw(
                clap::error::ErrorKind::InvalidValue,
                "--initialize-timeout must be a positive duration such as 90s, 20m or 1h\n",
            ))
        })
}

pub fn parse<I, T>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    parse_invocation(args).map(|inv| inv.command)
}
