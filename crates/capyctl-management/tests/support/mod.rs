//! ADR 0040 §2: a role serves its management routers only through
//! `compose`, so these tests do too, and reach them as the CLI on the box
//! does: from loopback, with the admin token.
use axum::{extract::connect_info::MockConnectInfo, Router};
use capyctl_management::{
    compose::{compose, ManagementPeer},
    ui_sessions::SessionVerifier,
    ManagementCredentials,
};

/// `router` composed under `credentials`, as a loopback connection sees it.
pub fn composed(credentials: ManagementCredentials, router: Router) -> Router {
    let verifier =
        SessionVerifier::open(credentials, capyctl_store::Store::open_in_memory().unwrap())
            .unwrap();
    compose(verifier, None, [router]).layer(MockConnectInfo(ManagementPeer {
        peer: "127.0.0.1:50000".parse().unwrap(),
        local: Some("127.0.0.1:7443".parse().unwrap()),
    }))
}
