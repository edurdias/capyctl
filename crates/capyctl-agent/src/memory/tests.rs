use super::*;

const GIB: i64 = 1 << 30;

/// A `/proc/meminfo` reading of `total` with `available` free, in GiB.
fn meminfo(total: i64, available: i64) -> HostMemorySample {
    parse_meminfo(
        &format!(
            "MemTotal: {} kB\nMemAvailable: {} kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
            total * GIB / 1024,
            available * GIB / 1024
        ),
        100,
    )
    .unwrap()
}

/// A fixture hierarchy: each entry is a cgroup path and its files.
fn hierarchy(cgroups: &[(&str, &[(&str, String)])]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (cgroup, files) in cgroups {
        let dir = root.path().join(cgroup.trim_start_matches('/'));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in *files {
            std::fs::write(dir.join(name), content).unwrap();
        }
    }
    root
}

fn limited(max: &str, current: i64, inactive_file: i64) -> Vec<(&'static str, String)> {
    vec![
        ("memory.max", format!("{max}\n")),
        ("memory.current", format!("{current}\n")),
        (
            "memory.stat",
            format!("anon 1\nfile 2\ninactive_file {inactive_file}\nactive_file 3\n"),
        ),
    ]
}

fn figures(sample: &HostMemorySample) -> (i64, i64) {
    (sample.memory.capacity_bytes, sample.memory.available_bytes)
}

// T26: `/proc/self/cgroup` names the unified cgroup on its `0::` line, and a v1
// memory controller on a line of its own.
#[test]
fn proc_self_cgroup_is_parsed() {
    assert_eq!(
        parse_proc_cgroup("0::/system.slice/capyctl.service\n").unwrap(),
        CgroupMembership {
            unified: Some("/system.slice/capyctl.service".into()),
            v1_memory: false,
        }
    );
    let hybrid = "12:cpu,cpuacct:/docker/abc\n4:memory:/docker/abc\n0::/docker/abc\n";
    assert_eq!(
        parse_proc_cgroup(hybrid).unwrap(),
        CgroupMembership {
            unified: Some("/docker/abc".into()),
            v1_memory: true,
        }
    );
    assert!(parse_proc_cgroup("not a cgroup line\n").is_err());
    assert!(parse_proc_cgroup("0::/a\n0::/b\n").is_err());
}

// T26: `max` is no limit; a number is one; anything else is refused.
#[test]
fn memory_max_is_max_or_a_number() {
    assert_eq!(parse_memory_max("max\n").unwrap(), None);
    assert_eq!(parse_memory_max("8589934592\n").unwrap(), Some(8 * GIB));
    for invalid in ["", "-1", "8G", "max max"] {
        assert!(parse_memory_max(invalid).is_err(), "{invalid:?}");
    }
    assert_eq!(
        parse_inactive_file("anon 5\ninactive_file 42\n").unwrap(),
        Some(42)
    );
    assert_eq!(parse_inactive_file("anon 5\n").unwrap(), None);
}

// T26: `memory.max` set to `max` all the way up bounds nothing.
#[test]
fn an_unlimited_cgroup_reads_meminfo() {
    let root = hierarchy(&[("/", &[]), ("/app", &limited("max", 3 * GIB, 0))]);
    let sample = bound_by_cgroup(meminfo(128, 100), Some("0::/app\n"), root.path()).unwrap();
    assert_eq!(figures(&sample), (128 * GIB, 100 * GIB));
    assert_eq!(sample.source, MemorySource::Meminfo);
    assert_eq!(sample.source.token(), "meminfo");
}

// T26: a numeric limit on the process's own cgroup bounds the capacity, and the
// availability is what it can still charge, its inactive page cache credited.
#[test]
fn a_numeric_limit_bounds_capacity_and_availability() {
    let root = hierarchy(&[("/app", &limited(&(32 * GIB).to_string(), 20 * GIB, 4 * GIB))]);
    let sample = bound_by_cgroup(meminfo(128, 100), Some("0::/app\n"), root.path()).unwrap();
    assert_eq!(figures(&sample), (32 * GIB, 16 * GIB));
    assert_eq!(
        sample.source,
        MemorySource::CgroupV2 {
            cgroup: "/app".into()
        }
    );
    assert_eq!(sample.source.token(), "cgroup_v2:/app");
    // Host availability below the cgroup's headroom still wins.
    let tight = bound_by_cgroup(meminfo(128, 10), Some("0::/app\n"), root.path()).unwrap();
    assert_eq!(figures(&tight), (32 * GIB, 10 * GIB));
}

