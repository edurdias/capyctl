//! Owner decision 2026-09-25: commands that read records print an aligned,
//! human-readable table by default (terminal or not), like the docker CLI.
//! `--format json` prints the command's JSON result unchanged. Only the
//! rendering lives here; every view is built from that same JSON result, so
//! a table can never show something the JSON does not carry. Wide or nested
//! detail (latency distributions, fingerprints, development-control marks)
//! stays in the JSON.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::grammar::{Command, ListResource};

/// Host id to host name, from the server's host inventory.
pub type HostNames = BTreeMap<String, String>;

/// The record views that print as a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// `list hosts`
    Hosts,
    /// `list deployments`
    Deployments,
    /// `list engines`: every host's published runtime profiles.
    Engines,
    /// `status deployment`
    Status,
    /// `engine list`: this machine's runtime profiles.
    LocalEngines,
    /// `engine detect`
    Detected,
}

impl View {
    /// The table view of a command, or `None` when it prints its JSON result
    /// (mutations, `inspect`, `validate`, `prune`, `drain`, `revoke`).
    pub fn of(command: &Command) -> Option<Self> {
        match command {
            Command::List { resource } => Some(match resource {
                ListResource::Hosts => View::Hosts,
                ListResource::Deployments => View::Deployments,
                ListResource::Engines => View::Engines,
            }),
            Command::Status { .. } => Some(View::Status),
            Command::EngineList => Some(View::LocalEngines),
            Command::EngineDetect { .. } => Some(View::Detected),
            _ => None,
        }
    }

    /// Views whose records name hosts by id only; the caller resolves names
    /// from the host inventory (best effort) before rendering.
    pub fn needs_host_names(self) -> bool {
        matches!(self, View::Deployments | View::Status)
    }
}

/// The host names in a `GET /management/v1/hosts` answer.
pub fn host_names(inventory: &Value) -> HostNames {
    inventory["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|host| {
            let id = host["host_id"].as_str()?;
            let name = host["name"].as_str().filter(|name| !name.is_empty())?;
            Some((id.to_owned(), name.to_owned()))
        })
        .collect()
}

/// Render `value` (the command's JSON result) as the view's table.
pub fn render(view: View, value: &Value, names: &HostNames) -> String {
    match view {
        View::Hosts => hosts(value),
        View::Deployments => deployments(value, names),
        View::Engines => engines(value),
        View::Status => status(value, names),
        View::LocalEngines => local_engines(value),
        View::Detected => detected(value),
    }
}

/// Columns left-aligned, separated by three spaces, upper-case headers, no
/// trailing blanks. An empty result prints the header line only.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(i) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    let mut out = String::new();
    let mut line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut text = String::new();
        for (i, cell) in cells.enumerate() {
            if i > 0 {
                text.push_str("   ");
            }
            text.push_str(cell);
            let pad = widths[i].saturating_sub(cell.chars().count());
            text.push_str(&" ".repeat(pad));
        }
        out.push_str(text.trim_end());
        out.push('\n');
    };
    line(&mut headers.iter().copied());
    for row in rows {
        line(&mut row.iter().map(String::as_str).take(headers.len()));
    }
    out
}

