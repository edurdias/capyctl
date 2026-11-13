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

The review fixes add regression cases for behavior that was failing before the fixes:
Edit now reads the accepted source document and numeric revision rather than the frozen
effective configuration; fit previews use start's observed-memory admission, include
drafter weights, preserve Hugging Face file and credential selections, and retain offline
hosts as unknown results. Revocation ends a deployment-check wait without cancelling work
already sent to a host. Source-route authorization is exercised on both roles, and the
route table now rejects a missing route rather than accepting equal 404 responses.
Additional cases cover suspended lifecycle hints, temporary UI entry redirects,
extensionless embedded assets, canonical browser origins and management overrides applied
before document validation. These tests are CPU regression evidence, not native-engine
qualification.

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
