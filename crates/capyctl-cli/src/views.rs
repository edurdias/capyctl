//! ADR 0021: the text form of every command result, built only from the JSON
//! result the command already returns (so text never shows what JSON does
//! not carry) plus the parsed command for names the result lacks.

use serde_json::Value;

use crate::detail::{record, Detail};
use crate::grammar::{Command, InitTarget, LifecycleAction, Resource};
use crate::table::{self, gib, HostNames, View};

pub struct Context<'a> {
    pub names: &'a HostNames,
    /// The `name` of the deployment file `deploy` read, when known.
    pub deployment_name: Option<String>,
}

/// Every command is named here on purpose: adding one to the grammar fails
/// the build until it is routed to a view or to the generic text.
pub fn render(command: &Command, value: &Value, context: &Context) -> String {
    match command {
        Command::List { .. }
        | Command::Status { .. }
        | Command::EngineList
        | Command::EngineDetect { .. } => match View::of(command) {
            Some(view) => table::render(view, value, context.names),
            None => generic(command, value),
        },
        Command::Doctor { .. } | Command::Start(_) | Command::ConfigShow { .. } => {
            generic(command, value)
        }
        Command::Deploy { .. } => deploy(value, context),
        Command::Lifecycle { action, deployment } => {
            lifecycle(*action, deployment, None, value, context)
        }
        Command::InstanceLifecycle {
            action,
            deployment,
            instance,
        } => lifecycle(*action, deployment, Some(*instance), value, context),
        Command::Delete { deployment, .. } => delete(deployment, value),
        Command::Init(target) => init(*target, value),
        Command::Invite { .. } => invite(value),
        Command::Join { .. } => Detail::new("Joined the server")
            .row("Host ID", s(&value["host_id"]))
            .render(),
        Command::Validate { .. } => validate(value),
        Command::EngineAdd { .. } => engine_add(value),
        Command::EngineRemove { .. } => engine_remove(value),
        Command::Drain { host, .. } => drain(host.as_deref(), value),
        Command::Revoke { host } => revoke(host, value),
        Command::PruneSources { .. } => prune(value),
        Command::Inspect { resource, id, .. } => {
            let kind = match resource {
                Resource::Host => "Host",
                Resource::Deployment => "Deployment",
                Resource::Config => "Configuration",
            };
            let name = value["name"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| id.clone())
                .unwrap_or_default();
            record(format!("{kind} {name}").trim_end(), value)
        }
    }
}

/// Commands without a purpose-built sentence: the name and every field.
fn generic(command: &Command, value: &Value) -> String {
    let name = format!("{command:?}");
    let name = name
        .split([' ', '(', '{'])
        .next()
        .unwrap_or("command")
        .to_lowercase();
    record(&format!("{name} done"), value)
}

