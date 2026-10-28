# ADR 0040 — A web UI built into the role, with browser sessions and off-box management

**Status:** Accepted (owner decisions 2026-10-27). The decisions are recorded below
("Owner decisions of 2026-10-27"). Plan: `docs/plans/2026-10-27-2235-feat-web-ui-plan.md`.
The off-box access and browser custom-engine parts ship only together with this ADR.
**Amends:** `SPEC.md` §16.5 and ADR 0019 §10 (the management listener may be bound off the
box, §1); `SPEC.md` §13.3 and ADR 0031 §8 (a management API introduces programs: custom
engines confirmed from the browser, §11); `SPEC.md` §14 (browser sessions are a second
management credential, and the management API gains the routes of §3–§12); ADR 0017 §3 (the
`host_admin` capability, §10); ADR 0008 (model sources pinned by the role on the chosen
host's terms, §9).
**Related:** ADR 0018 (engine registration, `engines.yaml`), ADR 0028 §2.1 (engine
environment rules), ADR 0030 (decision models, `/v1/systemone`), SPEC §4.3 (the shutdown
admission gate), `docs/design/milestones/f2-sglang-design.md` §7 (the management API is the
single semantic boundary for the CLI, a future UI and integrations).

## Context

CapyCTL's audience is home users and prosumers running models on GPU machines they own.
Every interaction goes through the CLI on the box, and remote administration is `ssh` plus
the local CLI (ADR 0019 §10). Answering what is loaded, what is parked and how much GPU
memory is free means running `status` commands repeatedly; adding a model means hand-writing
deployment YAML and finding out at deploy time whether it fits (memory fit is enforced today
only as refusals at deploy and start, such as `insufficient_device_memory`).

The owner wants a full web UI, built into the server and standalone roles, that covers
everything a running role can do: live observability, lifecycle actions, guided model setup,
engine and host administration, and a playground for chat and decision models. It is
single-user, with the same authority as the CLI, and must be reachable from a phone over
Tailscale without being exposed by default.

The management API lacks several of the capabilities this needs: engine detect, add and
remove (including custom engines), a fit check before deploying, listing and pruning model
copies, viewing host configuration, sign-in links and sessions, and a playground path
authorized by a UI session. Event-stream credentials are fixed and cannot be revoked
(`crates/capyctl-management/src/events.rs`), and each open tab holds one event stream within
the default cap of 16.

Several of these needs reverse earlier owner decisions: the management listener stays on
loopback (ADR 0019 §10), no management API introduces a program (SPEC §13.3, ADR 0031 §8),
and only the CLI pins model sources. This ADR records the new decisions and the risks
accepted with them.

## Decision

### 1. Off-box management listener (opt-in)

The UI and the API share the management listener. The UI is served under `/ui/`, and `/`
redirects there.

The management listener stays on loopback by default. One explicit setting per role allows a
non-loopback `bind`: `remote: true`, spelled `listeners.management.remote` on a server,
`server.listeners.management.remote` in a standalone document, `--management-remote` and
`CAPYCTL_MANAGEMENT_REMOTE`. Without `remote`, the loopback check applies as before; parsing
an address is split from that check.

`public_address` is optional: an origin such as `http://box:7443` or
`https://box.tail1234.ts.net`. It names the address used in sign-in links and the extra
origin the request checks of §4 accept, which is how `tailscale serve` works.

Off-box access is plain HTTP and prints no warning; the owner's network (such as Tailscale)
provides the protection, as for the inference listener (ADR 0019 §8). An off-box bind
address that is unavailable at start fails the role's start, like any listener.

The CLI's recorded-address reader accepts the recorded off-box address. On an unspecified
bind (`0.0.0.0`, `::`) it connects through loopback.

### 2. One composition point and principals

`capyctl_management::compose` merges the role's management routers under one outer layer.
That layer resolves the principal, admin or session, and refuses unauthenticated requests,
except the session exchange and the UI's static routes. The eight existing `authenticate`
middlewares read that principal instead of checking credentials themselves, so no route can
accept only one credential by accident.

When an `Authorization` header is present it alone decides. Session lookups use an in-memory
table written through to the store, so authorization never waits on the store's read permits.

### 3. The bearer token only from loopback or the local address

The admin bearer token is accepted only from a loopback peer, or from a peer whose address is
the connection's own local address (the CLI on the box reaching a specific bind), and never
on a request carrying `X-Forwarded-For` or `Forwarded`. This keeps the admin token on the box
when `remote` is set. Off the box, the only management credential is a browser session.

### 4. Browser sessions, the cookie and the request checks

Sessions and sign-in links live in the store (schema v59), keyed by SHA-256 digests of
256-bit random tokens, following the invitation pattern, with one-shot consumption in an
`IMMEDIATE` transaction. Session ids are random ULIDs, never digests. Sessions survive
restarts; revoking one deletes its row. A session has the admin token's authority.

The cookie is `capyctl_session`: `HttpOnly`, `SameSite=Strict`, `Path=/`, no `Secure`
(plain HTTP, §1) and no `Domain`. Its `Max-Age` is 400 days, the browsers' limit, and it is
re-issued on use once a day has passed since the last refresh, so a session lasts until
revoked. Sign-out and revocation answer with `Max-Age=0`.

Request checks for a cookie-authenticated request:

- Every one, GET included, carries `X-CapyCTL-UI: 1`. A page on another origin cannot send
  it without a CORS preflight, which the role never answers; this also stops other sites
  from holding event streams open.
- A method other than GET or HEAD also carries an `Origin` equal, case-insensitively, to
  `http://` plus the request's authority, or to the `public_address` origin. If
  `Sec-Fetch-Site` is present it is `same-origin`.
- A request with a body declares `Content-Type: application/json`.

Anything else is refused `403 origin_refused`. The unauthenticated session exchange applies
the same checks. SameSite cannot tell apart two ports or two tailnet machines, so the custom
header and the exact Origin check are the CSRF defense.

### 5. Sign-in links and `capyctl open ui`

Sign-in is a two-step exchange:

1. `POST /management/v1/ui/sign-in-links` (admin principal only) returns a token valid for
   10 minutes. Links are stored in SQLite, and several may be outstanding at once.
2. `capyctl open ui [--address <origin>]` prints `<origin>/ui/sign-in#t=<token>` and a
   terminal QR code. The origin is `--address`, else `public_address`, else a specific
   bind, else loopback with a note that a phone needs the off-box setting; never a `Host`
   header.
3. The page removes the fragment from the address bar. On the owner's click it POSTs the
   token to `/management/v1/ui/sessions`, which consumes it atomically, ignores any cookie
   it is sent, and sets a fresh one. A GET never consumes a token, so link previewers and
   scanners cannot use it up.

### 6. Revocation

Session routes: `GET /management/v1/ui/session` (current), `GET /management/v1/ui/sessions`,
`POST /management/v1/ui/sessions/{id}/revoke`, `POST /management/v1/ui/sessions/revoke-all`
and `POST /management/v1/ui/sign-out`.

One session verifier per role, shared by every router, owns a revocation broadcast. Event
streams, playground forwards and host-admin waits owned by a revoked session end immediately,
not at the stream's 300-second lifetime. Revoking a session does not undo what it ran.

### 7. Rotation by restart

Rotating the admin token means replacing it in the role's credentials file and restarting
the role; no rotation command exists and none is added. Each session and each sign-in link
records a fingerprint of the admin token's digest. At role start, every session or link whose
fingerprint differs is deleted, and consuming a link checks it, so rotating the token by
restart ends every session and every outstanding link.

### 8. Supported-action hints

The snapshot gains, on each deployment and instance, `actions`: each action's name, whether
it is available, and a reason code when it is not. A pure function in `crates/capyctl-store`
returns typed reasons, which management maps to codes through the existing error mapping.
Hints cover only refusals that depend on state alone; refusals that depend on runtime
conditions (room, sources) stay with the handlers, whose validation remains authoritative.
Suspend and resume, refused as unsupported today, get no hints.

### 9. The deployment check endpoint (amends ADR 0008)

`POST /management/v1/deployments/check` takes `{"yaml": "<text>"}` or `{"config": {…}}` and
stores nothing. It parses with the same strict parser as `deploy model --file`.

**Pinning on the server, on the host's terms.** The role pins model sources itself: the
Hugging Face endpoint and token come from the chosen host's `model_sources`, not from a
shell. A standalone role pins in-process; a server pins through a `host_admin` operation on
the chosen host (§10). Resolved pins are cached per repository and reference. The CLI's
pinning code moves into `crates/capyctl-config` and is shared. A `~/` path is refused, since
the role cannot know which home it means. Until now only the CLI pinned sources; this is the
amendment to ADR 0008.

**Sizing before measurement.** The weights come from, in order: a recorded measurement of the
same pinned checkpoint; the weight-file sizes the source reports for the pinned commit (the
`is_weight` rule); or the on-disk size of a local path on the host. A verdict built on sizes
rather than a measurement is marked `estimated`.

**Fit.** Each candidate host is resolved in a read-only transaction, using a per-host
resolution factored out of the store's acceptance and placement code, against memory free
now. The response carries the normalized JSON config, the emitted YAML, errors (with a
position when the parser provides one), and a verdict: `fits`; `needs_room` (fits the host's
managed memory but not what is free now; the models to switch out are not named);
`does_not_fit`, with the limit; or `unknown`, with a reason (host offline, no observations,
or `weights_unknown`). A model that does not fit can still be added, with the existing
`POST /management/v1/deployments`.

### 10. Host administration (amends ADR 0017's capability table)

Host-local operations get host-scoped routes:

- engines: `GET …/hosts/{host}/engines/detected` (optional extra search directories, the API
  form of `engine detect --path`), `POST …/hosts/{host}/engines`,
  `POST …/hosts/{host}/engines/{name}/remove`;
- model copies: `GET …/hosts/{host}/model-store`, `POST …/hosts/{host}/model-store/prune`;
- host configuration: `GET …/hosts/{host}/config`;
- custom engines: §11.

The logic behind `capyctl engine`, `prune sources` and host effective config moves into a new
module, `capyctl_agent::host_admin`; the CLI keeps its prompts and calls it. A standalone
role calls the module in-process, with `StandaloneControl`, under the engines lock. A server
carries the operation to the host in a top-level `HostAdminRequest`/`HostAdminReply` pair
with a request id; the host runs it outside its session loop. Host configuration on a server
comes from the approved document the server already stores. Prune computes the reference set
when requested.

A host that implements this declares the capability `host_admin`, added to ADR 0017's
capability table (server to host: HostAdminRequest and its HostAdminReply).

Response times: an add answers synchronously within a deadline of at least 150 seconds. A
remove answers 202, and the UI follows the existing `retiring` flag. A lost session answers
`outcome_unknown` and is never retried automatically.

Refusal codes are the existing ones: an offline host is refused `observation_stale`, an older
host `host_capability_missing:host_admin`, and a host too old to receive the message
`host_upgrade_required`.

A built-in engine add takes a detected candidate. A named executable path or
`allow_remote_code` runs code, so it goes through §11's confirmation.

### 11. Custom engines from the browser (amends ADR 0031 §8 and SPEC §13.3)

Custom engines can be added and changed from any signed-in session. This reverses ADR 0031's
owner decision of 2026-10-20 that `engine add --custom` confirms on a terminal on the box, and
SPEC §13.3's rule that no management API introduces a program. The server enforces the
confirmation over a defined digest:

- **Preview.** `POST …/hosts/{host}/custom-engines/preview` takes the manifest content in
  the body, measures the installation, and returns the exact command, the approval summary
  the CLI shows, and a digest.
- **Digest.** A domain-separated SHA-256 over canonical JSON of: the operation, with the
  `custom-engines.yaml` revision for a change; the host id and profile name; the canonical
  executable path and the manifest digest; the arguments and the profile's environment names
  and values; the approved environment, drift policy and deep-park setting; the interpreter,
  root, idle signal, API and engine-key mode; and the measured installation digest, file
  count and bytes.
- **Add or change.** `POST …/hosts/{host}/custom-engines` carries the manifest, that digest
  and the profile name typed by the owner as the confirmation. The host re-measures; any
  difference is refused `confirmation_stale`, and the previewed installation digest is the
  one pinned (ADR 0031).
- The manifest is capped below the 64 KiB server-to-host message limit.
- The CLI keeps its terminal confirmation; the digest code is shared with it.

### 12. The playground route

`POST /management/v1/playground/chat/completions` and `POST /management/v1/playground/systemone`
accept the router's bodies and stream its response. The role builds a second instance of its
router with `RouterDeps { api_key: None, .. }`, wraps it in the same shutdown admission gate as
the inference listener (SPEC §4.3), and hands it to management. Scheduling, waking, switching
and metrics behave as for any app request, with no warning before sending; the inference key
is never read and never reaches a browser, and inference authentication for apps is unchanged.
The forward passes only the body, its content type and a request id, matches the router's
body limit, writes no prompt or response anywhere, and ends when the browser disconnects or
the session is revoked. Conversation history is kept only in the browser.

### 13. Embedded UI assets and the release build

The frontend is Svelte 5 with plain Vite and TypeScript in `web/`. Built assets are embedded
with `rust-embed` (with `allow_missing` and `deterministic-timestamps`) in a new crate,
`crates/capyctl-webui`, behind a small asset trait. The crate registers `/`, `/ui`, `/ui/` and
`/ui/{*path}` explicitly and never sets a router fallback.

Only the release workflow builds `web/` (Node 22, `npm ci`, `npm run build`, no source maps),
before `packaging/release.sh`. The build writes its commit into `web/dist/`; `release.sh`
refuses to package when `web/dist/index.html` is missing or that commit differs from HEAD, and
records the bundle's digest in BUILDINFO. `dist/` is never committed, and no `build.rs` runs
npm. Builds without assets serve a built-in page saying the build has no web UI. Third-party
notices for bundled packages are served at `/ui/licenses.txt`.

### 14. The UI's content security policy

UI responses carry
`Content-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'`,
plus `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff` and
`Referrer-Policy: same-origin`. Inline styles are allowed because CodeMirror and Svelte
transitions inject `<style>` elements; with scripts locked to `'self'` an injected style
cannot run code. `index.html` is `no-cache`; hashed assets are immutable for a year. Model
output, engine logs and YAML render as text, never as HTML, because a script injected into
the UI could add a custom engine.

## Consequences

- The management listener gains a second credential type and an opt-in off-box bind, and
  every management middleware changes to read the composed principal.
- Schema v59 adds two tables; older binaries refuse the newer store, as for every schema
  change.
- New server-to-host messages behind the `host_admin` capability, so mixed versions refuse
  rather than fail.
- The release workflow gains a Node build, and the binary grows by the UI bundle.

Accepted risks:

- **Plain HTTP off the box.** On a plain LAN, session cookies travel unencrypted. Plain HTTP
  also carries invitation secrets, prompts, engine logs and configuration.
- **Custom engines from any session.** Anyone holding a session who can reach the bind
  address can run any program as the CapyCTL user; Tailscale ACLs or the LAN are the only
  boundary. On a server, a session can run programs on every enrolled host.
- **A session is equivalent to the admin token**, and revoking it does not undo what it ran.
- **The cookie is sent to every port on its host name** (cookies do not separate ports).

## Alternatives considered

- **A second listener for the UI.** Rejected: one more port to configure, and cookies are
  shared across ports anyway.
- **Loopback-only with SSH tunnels; network-reachable by default; HTTPS-only off-box.**
  Rejected by the owner (below).
- **Built-in TLS, or a private-address-only restriction off the box.** Rejected by the owner:
  the owner's network provides the protection.
- **Pasting the admin token into the browser, or offering both.** Rejected by the owner: the
  admin token never leaves the box.
- **A separately installed companion app.** Rejected: a second install and a version-skew
  surface.
- **A UI that reaches engines or the store directly.** Rejected: it breaks SPEC §14 and the
  F2 design's "not permission to invent a separate UI control path".
- **Accepting sessions on the inference listener for the playground.** Rejected: it would
  need credentialed CORS and widen the network-facing auth of the endpoint apps use.
- **Custom engines from loopback-only sessions, or CLI-only.** Rejected by the owner: full
  parity.
- **`EventSource` for the event stream.** Rejected: it hides HTTP statuses (401, 410, 429),
  cannot send `X-CapyCTL-UI`, and its automatic `Last-Event-ID` conflicts with `?after=`; the
  UI reads the stream with `fetch` and `eventsource-parser`.
- **SvelteKit.** Rejected: its inline bootstrap script conflicts with §14's CSP. Preact with
  signals was equally viable.
- **Server-rendered templates with htmx.** Rejected: an HTML surface beside the JSON API.
- **Committed `web/dist/`, or a `build.rs` that runs npm.** Rejected: regenerable output is
  not committed, and the release check builds `--offline`.
- **Per-response CSP nonces.** Rejected: they would mean templating `index.html` on the
  server.
- **A dry run of the real accept for the fit check.** Rejected in favor of a per-host
  resolution factored out of acceptance and placement, run read-only.
- **Not built:** a `Host` header allowlist against DNS rebinding (a rebinding page cannot
  carry the host-only cookie and never has the bearer token); a live admin-token rotation
  command (§7); CLI commands to list and revoke sessions (a browser on the box can revoke a
  lost device's session); a pre-send playground warning about switching; rate limits on the
  sign-in exchange (a 256-bit token cannot be guessed).

## Verification

Conformance tests added with the implementation (tests cite ADR 0040):

- `crates/capyctl-config/tests/` and `crates/capyctl-cli/tests/`: the off-box bind with
  `remote` and `public_address`, and the loopback check without it (§1).
- `crates/capyctl-store/tests/ui_sessions.rs`: sessions and links by digest, one-shot
  consumption, revocation, and deletion of stale fingerprints at start, including the v58 to
  v59 migration (§4, §7).
- `crates/capyctl-management/tests/compose.rs` and
  `crates/capyctl-management/tests/ui_sessions.rs`: every route of both roles' composed
  routers answers 401 with no credential and its usual status with a valid session and
  `X-CapyCTL-UI: 1`; the bearer token only from loopback or the local address; the request
  checks; the sign-in exchange; revocation ending live streams (§2–§6).
- `crates/capyctl-cli/tests/open_ui.rs`: `capyctl open ui` and its origin order (§5).
- `crates/capyctl-management/tests/snapshot_actions.rs`: supported-action hints (§8).
- `crates/capyctl-management/tests/check.rs`: the check endpoint, pinning, sizing and
  verdicts, with the store unchanged (§9).
- `crates/capyctl-management/tests/host_admin.rs`, `crates/capyctl-agent/tests/` and
  `crates/capyctl-cli/tests/`: host administration on a standalone role and the custom-engine
  digest and `confirmation_stale` (§10, §11).
- `crates/capyctl-agent/tests/`, `crates/capyctl-cli/tests/` and `tests/harness/`: host
  administration over the server, the `host_admin` capability gate and its refusals (§10).
- `crates/capyctl-management/tests/playground.rs`: the playground forward, headers,
  cancellation and the absence of the inference key (§12).
- `crates/capyctl-webui/tests/serve.rs`: the explicit routes, the no-UI page, path escapes,
  and the CSP and headers on every UI response (§13, §14).

## Owner decisions of 2026-10-27

1. **One product contract for the whole UI: watch, operate, set up and try.** Rejected:
   shipping Watch and Operate first as their own contract.
2. **The primary user is the owner-operator.** Rejected: a non-technical household member, a
   two-tier owner-plus-others setup, and a newcomer doing first-run setup in the browser.
3. **Loopback by default, with an owner opt-in to another address** (§1). Rejected:
   loopback-only with SSH tunnels, network-reachable by default, and HTTPS-only off-box.
4. **Off-box access is plain HTTP and prints no warning** (§1). Rejected: built-in TLS and a
   private-address-only restriction.
5. **Sign in through a one-time link or QR code issued by the CLI on the box** (§5).
   Rejected: pasting the admin token, and offering both.
6. **"Full" means everything a running role can do.** Rejected: strict CLI parity including
   browser-based first-run setup, and a day-2-only UI; `init`, `start` and `join` run before
   or outside a serving role.
7. **Custom engines can be added and changed from any signed-in session** (§11). Rejected:
   loopback-only sessions and CLI-only.
8. **The playground keeps conversation history only in the browser** (§12). Rejected: no
   history, a minimal chat-only playground, and a rich playground.
9. **Desktop and phone are both targets.** Rejected: desktop-only and full parity on a phone.
10. **A classic sidebar layout.** Rejected: a model-cards-first home and a memory-map-first
    home.
11. **Adding a model uses a guided form with a YAML view.** Rejected: a form only and a YAML
    editor only.
