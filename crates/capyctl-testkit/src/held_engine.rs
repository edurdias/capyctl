//! SPEC §10 (amended 2026-10-21): an engine double on a real TCP socket that
//! reads every request and never finishes answering it, counting the requests
//! that arrived and the connections their client then closed. A test sees
//! from it whether CapyCTL closed the engine connection of a request it
//! cancelled.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A chat stream's response head and a chunk that only opens the reply (it
/// names the assistant role and carries nothing else): not an answer.
const OPENING: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
transfer-encoding: chunked\r\n\r\n\
b1\r\ndata: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\
\"model\":\"served-name\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\
\"content\":\"\"},\"finish_reason\":null}]}\n\n\r\n";

pub struct HeldEngine {
    address: std::net::SocketAddr,
    arrived: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
}

impl HeldEngine {
    /// Listen on a loopback port and hold every request without a byte of
    /// answer.
    pub async fn start() -> Self {
        Self::serve(None).await
    }

    /// As [`HeldEngine::start`], answering every request with a chat stream's
    /// head and its opening chunk, then nothing more.
    pub async fn opening() -> Self {
        Self::serve(Some(OPENING)).await
    }

    async fn serve(opening: Option<&'static [u8]>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the held engine");
        let address = listener.local_addr().expect("held engine address");
        let arrived = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let (counted, ended) = (arrived.clone(), closed.clone());
        tokio::spawn(async move {
            while let Ok((mut connection, _)) = listener.accept().await {
                let (arrived, closed) = (counted.clone(), ended.clone());
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let mut first = true;
                    loop {
                        match connection.read(&mut buffer).await {
                            // The client closed (or reset) its connection.
                            Ok(0) | Err(_) => break,
                            Ok(_) if first => {
                                first = false;
                                arrived.fetch_add(1, Ordering::SeqCst);
                                if let Some(opening) = opening {
                                    if connection.write_all(opening).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Ok(_) => {}
                        }
                    }
                    closed.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            address,
            arrived,
            closed,
        }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    /// Connections that sent a request.
    pub fn arrived(&self) -> usize {
        self.arrived.load(Ordering::SeqCst)
    }

    /// Connections their client closed.
    pub fn closed(&self) -> usize {
        self.closed.load(Ordering::SeqCst)
    }

    /// Whether `count` requests arrived within `bound`.
    pub async fn arrived_within(&self, count: usize, bound: Duration) -> bool {
        within(bound, || self.arrived() >= count).await
    }

    /// Whether `count` connections were closed by their client within `bound`.
    pub async fn closed_within(&self, count: usize, bound: Duration) -> bool {
        within(bound, || self.closed() >= count).await
    }
}

async fn within(bound: Duration, done: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + bound;
    while !done() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    true
}
