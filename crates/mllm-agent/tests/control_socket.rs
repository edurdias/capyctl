//! ADR 0018 §3: the local control channel. Owner-only socket, peer uid
//! checked, one bounded request and reply per connection, nothing but engine
//! add, remove and list. CPU tests; not qualification.
use mllm_agent::control_socket::*;
use serde_json::{json, Value};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Echo(Mutex<Vec<ControlRequest>>);

#[async_trait::async_trait]
impl ControlHandler for Echo {
    async fn handle(&self, request: ControlRequest) -> Value {
        self.0.lock().unwrap().push(request.clone());
        json!({"ok": true, "request": request.to_line()})
    }
}

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn own_uid() -> u32 {
    unsafe { libc::geteuid() }
}

async fn serving(
    uid: u32,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Arc<Echo>,
    tokio::sync::watch::Sender<bool>,
) {
    let dir = private_dir();
    let path = dir.path().join(SOCKET_NAME);
    let server = ControlServer::bind(&path).unwrap();
    let echo = Arc::new(Echo(Mutex::new(vec![])));
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    tokio::spawn(server.serve(echo.clone(), uid, shutdown));
    (dir, path, echo, stop)
}

// T37: the socket is mode 0600 and a request from the owner round-trips.
#[tokio::test]
async fn the_owner_is_answered_over_a_0600_socket() {
    let (_dir, path, echo, _stop) = serving(own_uid()).await;
    let meta = std::fs::symlink_metadata(&path).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let reply = request(
        &path,
        &ControlRequest::Remove {
            profile: "vllm".into(),
            drain: true,
        },
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(echo.0.lock().unwrap().len(), 1);
}

// T37: a connection from another user id is closed unanswered and the
// handler never runs.
#[tokio::test]
async fn another_user_id_is_refused() {
    let (_dir, path, echo, _stop) = serving(own_uid().wrapping_add(1)).await;
    let result = request(&path, &ControlRequest::List, Duration::from_secs(5)).await;
    assert!(result.is_err(), "{result:?}");
    assert!(echo.0.lock().unwrap().is_empty());
}

// T37: only the three operations, version 1, one bounded line.
#[tokio::test]
async fn only_bounded_known_requests_are_accepted() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_dir, path, echo, _stop) = serving(own_uid()).await;
    for line in [
        r#"{"v":1,"op":"shell","cmd":"id"}"#,
        r#"{"v":2,"op":"add"}"#,
        r#"{"v":1,"op":"remove"}"#,
        r#"{"v":1,"op":"list","extra":true}"#,
        r#"{"v":1,"op":"remove","profile":"","drain":false}"#,
        "not json",
    ] {
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).await.unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["ok"], false, "{line}");
        assert_eq!(reply["code"], "invalid_request", "{line}");
    }
    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let _ = stream.write_all(&vec![b'x'; MAX_LINE + 10]).await;
    let mut reply = String::new();
    let _ = BufReader::new(stream).read_line(&mut reply).await;
    assert!(
        reply.is_empty() || reply.contains("invalid_request"),
        "{reply}"
    );
    assert!(echo.0.lock().unwrap().is_empty());
}

// T37: the request line round-trips for every operation.
#[test]
fn request_lines_round_trip() {
    for request in [
        ControlRequest::Add,
        ControlRequest::List,
        ControlRequest::Remove {
            profile: "sglang".into(),
            drain: false,
        },
    ] {
        assert_eq!(
            ControlRequest::parse_line(&request.to_line()).unwrap(),
            request
        );
    }
}

// T33: a stale socket left by a crashed role is replaced; a live one is not;
// a regular file at the path is never removed.
#[tokio::test]
async fn a_stale_socket_is_replaced_but_a_live_one_or_a_file_is_not() {
    let (dir, path, _echo, _stop) = serving(own_uid()).await;
    assert!(matches!(
        ControlServer::bind(&path),
        Err(ControlError::InUse(_))
    ));
    let stale = dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert!(ControlServer::bind(&stale).is_ok());
    let file = dir.path().join("file.sock");
    std::fs::write(&file, "keep").unwrap();
    assert!(matches!(
        ControlServer::bind(&file),
        Err(ControlError::Occupied(_))
    ));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep");
}

// T33: a symlink at the socket path is never followed or removed, even
// when it points at a stale socket.
#[tokio::test]
async fn a_symlink_at_the_socket_path_is_left_alone() {
    let dir = private_dir();
    let target = dir.path().join("target.sock");
    drop(std::os::unix::net::UnixListener::bind(&target).unwrap());
    let link = dir.path().join(SOCKET_NAME);
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        ControlServer::bind(&link),
        Err(ControlError::Occupied(_))
    ));
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::symlink_metadata(&target)
        .unwrap()
        .file_type()
        .is_socket());
}

