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
    Server,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Start,
    Park,
    Stop,
    Preinitialize,
    Undeploy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListResource {
    Hosts,
    Deployments,
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
    Invite { name: String },
    Join { join_file: PathBuf },
    List { resource: ListResource },
    Inspect { resource: Resource, id: Option<String>, effective: bool },
    Doctor { host: String },
    Qualify { deployment: String },
    Deploy { file: Option<PathBuf>, activate: bool, wait: bool },
    Status { deployment: String, watch: bool },
    Lifecycle { action: LifecycleAction, deployment: String },
    Validate { file: PathBuf },
}

impl Command {
    pub fn label(&self) -> String {
        match self {
            Command::Start(role) => format!("start {role:?}").to_lowercase(),
            Command::Init(target) => format!("init {target:?}").to_lowercase(),
            Command::Invite { name } => format!("invite host {name}"),
            Command::Join { join_file } => {
                format!("join host --join-file {}", join_file.display())
            }
            Command::List { resource } => format!("list {resource:?}").to_lowercase(),
            Command::Inspect { resource, id, effective } => {
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
            Command::Qualify { deployment } => format!("qualify deployment {deployment}"),
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
                    LifecycleAction::Undeploy => ("undeploy", "model"),
                };
                format!("{action} {resource} {deployment}")
            }
            Command::Validate { file } => format!("validate config --file {}", file.display()),
        }
    }
}

#[derive(Parser)]
#[command(name = "mllm", version, about = "mllm control-plane CLI", disable_help_subcommand = true)]
struct Cli {
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(long, global = true, value_name = "TARGET")]
    output: Option<String>,
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum CliCommand {
    Start { #[command(subcommand)] target: StartTarget },
    Init { #[command(subcommand)] target: InitTarget },
    Invite {
        resource: HostWord,
        #[arg(long)]
        name: String,
    },
    Join {
        resource: HostWord,
        #[arg(long)]
        join_file: PathBuf,
    },
    List { #[command(subcommand)] resource: ListArgs },
    Inspect { #[command(subcommand)] resource: InspectArgs },
    Doctor {
        resource: HostWord,
        host: String,
    },
    Qualify {
        resource: DeploymentWord,
        deployment: String,
    },
    Deploy { #[command(subcommand)] resource: DeployArgs },
    Status { #[command(subcommand)] resource: StatusArgs },
    Park {
        resource: DeploymentWord,
        deployment: String,
    },
    Stop {
        resource: DeploymentWord,
        deployment: String,
    },
    Preinitialize {
        resource: DeploymentWord,
        deployment: String,
    },
    Undeploy {
        resource: ModelWord,
        deployment: String,
    },
    Validate { #[command(subcommand)] resource: ValidateArgs },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum StartTarget {
    Server,
    Host,
    Standalone,
    Deployment { deployment: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
enum ListArgs {
    Hosts,
    Deployments,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum InspectArgs {
    Host { host: String },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModelWord {
    Model,
}

impl From<CliCommand> for Command {
    fn from(cli: CliCommand) -> Self {
        match cli {
            CliCommand::Start { target } => match target {
                StartTarget::Server => Command::Start(Role::Server),
                StartTarget::Host => Command::Start(Role::Host),
                StartTarget::Standalone => Command::Start(Role::Standalone),
                StartTarget::Deployment { deployment } => {
                    Command::Lifecycle { action: LifecycleAction::Start, deployment }
                }
            },
            CliCommand::Init { target } => Command::Init(target),
            CliCommand::Invite { name, .. } => Command::Invite { name },
            CliCommand::Join { join_file, .. } => Command::Join { join_file },
            CliCommand::List { resource } => match resource {
                ListArgs::Hosts => Command::List { resource: ListResource::Hosts },
                ListArgs::Deployments => Command::List { resource: ListResource::Deployments },
            },
            CliCommand::Inspect { resource } => match resource {
                InspectArgs::Host { host } => Command::Inspect {
                    resource: Resource::Host,
                    id: Some(host),
                    effective: false,
                },
                InspectArgs::Deployment { deployment, effective_config } => Command::Inspect {
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
            CliCommand::Qualify { deployment, .. } => Command::Qualify { deployment },
            CliCommand::Deploy { resource } => match resource {
                DeployArgs::Model { file, activate, wait } => {
                    Command::Deploy { file, activate, wait }
                }
            },
            CliCommand::Status { resource } => match resource {
                StatusArgs::Deployment { deployment, watch } => {
                    Command::Status { deployment, watch }
                }
            },
            CliCommand::Park { deployment, .. } => {
                Command::Lifecycle { action: LifecycleAction::Park, deployment }
            }
            CliCommand::Stop { deployment, .. } => {
                Command::Lifecycle { action: LifecycleAction::Stop, deployment }
            }
            CliCommand::Preinitialize { deployment, .. } => {
                Command::Lifecycle { action: LifecycleAction::Preinitialize, deployment }
            }
            CliCommand::Undeploy { deployment, .. } => {
                Command::Lifecycle { action: LifecycleAction::Undeploy, deployment }
            }
            CliCommand::Validate { resource } => match resource {
                ValidateArgs::Config { file } => Command::Validate { file },
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub command: Command,
    pub config: Option<PathBuf>,
    pub output: Option<String>,
}

pub fn parse_invocation<I, T>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    Ok(Invocation {
        command: cli.command.into(),
        config: cli.config,
        output: cli.output,
    })
}

pub fn parse<I, T>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    parse_invocation(args).map(|inv| inv.command)
}