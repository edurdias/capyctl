use mllm_protocol::pb::agent_control_client::AgentControlClient;
use mllm_protocol::pb::agent_control_server::{AgentControl, AgentControlServer};
use mllm_protocol::pb::{agent_to_server, server_to_agent, AgentToServer, Connect, Envelope, LaunchMember, ServerToAgent};
use mllm_protocol::{deadline_ok, now_unix_ms, PROTOCOL_VERSION, SKEW_TOLERANCE_MS};
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::StreamExt;
use tonic::transport::{Endpoint, Server};
use tonic::{Request, Response, Status, Streaming};

struct Echo;

#[tonic::async_trait]
impl AgentControl for Echo {
    type SessionStream = ReceiverStream<Result<ServerToAgent, Status>>;

    async fn session(
        &self,
        request: Request<Streaming<AgentToServer>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty stream"))?;
        let operation_id = match &first.msg {
            Some(agent_to_server::Msg::Connect(c)) => c
                .envelope
                .as_ref()
                .map(|e| e.operation_id.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let launch = LaunchMember {
                profile_name: "fake-engine".into(),
                profile_fingerprint: String::new(),
                plan: None,
                rendered_command_json: String::new(),
                role: "worker".into(),
                member_id: "member-1".into(),
                envelope: Some(Envelope {
                    operation_id,
                    protocol_version: PROTOCOL_VERSION.into(),
                    ..Default::default()
                }),
            };
            let _ = tx
                .send(Ok(ServerToAgent {
                    msg: Some(server_to_agent::Msg::LaunchMember(launch)),
                }))
                .await;
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn spawn_server(ready: oneshot::Sender<SocketAddr>) {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _ = ready.send(addr);
    Server::builder()
        .add_service(AgentControlServer::new(Echo))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await
        .unwrap();
}

#[tokio::test]
async fn agent_control_roundtrip_over_real_channel() {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(spawn_server(tx));
    let addr = rx.await.unwrap();
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client = AgentControlClient::new(channel);

    let (req_tx, req_rx) = mpsc::channel::<AgentToServer>(8);
    req_tx
        .send(AgentToServer {
            msg: Some(agent_to_server::Msg::Connect(Connect {
                host_id: "host-1".into(),
                protocol_version: PROTOCOL_VERSION.into(),
                journal_resume_token: Vec::new(),
                envelope: Some(Envelope {
                    host_id: "host-1".into(),
                    operation_id: "op-1".into(),
                    protocol_version: PROTOCOL_VERSION.into(),
                    ..Default::default()
                }),
            })),
        })
        .await
        .unwrap();
    drop(req_tx);

    let request_stream = ReceiverStream::new(req_rx);
    let mut stream = client.session(request_stream).await.unwrap().into_inner();
    let resp = stream.message().await.unwrap().unwrap();

    let launch = match resp.msg {
        Some(server_to_agent::Msg::LaunchMember(l)) => l,
        other => panic!("expected LaunchMember echo, got {other:?}"),
    };
    let envelope = launch.envelope.expect("envelope wired");
    assert_eq!(envelope.operation_id, "op-1");
    assert_eq!(envelope.protocol_version, "1");
}

#[test]
fn protocol_version_is_pinned() {
    assert_eq!(PROTOCOL_VERSION, "1");
}

#[test]
fn deadline_enforced_with_skew_tolerance() {
    let now = now_unix_ms();
    assert!(deadline_ok(now - 10_000, now, SKEW_TOLERANCE_MS));
    assert!(!deadline_ok(now - 45_000, now, SKEW_TOLERANCE_MS));
}
