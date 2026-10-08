use super::*;
use std::os::unix::fs::PermissionsExt;

const KEY: &str = "0f3c9a5e7b1d2c4f6a8e0b2d4f6a8c0e1f3a5b7c9d0e2f4a6b8c0d1e3f5a7b9c";

fn log_in(dir: &tempfile::TempDir, max: u64, keep: u32) -> (PathBuf, RotatingLog) {
    let path = dir.path().join("launch.log");
    let log = RotatingLog::open(path.clone(), max, keep).unwrap();
    (path, log)
}

// T21: lines are redacted as written; a carriage return ends a line too.
#[test]
fn relayed_lines_are_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut log) = log_in(&dir, MAX_FILE_BYTES, KEEP_ROTATED);
    let mut redactor = LogRedactor::new();
    redactor.own("observer-cred-9");
    let input =
        format!("server_args api_key='{KEY}'\rprogress 50%\nobserver-cred-9 joined\npartial");
    relay(input.as_bytes(), &redactor, &mut log);
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        "server_args api_key='<redacted>'\rprogress 50%\n<redacted> joined\npartial\n"
    );
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

// T21: a secret is never split by the read size: lines are assembled first.
#[test]
fn a_secret_split_across_reads_is_still_redacted() {
    struct Trickle<'a>(&'a [u8]);
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(3).min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let (path, mut log) = log_in(&dir, MAX_FILE_BYTES, KEEP_ROTATED);
    let mut redactor = LogRedactor::new();
    redactor.own("abcdefgh-owned");
    let input = b"key abcdefgh-owned here\n";
    relay(Trickle(input), &redactor, &mut log);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "key <redacted> here\n"
    );
}

// An overlong run without a break is cut at a space or replaced whole.
#[test]
fn overlong_output_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut log) = log_in(&dir, MAX_FILE_BYTES, KEEP_ROTATED);
    let blob = "x".repeat(MAX_LINE_BYTES + 10);
    relay(blob.as_bytes(), &LogRedactor::new(), &mut log);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "<redacted>\n");
}

// The log is bounded: it rotates to `.1`, `.2` and drops anything older.
#[test]
fn the_log_rotates_and_stays_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (path, mut log) = log_in(&dir, 100, 2);
    let mut input = String::new();
    for n in 0..100 {
        input.push_str(&format!("line {n:03} of output\n"));
    }
    relay(input.as_bytes(), &LogRedactor::new(), &mut log);
    let current = std::fs::read_to_string(&path).unwrap();
    assert!(current.ends_with("line 099 of output\n"), "{current}");
    for file in [path.clone(), rotated(&path, 1), rotated(&path, 2)] {
        let meta = std::fs::metadata(&file).unwrap();
        assert!(meta.len() <= 100, "{}", file.display());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
    assert!(!rotated(&path, 3).exists());
    assert!(std::fs::read_to_string(rotated(&path, 1))
        .unwrap()
        .starts_with("line 09"));
}
