# Status: ADR 0040: a web UI built into the role, with browser sessions and off-box management — 2026-10-28 (branch `feat/web-ui`)

ADR 0040 (owner decisions of 2026-10-27) adds a CapyCTL-branded web UI to the server and
standalone roles, served under `/ui/` on the management listener and acting only through the
management API. It amends ADR 0019 §10 and SPEC §16.5 (an opt-in off-box management
listener: `remote: true`, `--management-remote`, `CAPYCTL_MANAGEMENT_REMOTE`, with
`public_address` for sign-in links and the extra accepted origin), SPEC §13.3 and ADR 0031 §8
(custom engines confirmed from the browser), ADR 0017 (host administration carried from the
server to a host behind the `host_admin` capability) and ADR 0008 (deployment pinning and a
fit check on the server). The owner decision of 2026-11-13 redid the screens on a design
review (ADR 0040, plan U20): a status line, host memory segmented per model, model rows with
their live readings and one primary action per state with the rest in a menu, a Why panel
for a failed model, state colours apart from the brand orange, a phone tab bar, and engines
as a table with dialogs. What changed: one composition point authorizes every management
route for the admin token or a browser session; the admin token is accepted only from
loopback or the connection's own local address and never with forwarding headers; sessions
and one-time sign-in links live in the store by SHA-256 digest, with the `capyctl_session`
cookie and the `X-CapyCTL-UI` and `Origin` request checks for plain HTTP; `capyctl open ui`
prints a 10-minute sign-in link and a QR code; revoking a session ends its event streams,
playground forwards and host-admin waits; rotating the admin token by restart ends every
session and link; supported-action hints, a deployment check endpoint, host administration
on standalone and server roles, and a playground route authorized by a session; the Svelte
app in `web/`, embedded with `rust-embed`, built only by the release workflow, with
`release.sh` refusing a missing or stale `web/dist/`. Tests land with each unit: the
conformance tests ADR 0040 §Verification names, including the all-routes authorization
test, the session store and rotation sweep, the cookie and request checks, and the web
app's unit tests. CPU and Fake-engine tests are not qualification. The live
check still needed: on the lab hosts, a server with a host against at least one native
engine, signed in from a phone over a non-loopback address, before a release ships the UI.

U8 mounts host-scoped engine detection, built-in registration and removal, custom-engine
preview and confirmation, model-copy listing and explicit pruning, and effective host
configuration. Standalone uses the shared agent operations and one in-process control
handler under the same mutation lock as the CLI socket. Servers carry these operations
over the chosen host's correlated control session, gated by `host_admin`; they never
administer the server's own machine. Adds, confirmations and removal wait up to
180 seconds, other operations up to 30 seconds. Offline hosts refuse promptly with
`observation_stale`; lost sessions answer `outcome_unknown`, without automatic retry.
Removal answers 202 after publication; concurrent engine reads show `retiring` until
the host publishes its removal. Custom confirmation
binds the host, name, manifest, settings, measured installation and change revision.
On standalone, effective host configuration describes the synthesized running host
document rather than a nonexistent host.yaml; on servers it describes the stored
approved document, available without contacting the host. Server deployment checks pin
and size through the chosen host using that host's model-source endpoint and token.

The milestone review of 2026-11-13 (a security review and a code review of the whole
branch) found no critical security issue and four blockers, fixed before merge. Failing
before, passing after:
- The Add and Edit screens read a nested `fit` the check never sends; the web tests now use the
  handler's real response shape.
- Edit loaded the frozen effective deployment. It now loads the accepted source document from
  `GET /management/v1/deployments/{id}/source`.
- The fit check ignored observed free memory; it now applies start's admission.
- A packaged host could not write `engines.yaml` (ADR 0018 amendment A4, below).

The security fixes:
- Unknown sign-in links no longer take the store's write lock, and exchanges are bounded.
- Session write-through is serialized and safe to cancel.
- Revocation ends host-admin waits and blocked event reads without cancelling a dispatched
  operation.
- An engine found only under extra directories needs the preview and typed confirmation.
- The admin token is refused beside any forwarding or proxy header.
- Host control requests use one capability-checked session and refuse an oversized frame
  without dropping the session.
- Unknown non-management paths on the management listener answer 404.

ADR 0018 amendment A4 (owner decision 2026-11-13) moves the engines files into the role's
state directory (`capyctl_config::registration::engines_path`), which the role, the CLI and
host administration all resolve. A role's start brings an existing file over once: it is moved
when the old location is writable and copied when it is read-only, as under the system units. The
packaged-layout test `packaged_layout_host_admin_add_remove_with_read_only_config_directory`
fails before and passes after.

# Release note: New features

- **A web UI.** A standalone role and a server now serve a web UI at `/ui/` on their
  management address: the Overview shows each host's memory split by the models holding
  it, and each model's state, reserved memory and live readings (time to first token,
  tokens per second per stream, requests in flight and waiting, KV cache fill); start,
  park, wake, stop and delete models, with a **Why?** panel when one fails; add and edit
  deployments with a memory fit check; manage engines, hosts, invitations and model copies;
  and chat with or test a model in the playground. On a phone the sections sit in a tab
  bar. Sign a browser in with `capyctl open ui`, which prints a one-time link and a
  QR code ([The web UI](../guide/web-ui.md)). The management listener stays on loopback
  unless you set `remote: true` (`--management-remote`, `CAPYCTL_MANAGEMENT_REMOTE`); from
  another machine only a signed-in browser is accepted, and `public_address` names the
  address for sign-in links, for example behind `tailscale serve`
  ([network access](../operations/network-access.md#web-ui-off-the-machine)). The store
  moves to a new schema version, so an older binary refuses it; revoke all sessions after
  restoring a state backup ([data retention](../operations/data-retention.md#web-ui-sessions-and-sign-in-links)).
- **Registered engines now live in the role's state directory** (for example
  `/var/lib/capyctl/host/engines.yaml`), so an installed service can add and remove engines
  from the web UI. On its first start after the upgrade, a role brings an existing
  `engines.yaml` (and `custom-engines.yaml`) over by itself. Under the system units the old file
  in `/etc/capyctl` is left in place and no longer read; you may delete it
  ([install](../operations/install.md)).
