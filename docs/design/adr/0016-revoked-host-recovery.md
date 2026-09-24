# ADR 0016 — A revoked host recovers by re-enrolling under the same identity

**Status:** Accepted (owner decision 2026-09-24).
**Amends:** `SPEC.md` §4.1 (recovery of a revoked or identity-less host). §13.2's
reconciliation and §13.3's revocation rules are applied as written, not changed.
**Related:** ADR 0009 (proof-carrying reconciliation), ADR 0011 (the state machine owns
recovery). It builds on the revocation command merged in PR #3 (`mllm revoke host`).

## Context

`mllm revoke host <name|id>` closes the host's control session, refuses its reconnects
and every new command, closes dispatch to its Ready engines and keeps it out of
placement. Its engines keep running and stay charged until an operator settles them
with evidence (SPEC §13.3). Revocation was absorbing: nothing could bring the host back.

SPEC §4.1 already says that losing identity files requires "explicit recovery or
re-enrollment, not automatic adoption of a similarly named host", and that a host-name
collision never authorizes replacing an identity. It did not say what recovery is. An
ordinary invitation cannot serve: the revoked host's name is still enrolled, so an
invitation for it is refused, and a new name would create a second host record while
the first one still owns engines, reservations and leases.

The owner decided on 2026-09-24 that a revoked host recovers by re-enrolling under the
**same** identity.

## Decision

1. **An explicit, single-use, short-lived recovery invitation.** An administrator runs
   `mllm invite host <name|id> --recover --output FILE`. The server refuses it unless the
   named host exists and is revoked (`host_not_revoked`, 409; an unknown host is 404). The
   invitation is bound to that host id, lives 15 minutes by default (at most one hour, like
   every invitation), and is journaled (`host_recovery_invited`). The join file names the
   host id it re-enrolls (`recover_host_id`); an ordinary invitation's file is unchanged.

2. **The host redeems it explicitly.** The host runs `mllm join host --join-file FILE
   --recover`. An ordinary `join host` refuses a recovery invitation, and `--recover`
   refuses an ordinary one, so neither path can turn into the other. The host keeps its
   state directory and journal. Its retained identity is replaced only when it belongs to
   the same controller and names the same host id; anything else is refused, never
   adopted. If the identity files were lost, recovery starts from fresh ones. A new key is
   always generated: the revoked certificate's key is never reused. As with enrollment,
   the new key and transaction are persisted before any request, so a retry with the same
   invitation resumes the same transaction and a lost reply replays the same certificate.

3. **A new certificate for the same host id; the old one stays revoked for ever.**
   Revocation now revokes each certificate by its own fingerprint as well as the host
   (store v32, `revoked_host_certificates`; certificates of hosts revoked before v32 are
   carried in as revoked). Redeeming a recovery invitation, in one transaction: checks the
   host is still revoked and still has the invited name, revokes every certificate it
   holds, issues a certificate bound to the same host id for the new key, lifts the host's
   revocation, records the new key for renewal, and journals `host_recovered`. The
   invitation is then spent: another transaction on it is refused, and any other
   outstanding recovery invitation recovers nothing because the host is no longer revoked.
   No second host record is ever created. Name-collision rules for new hosts are
   unchanged.

4. **Reconnect reconciles; nothing reopens on trust.** The recovered host reconnects and
   the existing reconciliation of SPEC §13.2 runs as after any session loss:
   - A Ready engine the host still owns is re-proven by a fresh native probe on the new
     session, matching the recorded process identities (pid, boot id, start ticks), before
     dispatch reopens. It is not relaunched.
   - A probe that fails leaves dispatch closed and the engine charged, retained and
     journaled as `readiness_unproven`.
   - Stops and drains that were pending complete normally once the host is back.
   - Accounting is never released without evidence.

5. **A lost journal settles only on gone evidence by identity.** A host that also lost its
   journal has no record of the engines the server still accounts for, so nothing
   re-proves them: they stay closed and charged. An operator stop is then the only way to
   settle them, and it settles only on gone evidence. For that, a Terminate now carries
   the process identities the server recorded for the launch (an additive
   `terminate_recorded_processes` field; empty, it encodes and digests exactly as
   before). A host whose journal knows the launch ignores them and acts on its own record.
   A host with no record of the launch never signals anything: it fences the handle so the
   launch can never start there, and it observes and reports each recorded identity's
   presence. While any recorded process is alive the server's gone check fails and the
   launch stays uncertain and charged; once every one is observed gone, the stop
   completes and releases with that evidence. Nothing is ever killed by a host that does
   not own it.

## Consequences

- A revoked host can come back without an operator deleting and redeploying everything it
  held, and without a second identity appearing in the inventory.
- Recovery is always two explicit actions (administrator and host) with a journal record
  of each. There is no automatic path from revoked to active.
- Revocation keeps its meaning: a revoked certificate is refused on every listener for
  ever, even after the host it belonged to recovers.
- A Terminate for a launch whose cleanup was armed before this change and is replayed
  after it carries a different payload digest; the host refuses the replay as a conflict
  and the cleanup stays uncertain until the operator acts. Only a cleanup in flight across
  the upgrade can meet this.
- CPU and Fake-engine tests cover this (T05, T06, T33, T34). They are not qualification;
  the live revocation row (M45) and a live recovery row remain to be run on the Sparks.

## Alternatives considered

- **Re-enroll under a new host id.** Rejected by the owner: the old record would still own
  engines, reservations and leases that nothing could settle through the new identity.
- **Let an ordinary invitation adopt a revoked name.** Rejected by SPEC §4.1: a name
  collision never authorizes replacing an identity.
- **Un-revoke the old certificate.** Rejected: a revoked certificate may be compromised,
  and SPEC §4.1 requires that a revoked host not accept commands through an old
  connection.