// T33: shutdown removes the socket this server bound, but never a file that
// replaced it at the same path.
#[tokio::test]
async fn shutdown_removes_only_our_own_socket() {
    let (_dir, path, _echo, stop) = serving(own_uid()).await;
    stop.send(true).unwrap();
    wait_until(|| std::fs::symlink_metadata(&path).is_err()).await;

    let dir = private_dir();
    let path = dir.path().join(SOCKET_NAME);
    let server = ControlServer::bind(&path).unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let echo = Arc::new(Echo(Mutex::new(vec![])));
    let served = tokio::spawn(server.serve(echo, own_uid(), shutdown));
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "someone else").unwrap();
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else");
}

async fn wait_until(mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached in time");
}

// T37 (Review Focus 3): a socket path over the sun_path limit is refused
// with its path, and the client says so instead of panicking.
#[tokio::test]
async fn an_overlong_socket_path_is_refused_cleanly() {
    let dir = private_dir();
    let deep = dir.path().join("d".repeat(120));
    std::fs::create_dir_all(&deep).unwrap();
    let path = deep.join(SOCKET_NAME);
    let refused = ControlServer::bind(&path);
    assert!(matches!(refused, Err(ControlError::PathTooLong(_))));
    let error = request(&path, &ControlRequest::List, Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ClientError::Unreachable(ref m) if m.contains(SOCKET_NAME)),
        "{error}"
    );
}

// T37: a client that connects and sends nothing is dropped after the read
// bound, and does not hold the server.
#[tokio::test]
async fn a_silent_client_does_not_hold_the_server() {
    let (_dir, path, echo, _stop) = serving(own_uid()).await;
    let _silent: Vec<_> = hold_silent_connections(&path, 4).await;
    let reply = request(&path, &ControlRequest::List, Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(reply["ok"], true);
    assert_eq!(echo.0.lock().unwrap().len(), 1);
}

async fn hold_silent_connections(
    path: &std::path::Path,
    count: usize,
) -> Vec<tokio::net::UnixStream> {
    let mut held = Vec::new();
    for _ in 0..count {
        held.push(tokio::net::UnixStream::connect(path).await.unwrap());
    }
    held
}

// T37 (ADR 0018 §3): the socket is bound only inside a directory owned by
// this user with mode 0700; anything wider is refused and nothing is created.
#[tokio::test]
async fn a_socket_outside_a_private_directory_is_refused() {
    let dir = private_dir();
    for mode in [0o755, 0o750, 0o701, 0o1700] {
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        let path = dir.path().join(SOCKET_NAME);
        let refused = ControlServer::bind(&path);
        assert!(
            matches!(refused, Err(ControlError::UnsafeDirectory(_))),
            "{mode:o}: {:?}",
            refused.as_ref().err()
        );
        assert!(std::fs::symlink_metadata(&path).is_err(), "{mode:o}");
    }
    // A symlink to a private directory is not the directory.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = dir.path().join("real");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        ControlServer::bind(&link.join(SOCKET_NAME)),
        Err(ControlError::UnsafeDirectory(_))
    ));
    // 0700 binds.
    ControlServer::bind(&target.join(SOCKET_NAME)).unwrap();
}

/// A role that takes the request line and then goes away without a reply
/// (it crashed, or was stopped mid-request); `delay` before it does.
fn accepts_then_vanishes(path: &std::path::Path, delay: Duration) -> std::thread::JoinHandle<()> {
    use std::io::BufRead;
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut line)
            .unwrap();
        std::thread::sleep(delay);
        drop(stream);
    })
}

// T37 (ADR 0018 §3; review decision I4): a request the role received but
// never answered, whether it closed the connection or the bound passed, is
// reported as unanswered, never as unreachable: its outcome is unknown.
#[tokio::test]
async fn a_request_the_role_took_but_never_answered_is_unanswered() {
    let dir = private_dir();
    let path = dir.path().join(SOCKET_NAME);
    let role = accepts_then_vanishes(&path, Duration::ZERO);
    let closed = request(&path, &ControlRequest::List, Duration::from_secs(5)).await;
    assert!(
        matches!(closed, Err(ClientError::Unanswered(_))),
        "{closed:?}"
    );
    role.join().unwrap();
    std::fs::remove_file(&path).unwrap();
    let role = accepts_then_vanishes(&path, Duration::from_millis(600));
    let late = request(&path, &ControlRequest::List, Duration::from_millis(200)).await;
    assert!(matches!(late, Err(ClientError::Unanswered(_))), "{late:?}");
    role.join().unwrap();
    // Nothing listening at all: unreachable, the request never left.
    std::fs::remove_file(&path).unwrap();
    let absent = request(&path, &ControlRequest::List, Duration::from_secs(5)).await;
    assert!(
        matches!(absent, Err(ClientError::Unreachable(_))),
        "{absent:?}"
    );
}
