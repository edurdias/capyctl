import type { ObservedState } from './types';

// ADR 0040 (plan KTD15): the UI's wording is a code-keyed table here, so the
// server's messages (which the CLI prints) stay as they are. Words follow the
// user guide (docs/guide/how-it-works.md): ready, parked, stopped. Internal
// terms (leases, epochs, reservations, fencing) never appear (SPEC R15).

export type StateWord = 'ready' | 'parked' | 'stopped' | 'starting' | 'failed' | 'parking' | 'stopping' | 'checking';

export const STATE_WORDS: Record<ObservedState, StateWord> = {
  ready: 'ready',
  parked: 'parked',
  stopped: 'stopped',
  failed: 'failed',
  starting: 'starting',
  waking: 'starting',
  queued: 'starting',
  parking: 'parking',
  stopping: 'stopping',
  uncertain: 'checking',
  reconciling: 'checking',
};

// A suspended deployment reads as stopped (plan KTD9, KTD15). A state this
// build does not know reads as "checking" rather than a raw code.
export function stateWord(state: ObservedState, suspended = false): StateWord {
  if (suspended) {
    return 'stopped';
  }
  return STATE_WORDS[state] ?? 'checking';
}

export interface Refusal {
  // What went wrong, in the owner's words.
  reason: string;
  // What the owner can do about it.
  action: string;
}

