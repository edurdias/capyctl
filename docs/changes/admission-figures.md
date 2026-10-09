# Status: A start refused on available memory gives its figures — 2026-10-09 (branch `admission-figures`)

Found live on host A (standalone, `main` eb214aad): with `managed_limit 110GiB`
and `free_reserve 11GiB`, accepted because together they fit the 121.7 GiB
total, a deployment with a 109.25 GiB cold charge passed placement but was
refused at arm. SPEC §7.2 admits a charge only when the memory available now,
less the charge, leaves the free reserve (118.2 − 109.25 = 8.95 < 11 GiB).
`capyctl start --wait` then reported only `gave up: resource or evidence check
failed: insufficient resources` after three attempts, with no figures.

The rule is unchanged (uncertainty keeps accounting); the refusal now carries
its figures end to end:

- `ResourceError::InsufficientAvailable(AvailableShortfall)`
  (`crates/capyctl-domain/src/resources.rs`) holds the domain, the memory
  available, the candidate's charge, other starts' charges not yet resident,
  the free reserve to keep (on a device domain, the part ADR 0019 §2 does not
  absorb) and the shortfall. `admit_phase`'s free-memory check
  (`crates/capyctl-scheduler/src/residency.rs`) returns it; the managed-limit
  refusal stays `Insufficient`. It reads in capacity_blocked's wording under
  the closed code a host's own refusal uses: `insufficient_memory: needs
  109.2 GiB of system memory, 118.2 GiB available and a 11.0 GiB free reserve
  to keep, 2.1 GiB short` (`insufficient_device_memory` on a GPU). Every place
  that treated `Insufficient` from this check as a free-memory refusal (a
  park's own phase, a switch victim's parking, a grant that allocates nothing
  new) treats the new variant the same way.
- The operation's failure, its journal, `status` (`latest_operation.reason`,
  with the `insufficient_memory` hint) and the CLI carry that text.
  `start --wait` ends a start that gave up for memory with
  `insufficient_resources` (exit 4), as `capacity_blocked` does.
- When a standalone role starts, or an enrolled host publishes its policy, a
  domain whose managed limit plus free reserve exceeds the memory available
  then (plus what deployments already charged there hold) gets a warning with
  the figures in the role log and in `list hosts` (`session.memory_warnings`,
  and a `warning:` line under the table). Nothing is refused for it.
- `docs/operations/configuration.md` (standalone memory limits) explains that
  available memory, not total, must cover the charge plus the reserve;
  `docs/guide/errors.md` shows the new reason.

Tests: the scheduler's free-memory refusals now assert their figures
(`resource_admission.rs`, `resource_sequences.rs`); domain unit tests cover
the wording and when the warning appears; `admission_figures.rs` drives the
shipped CLI against an in-process standalone with 10 GiB of 32 available
(status JSON and text carry the four figures; `list hosts` warns, and does
not with all 32 GiB available); `live_profiles.rs` checks an enrolled
host's publication warns only when its limits exceed what it reports
available; `start_wait_replicas.rs` checks the `--wait` JSON error and exit 4.
CPU and Fake-engine tests only; they are not qualification. Live check still
needed: repeat the host A start and confirm the figures in `start --wait` and
the start-up warning.

# Release note: Memory

- **A start refused for memory says why, with numbers.** When a deployment
  fits the managed limit but the memory available now, less its startup
  charge, would not leave the free reserve, status and `start --wait` give the
  available memory, the charge, the reserve and the shortfall, and `--wait`
  exits with `insufficient_resources`. A role whose managed limit plus free
  reserve is more than the memory available when it starts warns about it in
  its log and in `capyctl list hosts`
  ([memory limits](../operations/configuration.md#standalone-memory-limits)).