/// A scalar as text; empty for null or absent.
fn s(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn opt(value: &Value) -> Option<String> {
    Some(s(value)).filter(|text| !text.is_empty())
}

fn verb(action: LifecycleAction) -> (&'static str, &'static str) {
    match action {
        LifecycleAction::Start => ("Start", "Started"),
        LifecycleAction::Park => ("Park", "Parked"),
        LifecycleAction::Stop => ("Stop", "Stopped"),
        LifecycleAction::Preinitialize => ("Preinitialize", "Preinitialized"),
    }
}

fn state_rows(detail: Detail, d: &Value, context: &Context) -> Detail {
    detail
        .row("Ready", table::ready(d))
        .row("Hosts", table::instance_hosts(d, context.names))
        .row("Operation", table::operation(&d["latest_operation"]))
}

fn deploy(value: &Value, context: &Context) -> String {
    let d = &value["deployment"];
    if d.is_object() {
        let name = opt(&d["name"])
            .or_else(|| context.deployment_name.clone())
            .unwrap_or_default();
        return Detail::new(format!("Deployed {name}: {}", s(&d["observed_state"])))
            .row("Revision", s(&d["revision"]))
            .row("Hosts", table::instance_hosts(d, context.names))
            .row("Ready", table::ready(d))
            .row_opt(
                "Startup",
                d["startup"]["bytes"]
                    .is_i64()
                    .then(|| table::startup(&d["startup"])),
            )
            .row_opt(
                "Context",
                d["context"]["tokens"]
                    .as_i64()
                    .map(|t| format!("{t} tokens")),
            )
            .row("Operation", table::operation(&d["latest_operation"]))
            .render();
    }
    let name = context
        .deployment_name
        .as_deref()
        .map(|n| format!(" {n}"))
        .unwrap_or_default();
    let summary = match (value["joined"].as_bool(), opt(&value["revision"])) {
        (Some(true), Some(rev)) => format!("Deployment{name} revision {rev} already accepted"),
        (_, Some(rev)) if rev != "1" => format!("Deployment{name} updated (revision {rev})"),
        (_, Some(rev)) => format!("Deployment{name} created (revision {rev})"),
        (Some(true), None) => format!("Deployment{name} already accepted"),
        (_, None) => format!("Deployment{name} created"),
    };
    let digest = match value["checkpoint_digest"].as_str() {
        Some("pending") => Some("being measured".to_owned()),
        other => other.map(str::to_owned),
    };
    Detail::new(summary)
        .row("Deployment ID", s(&value["deployment_id"]))
        .row("Operation", s(&value["operation_id"]))
        .row_opt("Checkpoint digest", digest)
        .render()
}

fn lifecycle(
    action: LifecycleAction,
    deployment: &str,
    instance: Option<u32>,
    value: &Value,
    context: &Context,
) -> String {
    let (request, done) = verb(action);
    let subject = match instance {
        Some(i) => format!("instance {i} of {deployment}"),
        None => deployment.to_owned(),
    };
    let d = &value["deployment"];
    if d.is_object() {
        return state_rows(
            Detail::new(format!("{done} {subject}: {}", s(&d["observed_state"]))),
            d,
            context,
        )
        .render();
    }
    let operation = opt(&value["operation_id"]).or_else(|| opt(&value["receipt"]["operation_id"]));
    let summary = if value["joined"] == true {
        format!(
            "Joined the {} already in progress for {subject}",
            request.to_lowercase()
        )
    } else {
        format!("{request} requested for {subject}")
    };
    Detail::new(summary)
        .row_opt("Operation", operation)
        .render()
}

fn delete(deployment: &str, value: &Value) -> String {
    let done = value["deleted"] == true;
    let summary = if done {
        format!("Deleted {deployment}")
    } else {
        format!("Delete requested for {deployment}")
    };
    Detail::new(summary)
        .row_opt("Operation", opt(&value["operation_id"]))
        .render()
}

fn init(target: InitTarget, value: &Value) -> String {
    let detail = Detail::new(format!("Wrote {}", s(&value["config"])))
        .row("State directory", s(&value["state_dir"]));
    match target {
        InitTarget::Host => detail.row("Runtime directory", s(&value["runtime_dir"])),
        InitTarget::Server => detail,
    }
    .render()
}

fn invite(value: &Value) -> String {
    Detail::new(format!(
        "Invitation for {} written to {}",
        s(&value["host_name"]),
        s(&value["invitation_file"])
    ))
    .note("Keep it private; it can be used once.")
    .render()
}

fn validate(value: &Value) -> String {
    let mut text = Detail::new(format!(
        "{} is a valid {} document",
        s(&value["file"]),
        s(&value["kind"])
    ))
    .row_opt(
        "Resolved against",
        resolved_against(&value["resolved_against"]),
    )
    .render();
    text.push_str(&resource_phases(value));
    // SPEC §15.3: without a host, say what was not checked.
    if value["resolved_against"].is_null() {
        if let Some(items) = value["requires_server"]
            .as_array()
            .filter(|items| !items.is_empty())
        {
            text.push_str("\nNot checked\n");
            for item in items {
                text.push_str(&format!("  {}\n", s(item)));
            }
        }
    }
    text
}

/// The host a deployment was resolved on; a group's hosts, the head first.
fn resolved_against(value: &Value) -> Option<String> {
    match value.as_array() {
        Some(hosts) => Some(hosts.iter().map(s).collect::<Vec<_>>().join(", ")),
        None => opt(value),
    }
}

/// The five phases of a deployment's resources: resolved on a host, by
/// domain; offline, the short form's figures (the host names the domains).
fn resource_phases(value: &Value) -> String {
    const PHASES: [&str; 5] = ["cold", "ready", "parking", "parked", "wake"];
    let line = |phase: &str| -> Option<String> {
        if let Some(resolved) = value["effective"]["resources"][phase].as_object() {
            let parts: Vec<String> = resolved
                .get("allocations")?
                .as_array()?
                .iter()
                .map(|a| {
                    format!(
                        "{} {}",
                        s(&a["domain"]),
                        gib(a["bytes"].as_i64().unwrap_or(0))
                    )
                })
                .collect();
            return Some(parts.join(", "));
        }
        let short = value["resources"][phase].as_object()?;
        Some(format!(
            "gpu {}, ram {}",
            s(&short["gpu"]),
            s(&short["ram"])
        ))
    };
    let lines: Vec<String> = PHASES
        .iter()
        .filter_map(|phase| line(phase).map(|l| format!("  {phase:<9}{l}\n")))
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("\nResources\n{}", lines.concat())
}

fn published(value: &Value) -> String {
    match value["published"].as_str() {
        Some("published") => "yes".into(),
        Some("role_not_running") => "when capyctl starts".into(),
        Some(other) => other.replace('_', " "),
        None => String::new(),
    }
}

fn engines_file(value: &Value) -> String {
    match opt(&value["revision"]) {
        Some(rev) => format!("{} (revision {rev})", s(&value["engines_file"])),
        None => s(&value["engines_file"]),
    }
}

fn engine_add(value: &Value) -> String {
    Detail::new(format!(
        "Registered {} ({} {})",
        s(&value["profile"]),
        s(&value["engine"]),
        s(&value["version"])
    ))
    .row("Executable", s(&value["executable"]))
    .row("Deep park", s(&value["deep_park"]))
    .row("CUDA", s(&value["cuda_home"]))
    .row("Engines file", engines_file(value))
    .row("Published", published(value))
    .render()
}

fn engine_remove(value: &Value) -> String {
    Detail::new(format!("Removed {}", s(&value["removed"])))
        .row("Engines file", engines_file(value))
        .row("Published", published(value))
        .render()
}

fn drain(host: Option<&str>, value: &Value) -> String {
    let host = opt(&value["host"])
        .or(host.map(str::to_owned))
        .unwrap_or_else(|| "this machine".into());
    // Without --wait the result lists `operations`; with it, the settled `deployments`.
    let stops = value["operations"]
        .as_array()
        .or(value["deployments"].as_array())
        .map_or(0, Vec::len);
    let stop_state = opt(&value["stops"])
        .or_else(|| (value["drained"] == true && stops > 0).then(|| "verified".to_owned()));
    let summary = if value["drained"] == true {
        format!("Drained {host}")
    } else {
        format!("Drain requested for {host}")
    };
    Detail::new(summary)
        .row_opt("Host state", opt(&value["host_state"]))
        .row_opt(
            "Stops",
            stop_state.map(|state| format!("{state} ({stops})")),
        )
        .render()
}

fn revoke(host: &str, value: &Value) -> String {
    let name = opt(&value["name"]).unwrap_or_else(|| host.to_owned());
    Detail::new(format!("Revoked {name}"))
        .row("Host ID", s(&value["host_id"]))
        .row("Engines", s(&value["engines"]))
        .note(format!(
            "To bring it back: capyctl invite host {name} --recover --output FILE, then capyctl join host --join-file FILE --recover on the host."
        ))
        .render()
}

fn prune(value: &Value) -> String {
    let removed = value["removed"].as_array().cloned().unwrap_or_default();
    let bytes = value["removed_bytes"].as_i64().unwrap_or(0);
    let applied = value["applied"] == true;
    let copies = if removed.len() == 1 { "copy" } else { "copies" };
    let summary = match (removed.is_empty(), applied) {
        (true, _) => "Nothing to remove".to_owned(),
        (false, true) => format!(
            "Removed {} unused model {copies} ({})",
            removed.len(),
            gib(bytes)
        ),
        (false, false) => format!(
            "Would remove {} unused model {copies} ({}); run again with --apply to remove {}",
            removed.len(),
            gib(bytes),
            if removed.len() == 1 { "it" } else { "them" }
        ),
    };
    let mut out = Detail::new(summary)
        .row("Model store", s(&value["model_store"]))
        .render();
    if !removed.is_empty() {
        let rows: Vec<(String, String)> = removed
            .iter()
            .map(|r| {
                (
                    s(&r["key"]),
                    r["bytes"].as_i64().map(gib).unwrap_or_default(),
                )
            })
            .collect();
        let width = rows
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        out.push('\n');
        for (key, size) in rows {
            out.push_str(&format!(
                "  {key}{}   {size}\n",
                " ".repeat(width - key.chars().count())
            ));
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::grammar::{InitTarget, LifecycleAction};
    use serde_json::json;
    use std::path::PathBuf;

    fn ctx(name: Option<&str>) -> (HostNames, Option<String>) {
        let mut names = HostNames::new();
        names.insert("h1".into(), "gpu-box".into());
        (names, name.map(str::to_owned))
    }

    fn run(command: Command, value: serde_json::Value, name: Option<&str>) -> String {
        let (names, deployment_name) = ctx(name);
        render(
            &command,
            &value,
            &Context {
                names: &names,
                deployment_name,
            },
        )
    }

    fn deploy(wait: bool) -> Command {
        Command::Deploy {
            file: Some(PathBuf::from("my-model.yaml")),
            activate: true,
            wait,
            revision: None,
            hf_endpoint: None,
            engine_env: vec![],
        }
    }

    fn deployment() -> serde_json::Value {
        json!({
            "name": "my-model", "observed_state": "ready", "revision": "1",
            "ready_instances": 1, "desired_instances": 1,
            "startup": {"bytes": 18_467_520_512i64}, "context": {"tokens": 26752},
            "latest_operation": {"kind": "initialize", "state": "succeeded"},
            "instances": [{"host_id": "h1", "index": 0}]
        })
    }

    // T02 (ADR 0021)
    #[test]
    fn deploy_without_wait_names_the_deployment_and_operation() {
        let receipt = json!({"api_version": "1", "deployment_id": "01D", "joined": false,
            "operation_id": "01OP", "revision": "1", "checkpoint_digest": "pending",
            "notice": "the checkpoint digest of my-model is being measured"});
        assert_eq!(
            run(deploy(false), receipt, Some("my-model")),
            "Deployment my-model created (revision 1)\n\n  Deployment ID       01D\n  Operation           01OP\n  Checkpoint digest   being measured\n"
        );
    }

    // T02 (ADR 0021)
    #[test]
    fn deploy_with_wait_prints_the_state_not_the_record() {
        let value = json!({"deployment": deployment(), "receipt": {"operation_id": "01OP", "revision": "1"}});
        assert_eq!(
            run(deploy(true), value, Some("my-model")),
            "Deployed my-model: ready\n\n  Revision    1\n  Hosts       gpu-box\n  Ready       1/1\n  Startup     17.2 GiB\n  Context     26752 tokens\n  Operation   initialize succeeded\n"
        );
    }

    // T02 (ADR 0021): asynchronous lifecycle requests, joined or new.
    #[test]
    fn lifecycle_requests_and_joins() {
        let park = Command::Lifecycle {
            action: LifecycleAction::Park,
            deployment: "my-model".into(),
        };
        assert_eq!(
            run(
                park.clone(),
                json!({"operation_id": "01OP", "joined": false}),
                None
            ),
            "Park requested for my-model\n\n  Operation   01OP\n"
        );
        assert_eq!(
            run(park, json!({"operation_id": "01OP", "joined": true}), None),
            "Joined the park already in progress for my-model\n\n  Operation   01OP\n"
        );
    }

    // T02 (ADR 0021): with --wait the result holds the deployment.
    #[test]
    fn lifecycle_with_wait_reports_the_outcome() {
        let stop = Command::InstanceLifecycle {
            action: LifecycleAction::Stop,
            deployment: "my-model".into(),
            instance: 0,
        };
        let mut d = deployment();
        d["observed_state"] = json!("stopped");
        assert_eq!(
            run(stop, json!({"deployment": d, "receipt": {"operation_id": "01OP"}}), None),
            "Stopped instance 0 of my-model: stopped\n\n  Ready       1/1\n  Hosts       gpu-box\n  Operation   initialize succeeded\n"
        );
    }

    // T02 T04 (ADR 0021)
    #[test]
    fn enrolment_commands() {
        assert_eq!(
            run(
                Command::Init(InitTarget::Host),
                json!({"config": "host.yaml", "initialized": true, "state_dir": "/s", "runtime_dir": "/s/runtime"}),
                None
            ),
            "Wrote host.yaml\n\n  State directory     /s\n  Runtime directory   /s/runtime\n"
        );
        assert_eq!(
            run(Command::Invite { name: "gpu-box".into(), recover: false }, json!({"host_name": "gpu-box", "invitation_file": "gpu-box.join"}), None),
            "Invitation for gpu-box written to gpu-box.join\n\nKeep it private; it can be used once.\n"
        );
        assert_eq!(
            run(
                Command::Join {
                    join_file: "gpu-box.join".into(),
                    recover: false
                },
                json!({"enrolled": true, "host_id": "01H"}),
                None
            ),
            "Joined the server\n\n  Host ID   01H\n"
        );
    }

    // T02 (ADR 0018, ADR 0021)
    #[test]
    fn engine_add_and_remove() {
        let add = Command::EngineAdd {
            path: None,
            name: None,
            deep_park: None,
            drift: crate::grammar::DriftChoice::default(),
            args: vec![],
            approved_options: vec![],
            approved_paths: vec![],
            env: vec![],
            approved_env: vec![],
        };
        let value = json!({"profile": "vllm", "engine": "vllm", "version": "0.29.0", "executable": "/v/bin/vllm",
            "deep_park": "enabled", "cuda_home": "/usr/local/cuda", "engines_file": "/c/engines.yaml",
            "revision": 1, "published": "role_not_running"});
        assert_eq!(
            run(add, value, None),
            "Registered vllm (vllm 0.29.0)\n\n  Executable     /v/bin/vllm\n  Deep park      enabled\n  CUDA           /usr/local/cuda\n  Engines file   /c/engines.yaml (revision 1)\n  Published      when capyctl starts\n"
        );
        let remove = Command::EngineRemove {
            name: "vllm".into(),
            drain: false,
        };
        assert_eq!(
            run(
                remove,
                json!({"engines_file": "/c/engines.yaml", "published": "published", "removed": "vllm", "revision": 3}),
                None
            ),
            "Removed vllm\n\n  Engines file   /c/engines.yaml (revision 3)\n  Published      yes\n"
        );
    }

    // Review focus 3: absent fields print nothing and never panic.
    #[test]
    fn absent_fields_are_left_out() {
        let park = Command::Lifecycle {
            action: LifecycleAction::Start,
            deployment: "m".into(),
        };
        assert_eq!(run(park, json!({}), None), "Start requested for m\n");
        assert_eq!(run(deploy(false), json!({}), None), "Deployment created\n");
    }

    // T02 (ADR 0021): inspect keeps every field; no text output starts with `{`.
    #[test]
    fn inspect_and_fallback_never_print_json() {
        let inspect = Command::Inspect {
            resource: crate::grammar::Resource::Deployment,
            id: Some("my-model".into()),
            effective: false,
        };
        let text = run(inspect, deployment(), None);
        assert!(text.starts_with("Deployment my-model\n\n"), "{text}");
        assert!(
            text.lines()
                .any(|l| l.trim_start().starts_with("Observed State") && l.ends_with(" ready")),
            "{text}"
        );
        let validate = Command::Validate {
            file: "d.yaml".into(),
            hosts: vec![],
            sets: vec![],
        };
        assert_eq!(
            run(
                validate,
                json!({"file": "d.yaml", "kind": "deployment", "valid": true, "resolved_against": null}),
                None
            ),
            "d.yaml is a valid deployment document\n"
        );
    }

    // T02 (ADR 0021): the match in `render` is exhaustive, so a new command
    // cannot fall through unnoticed; every command renders text, never JSON.
    #[test]
    fn every_command_renders_text() {
        use crate::grammar::{ListResource, Role};
        let all = vec![
            Command::Start(Role::Server),
            Command::Init(InitTarget::Server),
            Command::Invite {
                name: "h".into(),
                recover: false,
            },
            Command::Join {
                join_file: "j".into(),
                recover: false,
            },
            Command::List {
                resource: ListResource::Hosts,
            },
            Command::Inspect {
                resource: crate::grammar::Resource::Host,
                id: None,
                effective: false,
            },
            Command::Doctor { host: "h".into() },
            deploy(false),
            Command::Status {
                deployment: "d".into(),
                watch: false,
            },
            Command::Lifecycle {
                action: LifecycleAction::Park,
                deployment: "d".into(),
            },
            Command::InstanceLifecycle {
                action: LifecycleAction::Park,
                deployment: "d".into(),
                instance: 0,
            },
            Command::Delete {
                deployment: "d".into(),
                stop: true,
            },
            Command::Validate {
                file: "f".into(),
                hosts: vec![],
                sets: vec![],
            },
            Command::ConfigShow {
                role: None,
                sets: vec![],
            },
            Command::Drain {
                host: None,
                wait: false,
            },
            Command::Revoke { host: "h".into() },
            Command::PruneSources {
                host_config: "h.yaml".into(),
                apply: false,
                referenced_file: None,
            },
            Command::EngineDetect { paths: vec![] },
            Command::EngineAdd {
                path: None,
                name: None,
                deep_park: None,
                drift: crate::grammar::DriftChoice::default(),
                args: vec![],
                approved_options: vec![],
                approved_paths: vec![],
                env: vec![],
                approved_env: vec![],
            },
            Command::EngineList,
            Command::EngineRemove {
                name: "v".into(),
                drain: false,
            },
        ];
        for command in all {
            let name = format!("{command:?}");
            let text = run(command, json!({}), None);
            assert!(!text.starts_with('{'), "{name}: {text}");
        }
    }

    // T02 (ADR 0021): review fixes, joined without a revision and a nameless record.
    #[test]
    fn deploy_edge_cases() {
        assert_eq!(
            run(deploy(false), json!({"joined": true}), Some("my-model")),
            "Deployment my-model already accepted\n"
        );
        let mut d = deployment();
        d["name"] = json!(null);
        let text = run(deploy(true), json!({"deployment": d}), Some("my-model"));
        assert!(text.starts_with("Deployed my-model: ready\n"), "{text}");
    }

    // T02 (ADR 0021): the results `delete` and `drain --wait` really return.
    #[test]
    fn delete_and_settled_drain_shapes() {
        let delete = || Command::Delete {
            deployment: "my-model".into(),
            stop: true,
        };
        assert_eq!(
            run(
                delete(),
                json!({"operation_id": "01OP", "deployment_id": "01D", "revision": "1", "joined": false, "deleted": true}),
                None
            ),
            "Deleted my-model\n\n  Operation   01OP\n"
        );
        assert_eq!(
            run(
                delete(),
                json!({"deleted": false, "cleanup": "pending", "deployment_id": "01D", "operations": []}),
                None
            ),
            "Delete requested for my-model\n"
        );
        let drain = Command::Drain {
            host: None,
            wait: true,
        };
        assert_eq!(
            run(
                drain,
                json!({"host": "gpu-box", "host_state": "online", "drained": true, "deployments": [{"operation_id": "01OP", "state": "succeeded"}], "refused": []}),
                None
            ),
            "Drained gpu-box\n\n  Host state   online\n  Stops        verified (1)\n"
        );
    }

    // T02 (ADR 0021): drain, revoke and prune summaries.
    #[test]
    fn host_maintenance_commands() {
        assert_eq!(
            run(Command::Revoke { host: "gpu-box".into() }, json!({"host_id": "01H", "name": "gpu-box", "revoked": true, "newly_revoked": true, "engines": "retained"}), None),
            "Revoked gpu-box\n\n  Host ID   01H\n  Engines   retained\n\nTo bring it back: capyctl invite host gpu-box --recover --output FILE, then capyctl join host --join-file FILE --recover on the host.\n"
        );
        assert_eq!(
            run(
                Command::Drain {
                    host: Some("gpu-box".into()),
                    wait: false
                },
                json!({"host": "gpu-box", "host_state": "offline", "drained": false, "stops": "pending", "operations": [{"deployment_id": "01D", "instance": 0, "operation_id": "01OP"}]}),
                None
            ),
            "Drain requested for gpu-box\n\n  Host state   offline\n  Stops        pending (1)\n"
        );
        assert_eq!(
            run(Command::PruneSources { host_config: "h.yaml".into(), apply: false, referenced_file: None }, json!({"model_store": "/m", "applied": false, "removed": [{"key": "sources/hf/a", "bytes": 1073741824}], "removed_bytes": 1073741824, "kept": [], "skipped": []}), None),
            "Would remove 1 unused model copy (1.0 GiB); run again with --apply to remove it\n\n  Model store   /m\n\n  sources/hf/a   1.0 GiB\n"
        );
    }
}