// Every code the management envelope carries today
// (crates/capyctl-management/src: lib.rs `error`, configuration.rs
// `ConfigurationFailure::response`, events.rs `Failure`, engine_log.rs,
// drain.rs, enrollment.rs, metrics.rs, hosts.rs), plus `origin_refused`
// from ADR 0040's request checks (plan KTD5).
export const MANAGEMENT_REFUSALS: Record<string, Refusal> = {
  admin_required: {
    reason: 'This action needs access from the CapyCTL box.',
    action: 'Run the administrative command on the box.',
  },
  sign_in_link_refused: {
    reason: 'This sign-in link cannot be used.',
    action: 'Run `capyctl open ui` on the box to create a new link.',
  },
  capability_missing: {
    reason: 'This host does not support the requested action.',
    action: 'Update CapyCTL on the host and reconnect it.',
  },
  toolchain_missing: {
    reason: 'The host is missing tools needed to install this engine.',
    action: 'Install the engine’s required tools on the host, then detect it again.',
  },
  unauthenticated: {
    reason: 'This browser is not signed in.',
    action: 'Run `capyctl open ui` on the box and open the link it prints.',
  },
  origin_refused: {
    reason: 'The request did not come from this CapyCTL page.',
    action: 'Reload the page from the address CapyCTL is served on and try again.',
  },
  invalid_request: {
    reason: 'CapyCTL did not accept the request.',
    action: 'Reload the page and try again. If it keeps happening, the UI and the server may be different versions.',
  },
  unsupported_media_type: {
    reason: 'CapyCTL did not accept the request format.',
    action: 'Reload the page and try again.',
  },
  identity_conflict: {
    reason: 'The host’s identity does not match what CapyCTL has on record.',
    action: 'Check which machine you are joining, then create a new invitation for it.',
  },
  host_not_revoked: {
    reason: 'This host is not revoked, so there is nothing to recover.',
    action: 'No action is needed.',
  },
  queue_full: {
    reason: 'CapyCTL is busy with other requests.',
    action: 'Wait a moment and try again.',
  },
  not_found: {
    reason: 'That item no longer exists.',
    action: 'Go back to the list; it may have been deleted.',
  },
  method_not_allowed: {
    reason: 'CapyCTL does not offer that action here.',
    action: 'Reload the page; the UI and the server may be different versions.',
  },
  internal: {
    reason: 'CapyCTL hit an internal error.',
    action: 'Try again. If it keeps happening, check the role’s log on the box.',
  },
  invalid_cursor: {
    reason: 'The live updates could not resume from where they left off.',
    action: 'The page reloads the current state by itself.',
  },
  cursor_expired: {
    reason: 'The live updates were away too long to catch up.',
    action: 'The page reloads the current state by itself.',
  },
  invalid_config: {
    reason: 'The model’s configuration is not valid.',
    action: 'Fix the field named in the details and try again.',
  },
  body_too_large: {
    reason: 'The request is too large.',
    action: 'Shorten the configuration and try again.',
  },
  deadline_exceeded: {
    reason: 'CapyCTL did not answer in time.',
    action: 'The change may already have happened. Check the current state on the host before trying again.',
  },
  unsupported_capability: {
    reason: 'This CapyCTL role or host cannot do that.',
    action: 'Upgrade the host or the role, then try again.',
  },
  revision_conflict: {
    reason: 'The model changed since this page loaded.',
    action: 'Review the current version and try again.',
  },
  idempotency_conflict: {
    reason: 'A different request was already sent with this request’s identity.',
    action: 'Start the action again from its button.',
  },
  route_conflict: {
    reason: 'Another model already uses that name.',
    action: 'Pick a different name.',
  },
  runtime_retained: {
    reason: 'The model’s engine is still being cleaned up.',
    action: 'Wait until the model shows stopped, then try again.',
  },
  delete_requires_cleanup: {
    reason: 'The model is still running or still being stopped.',
    action: 'Stop the model, or use Stop and delete, and wait until it shows stopped.',
  },
  reconciliation_required: {
    reason: 'CapyCTL is still checking what is running.',
    action: 'Wait a moment and try again.',
  },
  checkpoint_digest_pending: {
    reason: 'CapyCTL is still checking the model’s files.',
    action: 'Wait a moment and try again.',
  },
  checkpoint_mismatch: {
    reason: 'The model’s files do not match the ones recorded for it.',
    action: 'Check the model files on the host, or deploy a new version of the model.',
  },
  checkpoint_unusable: {
    reason: 'The model’s memory needs, measured from its files, do not fit this configuration.',
    action: 'Change the memory limits or placement and save the model again.',
  },
  model_source_pending: {
    reason: 'The model is still downloading on its host.',
    action: 'Wait for the download to finish, then try again.',
  },
  model_source_failed: {
    reason: 'The model’s download failed on its host.',
    action: 'Fix the model source or the host’s download settings, then save the model again.',
  },
  host_policy_denied: {
    reason: 'The host’s settings do not allow this.',
    action: 'Change the host’s settings on the box, or choose another host.',
  },
  capacity_blocked: {
    reason: 'There is not enough free memory, GPU or port room on the host.',
    action: 'Park or stop another model, or use Make room and start.',
  },
  host_ineligible: {
    reason: 'No allowed host can run this model right now.',
    action: 'Check the hosts’ health on the Hosts page, or allow another host.',
  },
  profile_not_published: {
    reason: 'No allowed host has the engine this model needs.',
    action: 'Add that engine on a host from the Engines page, then save the model again.',
  },
  startup_requires_empty_host: {
    reason: 'The model’s first start needs the whole host, because its memory use is not measured yet.',
    action: 'Use Make room and start to switch the other models out.',
  },
  switch_failed: {
    reason: 'Making room did not finish, so nothing was started.',
    action: 'Try again; models that were not switched out keep serving.',
  },
  still_stopping: {
    reason: 'A stop is still finishing, and new starts wait for it.',
    action: 'Wait until the stop finishes, then start again.',
  },
  lifecycle_conflict: {
    reason: 'The model’s current state does not allow that action.',
    action: 'Wait for the model to settle, then try again.',
  },
  host_not_found: {
    reason: 'That host is not known to CapyCTL.',
    action: 'Go back to the Hosts page; it may have been removed.',
  },
  forbidden: {
    reason: 'This engine log was written in debug mode and is never sent off the host.',
    action: 'Read it on the host itself.',
  },
  observation_stale: {
    reason: 'CapyCTL does not have a current view of this host.',
    action: 'Check that the host is connected and wait for its details to refresh before starting another change.',
  },
  snapshot_unavailable: {
    reason: 'CapyCTL could not read its current state.',
    action: 'Wait a moment and try again.',
  },
  // ADR 0040 §§10, 11: host administration.
  shutting_down: {
    reason: 'CapyCTL is shutting down.',
    action: 'Wait for CapyCTL to restart and check the current state before trying again.',
  },
  profile_in_use: {
    reason: 'A model still uses this engine profile.',
    action: 'Move or remove the models using this profile before removing it.',
  },
  profile_exists: {
    reason: 'An engine profile already uses that name.',
    action: 'Choose another name, or review the existing profile before changing it.',
  },
  profile_not_found: {
    reason: 'That engine profile no longer exists.',
    action: 'Refresh the engines list and choose an existing profile.',
  },
  publish_rejected: {
    reason: 'The running CapyCTL role did not accept the engine profile.',
    action: 'Check the host’s engines and configuration before making another change.',
  },
  confirmation_required: {
    reason: 'This engine change needs your approval.',
    action: 'Request a preview, review the command and its effects, then confirm it.',
  },
  custom_manifest_invalid: {
    reason: 'The custom engine manifest is not valid.',
    action: 'Correct the manifest using the details, then request a new preview.',
  },
  engine_unsupported: {
    reason: 'This engine installation does not support the requested setup.',
    action: 'Choose a supported engine installation or change the requested settings.',
  },
  host_admin_busy: {
    reason: 'The host is busy with other configuration changes.',
    action: 'Wait for those changes to finish, refresh the host’s details, then try again.',
  },
  reply_too_large: {
    reason: 'The host’s answer was too large to send.',
    action: 'Check the host’s configuration and engines directly before repeating a change.',
  },
  'host_capability_missing:host_admin': {
    reason: 'This host runs an older CapyCTL that cannot be managed from here.',
    action: 'Update CapyCTL on the host and restart it, then try again.',
  },
  host_upgrade_required: {
    reason: 'This host runs a CapyCTL too old to talk to this server.',
    action: 'Update CapyCTL on the host and restart it, then try again.',
  },
  outcome_unknown: {
    reason: 'The host did not return a confirmed answer, so the change may or may not have happened.',
    action: 'Check the host’s current configuration and engines before trying again.',
  },
  confirmation_stale: {
    reason: 'The host’s details changed after you reviewed the change.',
    action: 'Request a new preview, review it, and confirm again.',
  },
  agent_unreachable: {
    reason: 'CapyCTL could not reach the role on the host, so the outcome is unknown.',
    action: 'Check the host’s engines list before trying again.',
  },
};

