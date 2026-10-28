// ADR 0040 §9 (plan U15): the guided form, the YAML view and the fit check,
// driven through the mounted App against the management API's shapes.
import { EditorView } from '@codemirror/view';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { parse } from 'yaml';
import type { Call } from './helpers';
import { json, managementError } from './helpers';
import { button, DEPLOYMENT_ID, open, role, setUp, snapshotWith, tearDown, tick } from './fixtures';

beforeEach(setUp);
afterEach(tearDown);

const CHECK = '/management/v1/deployments/check';
const DEBOUNCE_MS = 500;

type Fit = { verdict: string; estimated?: boolean; limit?: string; reason?: string; host_id?: string };

// The check endpoint answering the posted YAML as its normalized config.
function checkWith(fit: Fit) {
  return (call: Call): Response => {
    const { yaml } = JSON.parse(call.body ?? '{}') as { yaml: string };
    return json({ config: parse(yaml) as unknown, yaml, errors: [], fit: { estimated: false, ...fit } });
  };
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
      config: { name: 'qwen', engine: 'vllm', model: { hf: 'Qwen/Qwen3-32B' } },
      activate: false,
    });
  });

  // SPEC R21 (ADR 0040 §9)
  it.each([
    [{ verdict: 'needs_room' }, 'starting it will switch other models out to make room'],
    [{ verdict: 'unknown', reason: 'weights_unknown' }, 'The model’s size is not known until it is measured on the host'],
  ])('shows the %o wording and keeps Add enabled', async (fit, wording) => {
    role({ snapshots: [snapshotWith([])], route: (call) => (call.url === CHECK ? checkWith(fit)(call) : undefined) });
    await open('/models/new');
    await type('name', 'qwen');
    await type('source', 'Qwen/Qwen3-4B');
    await tick(DEBOUNCE_MS);
    expect(text()).toContain(wording);
    expect(button('Add').disabled).toBe(false);
  });
});

describe('edit a model', () => {
  const EFFECTIVE = `/management/v1/deployments/${DEPLOYMENT_ID}/effective-config`;
  const REPLACE = `/management/v1/deployments/${DEPLOYMENT_ID}`;

  // SPEC R23
  it('keeps the draft after a revision_conflict and offers a reload', async () => {
    const replaces: Call[] = [];
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === EFFECTIVE) {
          return json({ deployment_id: DEPLOYMENT_ID, revision: '4', effective: { name: 'qwen', engine: 'vllm', model: { hf: 'Qwen/Qwen3-4B' } }, hosts: [] });
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
    expect(JSON.parse(replaces[0].body ?? '{}')).toMatchObject({ expected_revision: 4, config: { name: 'qwen-renamed' } });
    expect(input('name').value).toBe('qwen-renamed');
    expect(button('Load the current version')).toBeTruthy();
  });

  // SPEC R28: the browser only ever has what the snapshot redacts.
  it('shows the redaction, not a secret, in the YAML view, and will not save it', async () => {
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === EFFECTIVE) {
          return json({
            deployment_id: DEPLOYMENT_ID,
            revision: '4',
            effective: { name: 'qwen', engine: 'vllm', model: { hf: 'Qwen/Qwen3-4B' }, engine_config: { args: ['--api-key=[redacted]'] } },
            hosts: [],
          });
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