/// Bytes in GiB with one decimal (`12.5 GiB`).
pub fn gib(bytes: i64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// Milliseconds as seconds (`600s`, `1.5s`).
pub fn seconds(ms: i64) -> String {
    if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// A host by name; its id when the inventory has no name for it.
pub fn host_label(id: &str, names: &HostNames) -> String {
    names.get(id).cloned().unwrap_or_else(|| id.to_owned())
}

/// One line of text: control characters (a multi-line error) become spaces
/// so they cannot break the alignment.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A scalar cell: strings as they are, numbers as written, `-` for absent.
fn text(value: &Value) -> String {
    match value {
        Value::Null => "-".into(),
        Value::String(s) if s.is_empty() => "-".into(),
        Value::String(s) => clean(s),
        Value::Bool(b) => yes_no(*b),
        Value::Array(items) => list(items),
        other => clean(&other.to_string()),
    }
}

fn yes_no(flag: bool) -> String {
    if flag { "yes" } else { "no" }.into()
}

/// A list cell: names joined with commas, `-` when empty.
fn list(items: &[Value]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| match item {
            Value::String(s) => clean(s),
            Value::Object(o) => o
                .get("name")
                .or_else(|| o.get("deployment"))
                .or_else(|| o.get("id"))
                .and_then(Value::as_str)
                .map(clean)
                .unwrap_or_else(|| clean(&item.to_string())),
            other => clean(&other.to_string()),
        })
        .collect();
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(",")
    }
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

/// SPEC §4.1 / ADR 0017: a host's standing, most severe first.
fn host_state(host: &Value) -> &'static str {
    let session = &host["session"];
    if host["revoked"] == true {
        "revoked"
    } else if host["online"] != true {
        "offline"
    } else if session["unresponsive"] == true {
        "unresponsive"
    } else if session["drain_pending"] == true {
        "draining"
    } else if session["reconciled"] == false {
        "reconciling"
    } else {
        "online"
    }
}

fn hosts(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["hosts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|host| {
            let id = host["host_id"].as_str().unwrap_or("-");
            let name = host["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .unwrap_or(id);
            let domains = host["session"]["domains"].as_array();
            let memory = match domains.filter(|d| !d.is_empty()) {
                Some(domains) => {
                    let sum = |key: &str| domains.iter().filter_map(|d| number(&d[key])).sum();
                    format!(
                        "{} / {}",
                        gib(sum("available_bytes")),
                        gib(sum("capacity_bytes"))
                    )
                }
                None => "-".into(),
            };
            let engines = host["session"]["profiles"]
                .as_array()
                .map(|p| list(p))
                .unwrap_or_else(|| "-".into());
            vec![
                clean(name),
                host_state(host).into(),
                yes_no(host["eligible"] == true),
                text(&host["binary_version"]),
                text(&host["compatibility"]),
                memory,
                engines,
            ]
        })
        .collect();
    table(
        &[
            "NAME",
            "STATE",
            "ELIGIBLE",
            "VERSION",
            "COMPATIBILITY",
            "MEMORY (FREE / TOTAL)",
            "ENGINES",
        ],
        &rows,
    )
}

/// The distinct hosts a deployment's instances are placed on, by name.
fn instance_hosts(deployment: &Value, names: &HostNames) -> String {
    let mut seen: Vec<String> = Vec::new();
    for instance in deployment["instances"].as_array().into_iter().flatten() {
        if let Some(id) = instance["host_id"].as_str() {
            let label = host_label(id, names);
            if !seen.contains(&label) {
                seen.push(label);
            }
        }
    }
    if seen.is_empty() {
        "-".into()
    } else {
        seen.join(",")
    }
}

fn ready(deployment: &Value) -> String {
    format!(
        "{}/{}",
        number(&deployment["ready_instances"]).unwrap_or(0),
        number(&deployment["desired_instances"]).unwrap_or(0)
    )
}

fn deployments(value: &Value, names: &HostNames) -> String {
    let rows: Vec<Vec<String>> = value
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            vec![
                text(&d["name"]),
                text(&d["kind"]),
                text(&d["desired_state"]),
                text(&d["observed_state"]),
                ready(d),
                text(&d["revision"]),
                instance_hosts(d, names),
            ]
        })
        .collect();
    table(
        &[
            "NAME", "KIND", "DESIRED", "STATE", "READY", "REVISION", "HOSTS",
        ],
        &rows,
    )
}

fn operation(op: &Value) -> String {
    if !op.is_object() {
        return "-".into();
    }
    let mut out = format!("{} {}", text(&op["kind"]), text(&op["state"]));
    if let Some(code) = op["error_code"].as_str() {
        out.push_str(&format!(" ({})", clean(code)));
    }
    out
}

