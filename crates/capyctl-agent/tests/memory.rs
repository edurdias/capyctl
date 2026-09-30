use capyctl_agent::memory::parse_meminfo;

#[test]
fn available_is_not_total() {
    let sample = parse_meminfo(
        "MemTotal: 128 kB\nMemAvailable: 40 kB\nSwapTotal: 8 kB\nSwapFree: 6 kB\n",
        100,
    )
    .unwrap();
    assert_eq!(sample.memory.domain, "system");
    assert_eq!(sample.memory.capacity_bytes, 128 * 1024);
    assert_eq!(sample.memory.available_bytes, 40 * 1024);
    assert_eq!(sample.memory.sampled_at_ms, 100);
    assert_eq!(sample.swap_used_bytes, 2 * 1024);
}

#[test]
fn bad_samples_fail_closed() {
    let valid = "MemTotal: 128 kB\nMemAvailable: 40 kB\nSwapTotal: 8 kB\nSwapFree: 6 kB\n";
    for invalid in [
        valid.replace("MemAvailable: 40 kB\n", ""),
        valid.replace("MemAvailable: 40", "MemAvailable: 129"),
        valid.replace("MemTotal: 128", "MemTotal: 0"),
        valid.replace("MemAvailable: 40", "MemAvailable: -1"),
        valid.replace("MemAvailable: 40 kB", "MemAvailable: 40 MB"),
        valid.replace("MemTotal: 128", "MemTotal: 9223372036854775807"),
        valid.replace("SwapFree: 6", "SwapFree: 9"),
        format!("{valid}MemTotal: 128 kB\n"),
        "x".repeat(65_537),
    ] {
        assert!(
            parse_meminfo(&invalid, 100).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(parse_meminfo(valid, -1).is_err());
}
