# Status: A changed hostname is reported clearly — 2026-10-21 (branch `fix/hostname-changed`)

SPEC §15.2 ("existing identity missing or mismatched: fail for recovery"). A
standalone role publishes its host under the machine's hostname (the device
inventory's `host_id`; the fixed name `standalone` when it observes no
inventory), and its store records that name as the embedded host's namespace.
Started on existing state under another hostname, as every new container is
unless `docker run --hostname` fixes it, the role failed late, when it
published its policy, with the opaque `internal: deploy: blocked: resource
policy refused: resource policy revision conflict` (found in the discrete-GPU
container run, `docs/container-qualification-discrete`).

The role now compares the hostname with the stored embedded host name right
after it derives its host policy, before the secrets key, the owned state, the
rendezvous sweep or any policy is written, and refuses with the new code
`host_name_changed`, exit 2 (invalid configuration, which the packaged units do
not restart). The message names the stored name, the current hostname and the
fix: `docker run --hostname <stored>`, or setting the machine's hostname back.
No setting names the host instead of the hostname, so none is offered. Only
the embedded host's record is read: the enrolled hosts a server store also
holds are never compared, and the host role, which uses its enrolled name, is
unchanged. A start without a device inventory, which publishes `standalone`
rather than the hostname, is not compared here and keeps the earlier refusal.

Code: `Store::embedded_host_name` (`capyctl-store`), `roles::check_host_name`
and `StartError::HostNameChanged` (`capyctl-cli`), the code and its exit in
`output.rs`. Tests (T06): the store returns no name on fresh state and the
published one after an import, unchanged by an enrolled host; a booted
standalone proceeds under its own name and under no hostname-derived name,
fresh state proceeds under any name, and another name is refused
`host_name_changed`, exit 2, naming both names and `docker run --hostname`;
the errors page documents the code with its exit. Docs: the errors guide, the
exit-code table and the container section of the install guide.

CPU and Fake-engine tests are not qualification. Still to check live: a
standalone container recreated without `--hostname` on existing state refuses
with `host_name_changed`, and recreated with the stored hostname starts.

# Release note: Fixes

- **A changed hostname is reported clearly.** A standalone role started on
  existing state under another hostname, as a new container gets unless
  `docker run --hostname` is given, used to fail with an opaque `resource
  policy revision conflict`. It now refuses with `host_name_changed` (exit 2),
  naming the stored and the current name and the fix: `docker run --hostname
  <stored name>`. See
  [Running in a container](../operations/install.md#running-in-a-container).