// T26: every ancestor's limit applies, so a tighter parent bounds the reading
// even when the process's own cgroup is unlimited or looser.
#[test]
fn a_tighter_parent_bounds_the_reading() {
    let root = hierarchy(&[
        ("/", &[]),
        (
            "/machine.slice",
            &limited(&(24 * GIB).to_string(), 6 * GIB, 0),
        ),
        (
            "/machine.slice/capyctl",
            &limited(&(64 * GIB).to_string(), 5 * GIB, 0),
        ),
        ("/machine.slice/capyctl/role", &limited("max", 5 * GIB, 0)),
    ]);
    let sample = bound_by_cgroup(
        meminfo(128, 100),
        Some("0::/machine.slice/capyctl/role\n"),
        root.path(),
    )
    .unwrap();
    assert_eq!(figures(&sample), (24 * GIB, 18 * GIB));
    assert_eq!(
        sample.source,
        MemorySource::CgroupV2 {
            cgroup: "/machine.slice".into()
        }
    );
}

// T26: a limit above the host's memory bounds neither figure, and availability
// never exceeds capacity.
#[test]
fn a_limit_above_the_host_reads_meminfo() {
    let root = hierarchy(&[("/app", &limited(&(256 * GIB).to_string(), GIB, 0))]);
    let sample = bound_by_cgroup(meminfo(128, 100), Some("0::/app\n"), root.path()).unwrap();
    assert_eq!(figures(&sample), (128 * GIB, 100 * GIB));
    assert_eq!(sample.source, MemorySource::Meminfo);
    let small = hierarchy(&[("/app", &limited(&(8 * GIB).to_string(), 0, 0))]);
    let sample = bound_by_cgroup(meminfo(128, 100), Some("0::/app\n"), small.path()).unwrap();
    assert_eq!(figures(&sample), (8 * GIB, 8 * GIB));
}

// T26: cgroup v1 limits are not read, and the reading says so; a kernel
// without cgroups reads meminfo.
#[test]
fn cgroup_v1_is_not_read_and_says_so() {
    let root = hierarchy(&[("/docker/abc", &limited(&GIB.to_string(), 0, 0))]);
    let v1 = "11:memory:/docker/abc\n3:cpu,cpuacct:/docker/abc\n";
    let sample = bound_by_cgroup(meminfo(128, 100), Some(v1), root.path()).unwrap();
    assert_eq!(figures(&sample), (128 * GIB, 100 * GIB));
    assert_eq!(sample.source, MemorySource::CgroupV1Unread);
    assert_eq!(sample.source.token(), "meminfo:cgroup_v1_unread");
    let none = bound_by_cgroup(meminfo(128, 100), None, root.path()).unwrap();
    assert_eq!(none.source, MemorySource::Meminfo);
}

// T26: a v2 cgroup whose limits cannot be read is reported, not assumed
// unlimited in silence.
#[test]
fn an_unreadable_cgroup_is_reported() {
    let root = hierarchy(&[("/app", &[("memory.max", "lots\n".into())])]);
    for membership in [
        "0::/missing\n",
        "0::/../outside\n",
        "0::/app\n",
        "garbage\n",
    ] {
        let sample = bound_by_cgroup(meminfo(128, 100), Some(membership), root.path()).unwrap();
        assert_eq!(figures(&sample), (128 * GIB, 100 * GIB), "{membership}");
        assert_eq!(
            sample.source,
            MemorySource::CgroupV2Unreadable,
            "{membership}"
        );
    }
}

// T26: a zero limit is no usable reading.
#[test]
fn a_zero_limit_is_refused() {
    let root = hierarchy(&[("/app", &limited("0", 0, 0))]);
    assert!(bound_by_cgroup(meminfo(128, 100), Some("0::/app\n"), root.path()).is_err());
}

// T26: the live reading on this machine is consistent whatever bounds it.
#[test]
fn the_live_reading_names_its_source() {
    let sample = read_host_memory().unwrap();
    assert!(sample.memory.capacity_bytes > 0);
    assert!(sample.memory.available_bytes <= sample.memory.capacity_bytes);
    assert!(!sample.source.token().is_empty());
}

#[test]
fn a_pinned_reading_states_capacity_and_availability() {
    let sample = pinned_host_memory(&format!("{}:{}", 32_i64 << 30, 8_i64 << 30), 7).unwrap();
    assert_eq!(sample.memory.capacity_bytes, 32 << 30);
    assert_eq!(sample.memory.available_bytes, 8 << 30);
    assert_eq!(sample.memory.sampled_at_ms, 7);
    assert_eq!(sample.swap_used_bytes, 0);
}

#[test]
fn a_malformed_pinned_reading_is_invalid() {
    for pinned in [
        "",
        "1024",
        "1024:",
        "x:1024",
        "1024:2048",
        "1000:1000",
        "-1024:0",
    ] {
        assert!(
            pinned_host_memory(pinned, 7).is_err(),
            "accepted {pinned:?}"
        );
    }
}
