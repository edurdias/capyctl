//! Action-first CLI grammar: `mllm <action> <resource> [identifier] [options]`.
//!
//! Clap-derive backed parsing that maps onto the stable `Command` shape
//! consumed by the role wiring. `parse` accepts both `&str` arrays and
//! `&[OsString]` (argv including the program name).

use std::ffi::OsString;
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
    /// Write a starter server configuration.
    Server,
    /// Write a starter host configuration.
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
    about = "Run vLLM and SGLang models on your own GPUs",
    disable_help_subcommand = true
)]
struct Cli {
    /// The configuration file of the server, host or standalone this command
    /// acts on.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Where `init` and `invite` write their file. `--output json` is the
    /// same as `--format json`.
    #[arg(long, global = true, value_name = "TARGET")]
    output: Option<String>,
    /// How a command that reads records prints them: an aligned table (the
    /// default, terminal or not) or the JSON result, as scripts need it.
    #[arg(long, global = true, value_name = "FORMAT", value_parser = ["table", "json"])]
    format: Option<String>,
    /// Short for `--format json`.
    #[arg(long, global = true, conflicts_with = "format")]
    json: bool,
    /// A ULID that names this change. Running the command again with the
    /// same id continues the same change instead of starting a new one.
    #[arg(long, global = true, value_name = "ID")]
    request_id: Option<String>,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum CliCommand {
    /// Start the server, a host, standalone, or a deployment.
    Start {
        #[command(subcommand)]
        target: StartTarget,
    },
    /// Write a starter configuration file for a server or a host.
    Init {
        #[command(subcommand)]
        target: InitTarget,
    },
    /// Create a single-use invitation file that lets a GPU machine join the
    /// server. With `--recover`, let a revoked host rejoin as itself.
    Invite {
        resource: HostWord,
        /// The host: a new host's name, or with `--recover` a revoked host's
        /// name or id. The same as `--name`.
        #[arg(conflicts_with = "name", required_unless_present = "name")]
        host: Option<String>,
        /// The host's name (instead of the positional argument).
        #[arg(long)]
        name: Option<String>,
        /// Create a recovery invitation for a revoked host.
        #[arg(long)]
        recover: bool,
    },
    /// Join this machine to a server with an invitation file. With
    /// `--recover`, rejoin a revoked host as itself; its state is kept.
    Join {
        resource: HostWord,
        /// The invitation file from `mllm invite host`.
        #[arg(long)]
        join_file: PathBuf,
        /// Redeem a recovery invitation.
        #[arg(long)]
        recover: bool,
    },
    /// List hosts, deployments or engines as a table.
    List {
        #[command(subcommand)]
        resource: ListArgs,
    },
    /// Print the full JSON record of a host or a deployment.
    Inspect {
        #[command(subcommand)]
        resource: InspectArgs,
    },
    /// Check a host. Not available in this release.
    Doctor {
        resource: HostWord,
        /// A host, by name or id.
        host: String,
    },
    /// Create a deployment from a YAML file, or update one.
    Deploy {
        #[command(subcommand)]
        resource: DeployArgs,
    },
    /// Show a deployment's state and its instances.
    Status {
        #[command(subcommand)]
        resource: StatusArgs,
    },
    /// Park a deployment: free its GPU memory and keep it ready to wake. The
    /// next request for it wakes it.
    Park {
        resource: DeploymentWord,
        /// A deployment, by name or id.
        deployment: String,
    },
    /// Stop a deployment's engines, or one instance. The deployment is kept.
    Stop {
        #[command(subcommand)]
        target: StopTarget,
    },
    /// Start a deployment, check it answers, then park it, so its first
    /// request only has to wake it.
    Preinitialize {
        resource: DeploymentWord,
        /// A deployment, by name or id.
        deployment: String,
    },
    /// Delete a deployment and its routes once its engines are confirmed
    /// stopped. Model files and caches on hosts are never touched.
    Delete {
        #[command(subcommand)]
        resource: DeleteArgs,
    },
    /// Check a configuration file without starting anything.
    Validate {
        #[command(subcommand)]
        resource: ValidateArgs,
    },
    /// Stop every engine on a host and keep its deployments; they start again
    /// when a request needs them. Stopping the mllm service does not do this.
    Drain {
        #[command(subcommand)]
        resource: DrainArgs,
    },
    /// Disconnect a host for good. Its mllm process exits with code 14; its
    /// engines keep running but get no more requests, and their GPU memory
    /// stays counted as used. Bring it back with `invite host --recover` and
    /// `join host --recover`.
    Revoke {
        #[command(subcommand)]
        resource: RevokeArgs,
    },
    /// Remove downloaded model copies that no deployment uses from this
    /// machine's model store. Deleting a deployment never does this.
    Prune {
        #[command(subcommand)]
        resource: PruneArgs,
    },
    /// Register, list or remove the vLLM and SGLang installations on this
    /// machine. Each one becomes a runtime profile deployments can name.
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
        /// An engine argument added to every launch with this profile
        /// (repeatable).
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// This machine's runtime profiles and whether the server accepted them.
    List,
    /// Remove a runtime profile.
    Remove {
        /// The profile name.
        name: String,
        /// Stop the deployments on this machine that use it first.
        #[arg(long)]
        drain: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum PruneArgs {
    /// List the copies under `<model store>/sources` that no deployment
    /// uses; remove them with `--apply`. A download in progress is never
    /// touched.
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
    /// Revoke a host. Revoking it again changes nothing.
    Host {
        /// A host, by name or id.
        host: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum DeleteArgs {
    /// Delete a deployment. Refused while any of its engines may still be
    /// running or starting, unless `--stop` is given.
    Deployment {
        /// A deployment, by name or id.
        deployment: String,
        /// Stop it first, wait until its engines are confirmed gone, then
        /// delete. If that cannot be confirmed yet it reports `pending`; run
        /// it again with the same `--request-id` to continue.
        #[arg(long)]
        stop: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum DrainArgs {
    /// Drain a host through the server.
    Host {
        /// A host, by name or id.
        host: String,
        /// For an offline host, wait until it reconnects and its engines stop,
        /// instead of returning at once.
        #[arg(long)]
        wait: bool,
    },
    /// Drain the engines of the standalone instance on this machine.
    Standalone,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum StartTarget {
    /// Start the server.
    Server,
    /// Start a host on this GPU machine.
    Host {
        /// Retain full native engine logs in private files (may contain secrets).
        #[arg(long)]
        debug_engine_logs: bool,
    },
    /// Start the server and one host together on this machine.
    Standalone {
        /// Retain full native engine logs in private files (may contain secrets).
        #[arg(long)]
        debug_engine_logs: bool,
    },
    /// Start a deployment's engines.
    Deployment {
        /// A deployment, by name or id.
        deployment: String,
        /// How long the engine may take to load (for example `20m`), instead
        /// of the deployment's `timeouts.initialize`.
        #[arg(long, value_name = "DURATION")]
        initialize_timeout: Option<String>,
        /// Make room by parking or stopping idle deployments, and report which.
        /// If it cannot fit even then, nothing is touched and the start fails
        /// with exit code 4. Without it a start never evicts anything.
        #[arg(long)]
        evict: bool,
        // SPEC §6.4: a partial start is never a success.
        /// Wait until every instance is ready; exit 0 only then. Exits 4 when
        /// there is no room, 13 when the launch fails, 10 on timeout and 15
        /// when no host can take it, with the reason.
        #[arg(long)]
        wait: bool,
    },
    // Owner decision Q7 (kept out of the help text, which users read).
    /// Start one instance, `<deployment>/<index>`.
    Instance {
        #[arg(value_parser = parse_instance)]
        instance: (String, u32),
        /// How long the engine may take to load, instead of the deployment's
        /// `timeouts.initialize`.
        #[arg(long, value_name = "DURATION")]
        initialize_timeout: Option<String>,
        /// Make room on its host by parking or stopping idle deployments, and
        /// report which.
        #[arg(long)]
        evict: bool,
        // SPEC §6.4: wait for the start's operation.
        /// Wait for the start to finish; on failure print the reason.
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum StopTarget {
    /// Stop every engine of a deployment.
    Deployment {
        /// A deployment, by name or id.
        deployment: String,
    },
    // Owner decision Q7 (kept out of the help text, which users read).
    /// Stop one instance, `<deployment>/<index>`.
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
    /// Hosts joined to the server and whether they are connected.
    Hosts,
    /// Deployments, their state and the hosts they run on.
    Deployments,
    /// Every host's registered engines.
    Engines,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum InspectArgs {
    /// A host's full record.
    Host {
        /// A host, by name or id.
        host: String,
    },
    /// A deployment's full record.
    Deployment {
        /// A deployment, by name or id.
        deployment: String,
        /// Include the configuration as resolved for launch.
        #[arg(long)]
        effective_config: bool,
    },
    /// A role's configuration. Not available in this release.
    Config {
        /// The role: server, host or standalone.
        #[arg(long)]
        role: Option<String>,
        /// Show it with defaults filled in.
        #[arg(long)]
        effective: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum DeployArgs {
    /// Deploy a model from a deployment file.
    Model {
        /// The deployment file (YAML).
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Start it as soon as it is accepted.
        #[arg(long)]
        activate: bool,
        /// With `--activate`: wait until it is ready.
        #[arg(long)]
        wait: bool,
        /// With `--activate`: how long the engine may take to load, instead of
        /// the deployment's `timeouts.initialize`.
        #[arg(long, value_name = "DURATION", requires = "activate")]
        initialize_timeout: Option<String>,
        // SPEC §14: an update is explicit and revision-aware; ADR 0013 §7
        // decides which changes restart instances.
        /// Update the existing deployment the file names. N is its current
        /// revision, as `mllm list deployments` shows it. Changing only the
        /// instance count keeps running instances; any other change restarts
        /// them.
        #[arg(long, value_name = "N", conflicts_with = "activate",
              value_parser = clap::value_parser!(i64).range(1..))]
        revision: Option<i64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum StatusArgs {
    /// A deployment's state and its instances.
    Deployment {
        /// A deployment, by name or id.
        deployment: String,
        /// Keep printing updates. Not available in this release.
        #[arg(long)]
        watch: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum ValidateArgs {
    /// Check one server, host, standalone or deployment file.
    Config {
        /// The file to check.
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
                StartTarget::Server => Command::Start(Role::Server),
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
                debug_engine_logs: true
            } | StartTarget::Host {
                debug_engine_logs: true
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

/// Website spec, Docs: the clap definition the CLI reference is generated
/// from. The same `Cli` the binary parses with, so the reference cannot drift.
pub fn command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}
