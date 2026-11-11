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

/// SPEC §10, ADR 0036 §4: an HTTP/1 peer whose idle timer wins before its
/// protocol receives a reused request. The socket stays open after the first
/// complete answer. Only when another upload reaches its kernel buffer does
/// the peer close it, without parsing that request. A fresh connection works.
/// This forces the ordering without relying on timer/scheduler sleeps.
pub struct ExpiredKeepaliveEngine {
    address: std::net::SocketAddr,
    accepted: Arc<AtomicUsize>,
    parsed: Arc<AtomicUsize>,
    resets: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ExpiredKeepaliveEngine {
    pub async fn start(answer: &'static str) -> Self {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(16 * 1024).unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = socket.listen(16).unwrap().into_std().unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let parsed = Arc::new(AtomicUsize::new(0));
        let resets = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (counted, read, reset, stopped) = (
            accepted.clone(),
            parsed.clone(),
            resets.clone(),
            stop.clone(),
        );
        let thread = std::thread::spawn(move || {
            let (mut first, _) = loop {
                if stopped.load(Ordering::SeqCst) {
                    return;
                }
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("keepalive peer accept: {error}"),
                }
            };
            counted.fetch_add(1, Ordering::SeqCst);
            read_request(&mut first);
            read.fetch_add(1, Ordering::SeqCst);
            write_answer(&mut first, answer);
            first.set_nonblocking(true).unwrap();
            let mut idle = Some(first);
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut fresh, _)) => {
                        counted.fetch_add(1, Ordering::SeqCst);
                        read_request(&mut fresh);
                        read.fetch_add(1, Ordering::SeqCst);
                        write_answer(&mut fresh, answer);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("keepalive peer accept: {error}"),
                }
                if let Some(connection) = &idle {
                    match connection.peek(&mut [0]) {
                        Ok(0) => idle = None,
                        Ok(_) => {
                            // Bytes are present but the application has not
                            // received them. Closing with unread data resets
                            // the upload, as an expired idle timer can do.
                            reset.fetch_add(1, Ordering::SeqCst);
                            idle = None;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => idle = None,
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            address,
            accepted,
            parsed,
            resets,
            stop,
            thread: Some(thread),
        }
    }

    pub fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    /// TCP connections accepted, fully parsed requests, and reused uploads reset.
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.accepted.load(Ordering::SeqCst),
            self.parsed.load(Ordering::SeqCst),
            self.resets.load(Ordering::SeqCst),
        )
    }
}

impl Drop for ExpiredKeepaliveEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request(socket: &mut std::net::TcpStream) {
    use std::io::Read;

    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
        assert!(head.len() < 8192);
    }
    let head = std::str::from_utf8(&head).unwrap();
    let mut remaining = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, size)| size.trim().parse::<usize>().unwrap());
    let mut buffer = [0; 4096];
    while remaining != 0 {
        let take = remaining.min(buffer.len());
        socket.read_exact(&mut buffer[..take]).unwrap();
        remaining -= take;
    }
}

fn write_answer(socket: &mut std::net::TcpStream, answer: &str) {
    use std::io::Write;

    write!(
        socket,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{answer}",
        answer.len()
    )
    .unwrap();
    socket.flush().unwrap();
}
