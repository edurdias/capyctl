// ADR 0040 §9 (plan U15): the guided form, the YAML view and the fit check,
// driven through the mounted App against the management API's shapes.
import { EditorView } from '@codemirror/view';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { parse, stringify } from 'yaml';
import type { CheckResult, Fit } from '../src/lib/deployment-check';
import type { Call } from './helpers';
import { json, managementError } from './helpers';
import { button, DEPLOYMENT_ID, open, role, setUp, snapshotWith, tearDown, tick } from './fixtures';

beforeEach(setUp);
afterEach(tearDown);

const CHECK = '/management/v1/deployments/check';
const DEBOUNCE_MS = 500;

const SHA = '0123456789abcdef0123456789abcdef01234567';

// C1 / U7: the real Rust check.rs `measured_weights_fit_with_a_pinned_config_and_no_writes`
// asserts this SHA, top-level verdict and estimated flag. The remaining envelope
// follows src/check.rs's response builder, not an invented nested `fit`.
// These are source-derived fixtures, not captured runtime responses.
const PINNED_CONFIG = {
  schema_version: 1,
  kind: 'deployment',
  name: 'preview',
  runtime_profile: 'vllm',
  routes: ['preview'],
  recipe: 'standard',
  recovery: 'reconcile',
  model: {
    source: { type: 'huggingface', repo: 'org/model', revision: SHA },
    content_fingerprint: 'measured',
    revision: '1',
  },
};

function checkBody(fit: Pick<Fit, 'verdict'> & Partial<Fit>, config: Record<string, unknown> = PINNED_CONFIG): CheckResult {
  const verdict = { estimated: false, reason: null, limit: null, ...fit };
  return {
    api_version: '1',
    config,
    yaml: stringify(config),
    errors: [],
    ...verdict,
    hosts: [{ host_id: 'lab', ...verdict, estimated: verdict.estimated ?? false, config }],
  };
}

function checkWith(fit: Pick<Fit, 'verdict'> & Partial<Fit>) {
  return (...args: [Call]): Response => { void args; return json(checkBody(fit)); };
}

// C2: the accepted input document, not EffectiveDeployment's selected devices,
// recipe fingerprint or frozen host. Keep the exact YAML the source read sends.
const SOURCE_CONFIG = { name: 'qwen', engine: 'vllm', model: { hf: 'Qwen/Qwen3-4B' } };
const SOURCE_YAML = '# accepted source\nname: qwen\nengine: vllm\nmodel: {hf: Qwen/Qwen3-4B}\n';

function sourceBody(revision = '4', config: Record<string, unknown> = SOURCE_CONFIG, yaml = SOURCE_YAML) {
  return { api_version: '1', deployment_id: DEPLOYMENT_ID, revision, config, yaml };
}

function input(name: string): HTMLInputElement {
  const found = document.querySelector<HTMLInputElement>(`[name="${name}"]`);
  if (found === null) {
    throw new Error(`no field ${name}`);
  }
  return found;
}

async function type(name: string, value: string): Promise<void> {
  const field = input(name);
  field.value = value;
  field.dispatchEvent(new Event('input', { bubbles: true }));
  await tick();
}

async function tab(name: 'Form' | 'YAML'): Promise<void> {
  button(name).click();
  await tick();
  await vi.dynamicImportSettled();
  await tick();
}

function editor(): EditorView {
  const dom = document.querySelector<HTMLElement>('.cm-editor');
  const view = dom ? EditorView.findFromDOM(dom) : null;
  if (view === null) {
    throw new Error('no YAML editor');
  }
  return view;
}

// What typing in the editor does: replace its whole text.
async function typeYaml(text: string): Promise<void> {
  const view = editor();
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
  await tick();
}

function text(): string {
  return document.body.textContent ?? '';
}

