pub mod pb {
    tonic::include_proto!("mllm.management.v1");
}

pub const PROTOCOL_VERSION: &str = "1";

pub const SKEW_TOLERANCE_MS: i64 = 30_000;

pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as i64
}

pub fn deadline_ok(deadline_unix_ms: i64, now: i64, tolerance_ms: i64) -> bool {
    now <= deadline_unix_ms + tolerance_ms
}
