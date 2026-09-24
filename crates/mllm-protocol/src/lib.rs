pub mod execution;
pub mod reports;
pub mod pb {
    #![allow(clippy::result_large_err)]
    // Generated message layouts: the member result (with WE3's checkpoint
    // evidence) is the largest session frame by design and is sent rarely.
    #![allow(clippy::large_enum_variant)]
    tonic::include_proto!("mllm.management.v1");
}

/// SPEC §13.1: the agent-control session protocol version. A peer that
/// connects with any other version is refused before it can report or be sent
/// a command. Version 2 added Park and Restore member actions, load reports and
/// member exit reports; a version 1 peer cannot interpret them, so it is
/// refused rather than silently misreading evidence.
pub const PROTOCOL_VERSION: &str = "2";

/// SPEC §13.1: the encoding version carried in every command identity. The
/// typed command encoding only grew additively (new actions on new field
/// numbers), so it stays at "1". Keeping it stable keeps every journaled
/// command digest verifiable after an agent upgrade, which is what lets a
/// restarted agent keep ownership of retained launches (SPEC §13.2).
pub const COMMAND_ENCODING_VERSION: &str = "1";

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

/// SPEC §13.1: whether a connecting peer speaks this session protocol.
pub fn compatible_peer(protocol_version: &str) -> bool {
    protocol_version == PROTOCOL_VERSION
}
