# The web UI

A standalone role and a server serve a web UI on their management address.
From a browser on your desktop or phone you can watch your models, hosts,
engines and memory, start, park and stop models, add and edit deployments,
manage engines and hosts, and try a model in the playground. The terminal is
still needed to `init`, `start` and `join`, and to sign in a new browser.

The UI is at `/ui/` on the management address (`/` takes you there). On the
default address that is `http://127.0.0.1:7443/ui/`, on the machine itself.

## Sign in

Each browser signs in once, with a one-time link the CLI prints on the machine
running CapyCTL:

```bash
capyctl open ui
```

It prints a sign-in link, `<address>/ui/sign-in#t=<token>`, and a QR code
under it. Open the link in the browser you want to use, or scan the code with
your phone, then select **Sign in this browser**. The link works once, for 10
minutes; several links may be outstanding at once. Opening a link only shows
the page, so a chat app or a scanner that previews it does not use it up.

The address in the link is, in order: `--address <origin>` if you give one,
else the role's `public_address`, else the management address when it names a
specific address. On the default loopback address the link works only on the
machine itself, and the command says that a phone needs the
[off-box setting](#use-it-from-another-device).

A browser that is not signed in, or whose session was revoked, shows a page
telling you to run `capyctl open ui`.

A browser stays signed in until you sign it out or revoke it, across restarts
of the role. Signing in with a browser gives it everything the admin token
allows.

## What you can do

- **Overview**: every model with its state (ready, parked, stopped, and the
  steps in between), what each host holds in GPU memory and host RAM, and
  which model the router is switching to. Changes appear within about two
  seconds.
- **Models**: a model's detail, and the actions its state allows: start,
  park, wake, stop and delete. When an action is refused, the UI says why, and
  offers **Make room and start** or **Stop and delete** where those apply.
  Add a model with a guided form or as YAML; the UI checks the deployment and
  whether it fits in memory before you save it.
- **Hosts** (server): enrolled hosts, draining and revoking them, inviting a
  new host, the models copied on each host, and each host's settings.
- **Engines**: detect, add and remove engines, including a
  [custom engine](engines.md#custom-engines), which you confirm by typing its
  name.
- **Playground**: chat with a model, or send a decision request, and see the
  timings. History stays in the browser.
- **Activity**: the event log.
- **Settings**: your signed-in browsers, and the version.

The UI acts through the same management API as the CLI, so everything it does
is recorded and refused in the same way.

## Use it from another device

The management address stays on loopback unless you allow it off the machine
with `remote: true`. Then bind it to your Tailscale address (or `0.0.0.0`):

```yaml
# standalone.yaml (a server document has the same block at the top level)
server:
  listeners:
    management:
      bind: "100.64.0.21:7443"   # this machine's `tailscale ip -4`
      remote: true
```

or for one run:

```bash
capyctl start standalone --management-remote --management-listen "$(tailscale ip -4):7443"
```

The UI is plain HTTP. Use it over Tailscale or a network you trust, and read
[Web UI off the machine](../operations/network-access.md#web-ui-off-the-machine)
before you turn it on: anyone with a signed-in browser who can reach the address
can run any program on your machines as the CapyCTL user, through a custom
engine.

The admin token never leaves the machine: from another device, a signed-in
browser is the only way in.

## Sign a browser out

In **Settings**, **Sign out** ends this browser's session, **Revoke** ends
another one, and **Revoke all sessions** ends every one, this browser's
included. A revoked browser stops at once, live views included. Revoking does
not undo anything that browser already did.

Rotating the admin token (replace it in the credentials file and restart the
role) also ends every session and every unused sign-in link. Restoring a
backup of the role's state brings back sessions you had revoked since the
backup, so revoke all sessions after a restore
([data retention](../operations/data-retention.md#web-ui-sessions-and-sign-in-links)).
