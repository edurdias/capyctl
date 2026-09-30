//! Standalone serves the host and engine inventory of its one embedded host
//! (`list hosts`, `list engines`, `inspect host`) at the same routes and in
//! the same JSON shapes a server does, so the client and its tables are the
//! same on every role.
//!
//! Fake engine, CPU only: never qualification of a native recipe.

mod support;

use capyctl_cli::table::{render, HostNames, View};

use support::safe_state_dir;

/// The command as the binary runs it: parsed, then sent to the local role.
async fn run(args: &[&str], root: &std::path::Path) -> serde_json::Value {
    let invocation = capyctl_cli::grammar::parse_invocation(
        std::iter::once("capyctl").chain(args.iter().copied()),
    )
    .unwrap();
    capyctl_cli::remote_roles::execute(&invocation, root)
        .await
        .unwrap_or_else(|failure| panic!("{}: {}", failure.code, failure.message))
}

// T07, T01: `list hosts`, `list engines` and `inspect host` answer on a
// standalone role, where they used to fail with `Route not found`.
#[tokio::test]
async fn standalone_lists_its_host_and_engines() {
    let dir = safe_state_dir();
    let app = support::boot(dir.path()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    // Only this test in this binary reads the management address.
    std::env::set_var(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, address.to_string());

    let hosts = run(&["list", "hosts"], dir.path()).await;
    assert_eq!(hosts["api_version"], "1", "{hosts}");
    let rows = hosts["hosts"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{hosts}");
    let host = &rows[0];
    assert_eq!(host["revoked"], false);
    assert_eq!(host["online"], true);
    assert_eq!(host["eligible"], true);
    assert_eq!(host["compatibility"], "supported");
    assert_eq!(host["binary_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(host["development_controls"]["state"], "not_exposed");
    let profiles = app.profiles();
    assert!(!profiles.is_empty());
    let published: Vec<&str> = host["session"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(published, profiles);
    assert!(!host["session"]["domains"].as_array().unwrap().is_empty());

    let name = host["name"].as_str().unwrap();
    let inspected = run(&["inspect", "host", name], dir.path()).await;
    assert_eq!(inspected["host_id"], host["host_id"]);

    let table = render(View::Hosts, &hosts, &HostNames::new());
    let header = table.lines().next().unwrap();
    assert!(header.starts_with("NAME"), "{table}");
    assert!(table.lines().nth(1).unwrap().contains("online"), "{table}");

    let engines = run(&["list", "engines"], dir.path()).await;
    let rows = engines["engines"].as_array().unwrap();
    assert_eq!(rows.len(), profiles.len(), "{engines}");
    for row in rows {
        assert_eq!(row["host_id"], host["host_id"]);
        assert_eq!(row["host"], host["name"]);
        assert_eq!(row["online"], true);
        assert_eq!(row["published"], "published");
        assert_eq!(row["retiring"], false);
        assert!(row["deployments"].as_array().unwrap().is_empty());
        assert!(profiles.contains(&row["profile"].as_str().unwrap().to_owned()));
    }
    let table = render(View::Engines, &engines, &HostNames::new());
    assert!(table.lines().next().unwrap().starts_with("HOST"), "{table}");
    assert_eq!(table.lines().count(), 1 + profiles.len(), "{table}");
    server.abort();
}