describe('add a model', () => {
  // SPEC R20, R22
  it('keeps the form and the YAML in step both ways', async () => {
    role({ snapshots: [snapshotWith([])], route: (call) => (call.url === CHECK ? checkWith({ verdict: 'fits' })(call) : undefined) });
    await open('/models/new');
    await type('engine', 'sglang');
    await tab('YAML');
    expect(parse(editor().state.doc.toString())).toMatchObject({ engine: 'sglang' });

    await typeYaml('name: qwen\nengine: sglang\nmodel: {hf: Qwen/Qwen3-4B}\nengine_config:\n  context_length: 8192\n');
    await tab('Form');
    expect(input('contextLength').value).toBe('8192');
    expect(input('name').value).toBe('qwen');
  });

  // SPEC R22: the form edits the document, not a fresh one.
  it('keeps a field only the YAML holds through a form edit', async () => {
    const checks: Call[] = [];
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url !== CHECK) {
          return undefined;
        }
        checks.push(call);
        return checkWith({ verdict: 'fits' })(call);
      },
    });
    await open('/models/new');
    await tab('YAML');
    await typeYaml('name: qwen\nengine: vllm\nmodel: {hf: Qwen/Qwen3-4B}\nlabels:\n  team: search\n');
    await tab('Form');
    await type('name', 'qwen-small');
    await tick(DEBOUNCE_MS);
    const sent = parse((JSON.parse(checks.at(-1)?.body ?? '{}') as { yaml: string }).yaml) as Record<string, unknown>;
    expect(sent).toMatchObject({ name: 'qwen-small', labels: { team: 'search' } });
  });

  // SPEC R22
  it('makes the form read-only while the YAML is invalid and keeps the text', async () => {
    role({ snapshots: [snapshotWith([])], route: (call) => (call.url === CHECK ? checkWith({ verdict: 'fits' })(call) : undefined) });
    await open('/models/new');
    await tab('YAML');
    const broken = 'name: qwen\nengine: [vllm\n';
    await typeYaml(broken);
    await tab('Form');
    expect(document.querySelector('fieldset')?.disabled).toBe(true);
    expect(text()).toContain('The YAML has errors, so the form is read-only.');
    await tab('YAML');
    expect(editor().state.doc.toString()).toBe(broken);
  });

  // SPEC R21, AE5
  it('shows a does_not_fit limit and still adds the model, stopped', async () => {
    const creates: Call[] = [];
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === CHECK) {
          return checkWith({ verdict: 'does_not_fit', limit: 'insufficient_device_memory: gpu0 has 8 GiB free, the model needs 30 GiB' })(call);
        }
        if (call.url === '/management/v1/deployments' && call.method === 'POST') {
          creates.push(call);
          return json({ api_version: '1', deployment_id: DEPLOYMENT_ID, revision: '1' }, 201);
        }
        return undefined;
      },
    });
    await open('/models/new');
    await type('name', 'qwen');
    await type('source', 'Qwen/Qwen3-32B');
    await tick(DEBOUNCE_MS);
    expect(text()).toContain('It does not fit in memory: insufficient_device_memory: gpu0 has 8 GiB free, the model needs 30 GiB.');
    const add = button('Add');
    expect(add.disabled).toBe(false);
    add.click();
    await tick();
    expect(creates).toHaveLength(1);
    expect(JSON.parse(creates[0].body ?? '{}')).toEqual({
      config: PINNED_CONFIG,
      activate: false,
    });
  });

  // SPEC R21 (ADR 0040 §9)
  it.each([
    [{ verdict: 'needs_room' }, 'starting it will switch other models out to make room'],
    [{ verdict: 'unknown', reason: 'weights_unknown' }, 'The model’s size is not known until it is measured on the host'],
    [{ verdict: 'unknown', reason: 'no_observations' }, 'The host has not reported its memory yet'],
  ] as const)('shows the %o wording and keeps Add enabled', async (fit, wording) => {
    role({ snapshots: [snapshotWith([])], route: (call) => (call.url === CHECK ? checkWith(fit)(call) : undefined) });
    await open('/models/new');
    await type('name', 'qwen');
    await type('source', 'Qwen/Qwen3-4B');
    await tick(DEBOUNCE_MS);
    expect(text()).toContain(wording);
    expect(button('Add').disabled).toBe(false);
  });

  // C1: with no candidate host the server's selected metadata can be null.
  it('shows an unknown fit with null metadata and no hosts', async () => {
    const answer = { ...checkBody({ verdict: 'unknown', estimated: null }), hosts: [] };
    role({ snapshots: [snapshotWith([])], route: (call) => call.url === CHECK ? json(answer) : undefined });
    await open('/models/new');
    await type('name', 'preview');
    await type('source', 'org/model');
    await tick(DEBOUNCE_MS);
    expect(text()).toContain('CapyCTL cannot tell yet whether the model fits.');
    expect(text()).not.toContain('This is an estimate');
    expect(button('Add').disabled).toBe(false);
  });

  // C1 / U7: top-level fit and per-host results are the management wire
  // shape; an offline candidate must not hide a fitting host.
  it('shows the selected fit, file-size estimate and every host result', async () => {
    const answer = checkBody({ verdict: 'fits', estimated: true });
    answer.hosts.unshift({ host_id: 'offline-box', verdict: 'unknown', reason: 'host_offline', estimated: false, limit: null });
    role({ snapshots: [snapshotWith([])], route: (call) => call.url === CHECK ? json(answer) : undefined });
    await open('/models/new');
    await type('name', 'preview');
    await type('source', 'org/model');
    await tick(DEBOUNCE_MS);
    expect(document.querySelector('.fit')?.getAttribute('data-verdict')).toBe('fits');
    expect(text()).toContain('This is an estimate from the size of the model’s files');
    expect(document.querySelector('[data-host="offline-box"]')?.textContent).toContain('The host is not connected');
    expect(document.querySelector('[data-host="lab"]')?.textContent).toContain('It fits in memory.');
    expect(button('Add').disabled).toBe(false);
  });

  // C1: object limits must stay visible, never collapse to "[object Object]".
  it('shows structured start limits without losing their fields', async () => {
    const limit = { code: 'insufficient_device_memory', device: 'gpu0', available_bytes: 8 * 1024 ** 3, required_bytes: 30 * 1024 ** 3 };
    role({ snapshots: [snapshotWith([])], route: (call) => call.url === CHECK ? checkWith({ verdict: 'does_not_fit', limit })(call) : undefined });
    await open('/models/new');
    await type('name', 'preview');
    await type('source', 'org/model');
    await tick(DEBOUNCE_MS);
    expect(text()).toContain(JSON.stringify(limit));
    expect(text()).not.toContain('[object Object]');
    expect(button('Add').disabled).toBe(false);
  });

  // C1 / U15: errors survive request()'s throw. Rust check.rs's duplicate-key
  // assertion fixes position.line at 4; columns remain 0-based, including zero.
  it.each([
    [{ line: 4, column: 0 }, null, 'Line 4, column 0: '],
    [null, 'engine_config.context_length', 'engine_config.context_length: '],
  ])('shows server validation locations from an HTTP 400 (%o)', async (position, path, prefix) => {
    role({
      snapshots: [snapshotWith([])],
      route: (call) => call.url === CHECK ? json({
        api_version: '1',
        config: null,
        yaml: null,
        errors: [{ message: 'Strict parser rejected this field', path, position }],
        error: { code: 'invalid_config', message: 'Strict parser rejected this field', retryable: false },
      }, 400) : undefined,
    });
    await open('/models/new');
    await type('name', 'preview');
    await type('source', 'org/model');
    await tick(DEBOUNCE_MS);
    expect(text()).toContain(`${prefix}Strict parser rejected this field`);
    expect(button('Add').disabled).toBe(true);
  });

  // U15: invalidation happens on edit, not only when the next debounce fires.
  it('ignores an old answer during the new draft’s debounce window', async () => {
    const checks: { call: Call; resolve: (response: Response) => void }[] = [];
    role({
      snapshots: [snapshotWith([])],
      // ES2022 does not expose Promise.withResolvers.
      route: (call) => call.url === CHECK ? new Promise<Response>((resolve) => checks.push({ call, resolve })) : undefined,
    });
    await open('/models/new');
    await type('name', 'preview');
    await type('source', 'org/model');
    await tick(DEBOUNCE_MS);
    await type('name', 'new-draft');
    expect(checks[0].call.signal?.aborted).toBe(true);
    checks[0].resolve(json(checkBody({ verdict: 'fits' })));
    await tick();
    expect(button('Add').disabled).toBe(true);
    expect(document.querySelector('.fit')).toBeNull();
    await tick(DEBOUNCE_MS);
    expect(checks).toHaveLength(2);
    checks[1].resolve(json(checkBody({ verdict: 'fits' })));
    await tick();
    expect(button('Add').disabled).toBe(false);
  });

  // U15: a transport can finish despite abort. Neither an old success nor an
  // old 400 may replace the current verdict or normalized config.
  it.each(['success', 'refusal'])('ignores an out-of-order old %s after a newer answer', async (older) => {
    const checks: { call: Call; resolve: (response: Response) => void }[] = [];
    const creates: Call[] = [];
    const currentConfig = { ...PINNED_CONFIG, name: 'current', routes: ['current'] };
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === CHECK) {
          return new Promise<Response>((resolve) => checks.push({ call, resolve }));
        }
        if (call.url === '/management/v1/deployments' && call.method === 'POST') {
          creates.push(call);
          return json({ api_version: '1', deployment_id: DEPLOYMENT_ID, revision: '1' }, 201);
        }
        return undefined;
      },
    });
    await open('/models/new');
    await type('source', 'org/model');
    await type('name', 'old');
    await tick(DEBOUNCE_MS);
    await type('name', 'current');
    await tick(DEBOUNCE_MS);
    expect(checks[0].call.signal?.aborted).toBe(true);
    checks[1].resolve(json(checkBody({ verdict: 'fits' }, currentConfig)));
    await tick();
    checks[0].resolve(older === 'success'
      ? json(checkBody({ verdict: 'does_not_fit', limit: 'obsolete limit' }))
      : json({ api_version: '1', errors: [{ message: 'obsolete parser error', position: { line: 4, column: 0 } }], error: { code: 'invalid_config', message: 'obsolete parser error' } }, 400));
    await tick();
    expect(document.querySelector('.fit')?.getAttribute('data-verdict')).toBe('fits');
    expect(text()).not.toContain('obsolete');
    expect(button('Add').disabled).toBe(false);
    button('Add').click();
    await tick();
    expect(JSON.parse(creates[0].body ?? '{}')).toEqual({ config: currentConfig, activate: false });
  });

  // U15 / KTD16: Add and start is two commands, not an activating create.
  // A refused start must not undo Add or allow a duplicate create.
  it.each([false, true])('adds before starting with separate keys (start refused: %s)', async (refused) => {
    const commands: Call[] = [];
    let acceptCreate: ((response: Response) => void) | undefined;
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === CHECK) {
          return checkWith({ verdict: 'fits' })(call);
        }
        if (call.url === '/management/v1/deployments' && call.method === 'POST') {
          commands.push(call);
          return new Promise<Response>((resolve) => { acceptCreate = resolve; });
        }
        if (call.url === `/management/v1/deployments/${DEPLOYMENT_ID}/actions`) {
          commands.push(call);
          return refused ? managementError(409, 'capacity_blocked') : json({ api_version: '1', deployment_id: DEPLOYMENT_ID, operation_id: 'start-operation' }, 202);
        }
        return undefined;
      },
    });
    await open('/models/new');
    await type('name', 'preview');
    await type('source', 'org/model');
    await tick(DEBOUNCE_MS);
    button('Add and start').click();
    await tick();
    expect(commands).toHaveLength(1);
    expect(JSON.parse(commands[0].body ?? '{}')).toEqual({ config: PINNED_CONFIG, activate: false });
    expect(acceptCreate).toBeTypeOf('function');
    acceptCreate?.(json({ api_version: '1', deployment_id: DEPLOYMENT_ID, revision: '7' }, 201));
    await tick();
    expect(commands).toHaveLength(2);
    expect(commands.map((call) => call.method)).toEqual(['POST', 'POST']);
    expect(JSON.parse(commands[1].body ?? '{}')).toMatchObject({ action: 'start', expected_revision: 7 });
    expect(commands[1].headers.get('Idempotency-Key')).toMatch(/^[0-9a-f]{32}$/);
    expect(commands[1].headers.get('Idempotency-Key')).not.toBe(commands[0].headers.get('Idempotency-Key'));
    if (refused) {
      expect(text()).toContain('The model was added and is stopped. It did not start:');
      expect(document.querySelector('a[href$="/models/' + DEPLOYMENT_ID + '"]')).not.toBeNull();
      expect(button('Add').disabled).toBe(true);
      expect(button('Add and start').disabled).toBe(true);
      button('Add and start').click();
      await tick();
      expect(commands).toHaveLength(2);
    } else {
      expect(window.location.pathname).toBe(`/ui/models/${DEPLOYMENT_ID}`);
    }
  });

  // U15 / R22: the first form edit warns once; later edits do not
  // recreate/reannounce the notice.
  it('warns once when form edits may move YAML comments', async () => {
    role({ snapshots: [snapshotWith([])], route: (call) => call.url === CHECK ? checkWith({ verdict: 'fits' })(call) : undefined });
    await open('/models/new');
    await tab('YAML');
    await typeYaml('# keep my note\nname: qwen\nengine: vllm\nmodel: {hf: Qwen/Qwen3-4B}\n');
    await tab('Form');
    expect(document.querySelector('.notice')).toBeNull();
    await type('name', 'first-edit');
    const notice = document.querySelector('.notice');
    expect(notice?.textContent).toContain('Comments in the YAML may not be kept');
    await type('name', 'second-edit');
    expect(document.querySelectorAll('.notice')).toHaveLength(1);
    expect(document.querySelector('.notice')).toBe(notice);
    await tab('YAML');
    await tab('Form');
    await type('engine', 'sglang');
    expect(document.querySelectorAll('.notice')).toHaveLength(1);
  });
});