// Every code the router envelope carries today
// (crates/capyctl-router/src/chat.rs `err` and lib.rs `err_json`).
export const ROUTER_REFUSALS: Record<string, Refusal> = {
  unauthorized: {
    reason: 'The request was not signed in.',
    action: 'Sign in again with `capyctl open ui` on the box.',
  },
  request_too_large: {
    reason: 'The message is too large.',
    action: 'Shorten the conversation or the message and send it again.',
  },
  invalid_request: {
    reason: 'The model did not accept the request.',
    action: 'Check the message and its settings, then send it again.',
  },
  internal: {
    reason: 'CapyCTL hit an internal error.',
    action: 'Try again. If it keeps happening, check the role’s log on the box.',
  },
  unknown_model: {
    reason: 'No model with that name exists.',
    action: 'Pick a model from the list.',
  },
  queue_full: {
    reason: 'The model is busy with other requests.',
    action: 'Wait a moment and send it again.',
  },
  unsupported_parameter: {
    reason: 'The model’s engine does not support one of the request’s settings.',
    action: 'Remove that setting and send it again.',
  },
  cache_salt_unsupported: {
    reason: 'The model’s engine cannot keep this request’s cache separate.',
    action: 'Send it without the cache setting.',
  },
  api_mismatch: {
    reason: 'This model answers a different kind of request (chat or decisions).',
    action: 'Use the playground tab that matches the model.',
  },
  unsupported: {
    reason: 'The model’s engine cannot answer this kind of request.',
    action: 'Choose another model.',
  },
  insufficient_resources: {
    reason: 'There is not enough room on the host to start the model.',
    action: 'Park or stop another model, then send it again.',
  },
  conflict: {
    reason: 'The model is changing state.',
    action: 'Wait for it to settle, then send it again.',
  },
  deployment_stopped: {
    reason: 'The model is stopped.',
    action: 'Start the model, then send it again.',
  },
  deployment_inactive: {
    reason: 'The model only starts when you start it.',
    action: 'Start the model, then send it again.',
  },
  activation_uncertain: {
    reason: 'The model may still be starting.',
    action: 'Wait a moment and send it again.',
  },
  activation_failed: {
    reason: 'The model failed to start.',
    action: 'Open the model’s page to see why.',
  },
  unavailable: {
    reason: 'The model is not serving right now.',
    action: 'Wait a moment and send it again.',
  },
  shutting_down: {
    reason: 'The model’s engine is shutting down.',
    action: 'Wait a moment and send it again.',
  },
  engine_rejected: {
    reason: 'The model’s engine rejected the request.',
    action: 'Check the details, adjust the message or its settings, and send it again.',
  },
  engine_error: {
    reason: 'The model’s engine answered with an error.',
    action: 'Open the model’s page and check its engine log.',
  },
};

export interface RefusalText {
  reason: string;
  action: string | null;
  // The server's own message, shown under "Details".
  details: string;
}

// The server's message is the fallback for a code this table does not know.
export function describeRefusal(error: { code: string | null; message: string; envelope: 'management' | 'router' | 'none' }): RefusalText {
  const table = error.envelope === 'router' ? ROUTER_REFUSALS : MANAGEMENT_REFUSALS;
  const known = error.code !== null && Object.hasOwn(table, error.code) ? table[error.code] : undefined;
  if (known) {
    return { reason: known.reason, action: known.action, details: error.message };
  }
  return { reason: error.message || 'CapyCTL refused the request.', action: null, details: error.message };
}

// ADR 0040 §9: why a fit check could not decide, in guide wording.
export const UNKNOWN_FIT_REASONS: Record<string, string> = {
  host_offline: 'The host is not connected, so CapyCTL cannot tell whether the model fits.',
  no_observations: 'The host has not reported its memory yet, so CapyCTL cannot tell whether the model fits.',
  weights_unknown: 'The model’s size is not known until it is measured on the host, so CapyCTL cannot tell whether it fits.',
};