/// An instance's LAST ERROR: the error it recorded (a placement refusal),
/// else, when its latest operation failed, that failure's code and the first
/// line of its message.
fn last_error(instance: &Value) -> String {
    if let Some(error) = instance["last_error"].as_str().filter(|e| !e.is_empty()) {
        return clean(error);
    }
    let op = &instance["latest_operation"];
    if op["state"] != "failed" {
        return "-".into();
    }
    let message = op["reason"].as_str().and_then(|r| r.lines().next());
    match (op["error_code"].as_str(), message) {
        (Some(code), Some(message)) => clean(&format!("{code}: {message}")),
        (Some(code), None) => clean(code),
        (None, Some(message)) => clean(message),
        (None, None) => "failed".into(),
    }
}

fn status(value: &Value, names: &HostNames) -> String {
    let d = value;
    let startup = number(&d["startup"]["bytes"])
        .map(gib)
        .unwrap_or_else(|| "-".into());
    let initialize = number(&d["timeouts"]["initialize_ms"])
        .map(seconds)
        .unwrap_or_else(|| "-".into());
    let mut out = table(
        &[
            "NAME",
            "KIND",
            "DESIRED",
            "STATE",
            "READY",
            "REVISION",
            "STARTUP",
            "INITIALIZE",
            "LAST OPERATION",
        ],
        &[vec![
            text(&d["name"]),
            text(&d["kind"]),
            text(&d["desired_state"]),
            text(&d["observed_state"]),
            ready(d),
            text(&d["revision"]),
            startup,
            initialize,
            operation(&d["latest_operation"]),
        ]],
    );
    let instances: Vec<Vec<String>> = d["instances"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            let host = i["host_id"]
                .as_str()
                .map(|id| host_label(id, names))
                .unwrap_or_else(|| "-".into());
            let devices = match &i["devices"] {
                Value::Object(o) if o.is_empty() => "-".into(),
                Value::Object(o) => clean(&Value::Object(o.clone()).to_string()),
                other => text(other),
            };
            vec![
                text(&i["index"]),
                host,
                text(&i["observed_state"]),
                text(&i["lifecycle"]),
                devices,
                last_error(i),
            ]
        })
        .collect();
    out.push('\n');
    out.push_str(&table(
        &[
            "INSTANCE",
            "HOST",
            "STATE",
            "LIFECYCLE",
            "DEVICES",
            "LAST ERROR",
        ],
        &instances,
    ));
    out
}

fn engines(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["engines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let host = e["host"]
                .as_str()
                .filter(|n| !n.is_empty())
                .or_else(|| e["host_id"].as_str())
                .unwrap_or("-");
            let state = if e["retiring"] == true {
                "retiring"
            } else if e["online"] == true {
                "online"
            } else {
                "offline"
            };
            vec![
                clean(host),
                text(&e["profile"]),
                text(&e["engine"]),
                text(&e["version"]),
                text(&e["custom"]),
                text(&e["deep_park"]),
                state.into(),
                text(&e["deployments"]),
            ]
        })
        .collect();
    table(
        &[
            "HOST",
            "PROFILE",
            "ENGINE",
            "VERSION",
            "CUSTOM",
            "DEEP PARK",
            "STATE",
            "DEPLOYMENTS",
        ],
        &rows,
    )
}

fn local_engines(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["engines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            vec![
                text(&e["profile"]),
                text(&e["source"]),
                text(&e["engine"]),
                text(&e["version"]),
                text(&e["custom"]),
                text(&e["deep_park"]),
                text(&e["published"]),
                text(&e["deployments"]),
            ]
        })
        .collect();
    table(
        &[
            "PROFILE",
            "SOURCE",
            "ENGINE",
            "VERSION",
            "CUSTOM",
            "DEEP PARK",
            "PUBLISHED",
            "DEPLOYMENTS",
        ],
        &rows,
    )
}