describe('edit a model', () => {
  const SOURCE = `/management/v1/deployments/${DEPLOYMENT_ID}/source`;
  const REPLACE = `/management/v1/deployments/${DEPLOYMENT_ID}`;

  // SPEC R23
  it('keeps the draft after a revision_conflict and offers a reload', async () => {
    const replaces: Call[] = [];
    let reads = 0;
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === SOURCE) {
          reads += 1;
          return json(reads === 1 ? sourceBody() : sourceBody('9', { ...SOURCE_CONFIG, name: 'current-source' }, 'name: current-source\nengine: vllm\nmodel: {hf: Qwen/Qwen3-4B}\n'));
        }
        if (call.url === CHECK) {
          return checkWith({ verdict: 'fits' })(call);
        }
        if (call.url === REPLACE && call.method === 'PUT') {
          replaces.push(call);
          return managementError(409, 'revision_conflict');
        }
        return undefined;
      },
    });
    await open(`/models/${DEPLOYMENT_ID}/edit`);
    await type('name', 'qwen-renamed');
    await tick(DEBOUNCE_MS);
    button('Save').click();
    await tick();
    expect(JSON.parse(replaces[0].body ?? '{}')).toMatchObject({ expected_revision: 4, config: PINNED_CONFIG });
    expect(input('name').value).toBe('qwen-renamed');
    expect(button('Load the current version')).toBeTruthy();
    button('Load the current version').click();
    await tick();
    expect(input('name').value).toBe('current-source');
    await tick(DEBOUNCE_MS);
    button('Save').click();
    await tick();
    expect(JSON.parse(replaces[1].body ?? '{}')).toMatchObject({ expected_revision: 9, config: PINNED_CONFIG });
  });

  // C2 / U15 / R23: use the accepted document's YAML and revision, but save
  // the check's normalized pinned config. Never read effective-config to edit.
  it('loads accepted source YAML and saves the revision returned with it', async () => {
    const replaces: Call[] = [];
    const harness = role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === SOURCE) {
          return json(sourceBody('31'));
        }
        if (call.url === CHECK) {
          expect(JSON.parse(call.body ?? '{}')).toEqual({ yaml: SOURCE_YAML });
          return checkWith({ verdict: 'fits' })(call);
        }
        if (call.url === REPLACE && call.method === 'PUT') {
          replaces.push(call);
          return json({ api_version: '1', deployment_id: DEPLOYMENT_ID, revision: '32' });
        }
        return undefined;
      },
    });
    await open(`/models/${DEPLOYMENT_ID}/edit`);
    expect(input('name').value).toBe('qwen');
    expect(input('engine').value).toBe('vllm');
    await tab('YAML');
    expect(editor().state.doc.toString()).toBe(SOURCE_YAML);
    expect(harness.calls.filter((call) => call.url.endsWith('/effective-config'))).toHaveLength(0);
    await tick(DEBOUNCE_MS);
    expect(button('Save').disabled).toBe(false);
    button('Save').click();
    await tick();
    expect(replaces).toHaveLength(1);
    expect(JSON.parse(replaces[0].body ?? '{}')).toEqual({ config: PINNED_CONFIG, expected_revision: 31 });
    expect(window.location.pathname).toBe(`/ui/models/${DEPLOYMENT_ID}`);
  });

  // SPEC R28: the browser only ever has what the snapshot redacts.
  it('shows the redaction, not a secret, in the YAML view, and will not save it', async () => {
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === SOURCE) {
          const config = { ...SOURCE_CONFIG, engine_config: { args: ['--api-key=[redacted]'] } };
          return json(sourceBody('4', config, stringify(config)));
        }
        return call.url === CHECK ? checkWith({ verdict: 'fits' })(call) : undefined;
      },
    });
    await open(`/models/${DEPLOYMENT_ID}/edit`);
    await tab('YAML');
    await tick(DEBOUNCE_MS);
    expect(editor().state.doc.toString()).toContain('--api-key=[redacted]');
    expect(button('Save').disabled).toBe(true);
  });
});