fn detected(value: &Value) -> String {
    let rows: Vec<Vec<String>> = value["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            vec![
                text(&c["engine"]),
                text(&c["version"]),
                text(&c["custom"]),
                text(&c["env"]),
                text(&c["source"]),
            ]
        })
        .collect();
    table(
        &["ENGINE", "VERSION", "CUSTOM", "ENVIRONMENT", "SOURCE"],
        &rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names() -> HostNames {
        host_names(&json!({"hosts": [
            {"host_id": "01HOSTA", "name": "gpu-a"},
            {"host_id": "01HOSTB", "name": ""},
        ]}))
    }

    #[test]
    fn columns_align_and_lines_carry_no_trailing_blanks() {
        let out = table(
            &["NAME", "STATE"],
            &[
                vec!["a-long-name".into(), "ready".into()],
                vec!["b".into(), "stopped".into()],
            ],
        );
        assert_eq!(
            out,
            "NAME          STATE\na-long-name   ready\nb             stopped\n"
        );
    }

    #[test]
    fn an_empty_result_prints_the_headers_only() {
        assert_eq!(
            render(View::Deployments, &json!([]), &HostNames::new()),
            "NAME   KIND   DESIRED   STATE   READY   REVISION   HOSTS\n"
        );
        let hosts = render(View::Hosts, &json!({"hosts": []}), &HostNames::new());
        assert_eq!(hosts.lines().count(), 1);
        assert!(hosts.starts_with("NAME   STATE   ELIGIBLE"));
        let detected = render(
            View::Detected,
            &json!({"candidates": []}),
            &HostNames::new(),
        );
        assert_eq!(
            detected,
            "ENGINE   VERSION   CUSTOM   ENVIRONMENT   SOURCE\n"
        );
    }

    #[test]
    fn host_ids_resolve_to_names_and_fall_back_to_the_id() {
        let names = names();
        assert_eq!(host_label("01HOSTA", &names), "gpu-a");
        // An empty name is no name.
        assert_eq!(host_label("01HOSTB", &names), "01HOSTB");
        assert_eq!(host_label("01HOSTC", &names), "01HOSTC");
        let out = render(
            View::Deployments,
            &json!([{"name": "chat", "kind": "model", "desired_state": "running",
                "observed_state": "ready", "ready_instances": 2, "desired_instances": 2,
                "revision": "3", "instances": [
                    {"index": 0, "host_id": "01HOSTA"},
                    {"index": 1, "host_id": "01HOSTC"},
                    {"index": 2, "host_id": "01HOSTA"}]}]),
            &names,
        );
        let row = out.lines().nth(1).unwrap();
        assert!(row.starts_with("chat   model"), "{out}");
        assert!(row.ends_with("gpu-a,01HOSTC"), "{out}");
        assert!(row.contains("2/2"), "{out}");
        assert!(!out.contains("01HOSTA"), "{out}");
    }

    #[test]
    fn units_are_human() {
        assert_eq!(gib(0), "0.0 GiB");
        assert_eq!(gib(3 * (1 << 30) / 2), "1.5 GiB");
        assert_eq!(seconds(600_000), "600s");
        assert_eq!(seconds(1_500), "1.5s");
        let out = render(
            View::Hosts,
            &json!({"hosts": [{"host_id": "01HOSTA", "name": "gpu-a", "revoked": false,
                "online": true, "eligible": true, "binary_version": "0.1.0",
                "compatibility": "supported",
                "session": {"reconciled": true, "domains": [
                    {"available_bytes": 64_i64 << 30, "capacity_bytes": 128_i64 << 30}],
                    "profiles": [{"name": "vllm"}, {"name": "sglang"}]}}]}),
            &HostNames::new(),
        );
        assert!(out.contains("64.0 GiB / 128.0 GiB"), "{out}");
        assert!(out.contains("vllm,sglang"), "{out}");
        assert!(out
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("gpu-a   online   yes"));
    }

    #[test]
    fn host_states_are_named() {
        let row = |host: Value| {
            render(View::Hosts, &json!({"hosts": [host]}), &HostNames::new())
                .lines()
                .nth(1)
                .unwrap()
                .to_owned()
        };
        assert!(
            row(json!({"host_id": "01X", "name": null, "revoked": true}))
                .starts_with("01X    revoked")
        );
        assert!(row(json!({"host_id": "01X", "online": false})).contains("offline"));
        assert!(row(json!({"host_id": "01X", "online": true,
            "session": {"drain_pending": true}}))
        .contains("draining"));
    }

    #[test]
    fn status_prints_the_deployment_then_its_instances() {
        let out = render(
            View::Status,
            &json!({"name": "chat", "kind": "model", "desired_state": "running",
                "observed_state": "failed", "ready_instances": 0, "desired_instances": 1,
                "revision": "1", "startup": {"bytes": 20_i64 << 30},
                "timeouts": {"initialize_ms": 900_000},
                "latest_operation": {"kind": "start", "state": "failed", "error_code": "activation_timeout"},
                "latency": {"tiers": []},
                "instances": [{"index": 0, "host_id": "01HOSTA", "observed_state": "failed",
                    "lifecycle": "stopped", "devices": ["GPU-0"],
                    "last_error": "engine exited\nwith code 1"}]}),
            &names(),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].starts_with("NAME   KIND    DESIRED   STATE"),
            "{out}"
        );
        assert!(lines[1].contains("20.0 GiB"), "{out}");
        assert!(lines[1].contains("900s"), "{out}");
        assert!(
            lines[1].ends_with("start failed (activation_timeout)"),
            "{out}"
        );
        assert_eq!(lines[2], "");
        assert!(lines[3].starts_with("INSTANCE   HOST"), "{out}");
        assert!(lines[4].contains("gpu-a"), "{out}");
        assert!(lines[4].ends_with("engine exited with code 1"), "{out}");
        assert!(!out.contains("tiers"), "latency stays in the JSON");
    }

    // T16: an instance whose launch failed shows the failure's code and
    // message in LAST ERROR (the shape `status deployment` returns after a
    // failed launch), not `-`.
    #[test]
    fn a_failed_launch_is_the_instance_last_error() {
        let failed = json!({"error_code": "launch_failed",
            "hint": "the engine exited before it was ready; check the deployment's engine_config",
            "kind": "initialize", "state": "failed",
            "reason": "launch failed: engine launch failed: the engine exited before readiness\nexit 1"});
        let out = render(
            View::Status,
            &json!({"name": "toy", "kind": "model", "desired_state": "ready",
                "observed_state": "failed", "ready_instances": 0, "desired_instances": 1,
                "revision": "1", "latest_operation": failed,
                "instances": [{"index": 0, "host_id": "01HOSTA", "observed_state": "failed",
                    "lifecycle": "active", "devices": [{"id": "gpu0", "sharing": "shared"}],
                    "last_error": null, "latest_operation": failed},
                    {"index": 1, "host_id": "01HOSTA", "observed_state": "ready",
                    "lifecycle": "active", "devices": [],
                    "latest_operation": {"kind": "initialize", "state": "succeeded"}}]}),
            &names(),
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[4].ends_with(
                "launch_failed: launch failed: engine launch failed: the engine exited before readiness"
            ),
            "{out}"
        );
        assert!(lines[5].ends_with('-'), "{out}");
    }

    #[test]
    fn engine_views_name_hosts_and_flags() {
        let out = render(
            View::Engines,
            &json!({"engines": [{"host_id": "01HOSTA", "host": "gpu-a", "online": true,
                "profile": "vllm", "engine": "vllm", "version": "0.11.0", "custom": false,
                "deep_park": "enabled", "retiring": false, "deployments": ["chat"]}]}),
            &HostNames::new(),
        );
        assert_eq!(
            out.lines().nth(1).unwrap(),
            "gpu-a   vllm      vllm     0.11.0    no       enabled     online   chat"
        );
        let local = render(
            View::LocalEngines,
            &json!({"engines": [{"profile": "local-vllm", "source": "environment",
                "engine": "vllm", "version": "0.11.0", "custom": true,
                "deep_park": null, "published": "published", "deployments": []}]}),
            &HostNames::new(),
        );
        assert!(local
            .lines()
            .nth(1)
            .unwrap()
            .starts_with("local-vllm   environment   vllm"));
        assert!(local
            .lines()
            .nth(1)
            .unwrap()
            .ends_with("yes      -           published   -"));
    }
}
