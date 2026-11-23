import { isMap, isScalar, isSeq, parseDocument, type Document } from 'yaml';

// SPEC R20, R22 (plan U15): the guided form edits the deployment file as a
// `yaml` Document, so a field the form does not model is kept as it is.

export interface DraftError {
  message: string;
  path?: string | null;
  // C1: parser lines are 1-based and columns are 0-based.
  position?: { line: number; column: number } | null;
}

export interface Draft {
  doc: Document;
  // Errors in the YAML itself; while there are any the form is read-only.
  errors: DraftError[];
}

export function parseDraft(text: string): Draft {
  const doc = parseDocument(text, { prettyErrors: false });
  const errors: DraftError[] = doc.errors.map((error) => ({
    message: error.message.split('\n')[0],
    position: error.linePos?.[0]
      ? { line: error.linePos[0].line, column: error.linePos[0].col - 1 }
      : null,
  }));
  if (errors.length === 0 && doc.contents !== null && !isMap(doc.contents)) {
    errors.push({ message: 'A deployment file is a mapping of fields, such as `name: my-model`.' });
  }
  return { doc, errors };
}

export type SourceKind = 'hf' | 'path';

// docs/guide/parking.md: an unset residency parks the way the hardware
// supports.
export const RESIDENCIES = ['', 'host_backed', 'deep', 'restart_only'] as const;

export interface FormFields {
  name: string;
  engine: string;
  sourceKind: SourceKind;
  source: string;
  host: string;
  // Comma-separated GPU ids (`devices: [{id: gpu0}]`).
  devices: string;
  instances: string;
  residency: string;
  memoryRequest: string;
  kvCache: string;
  // '' is chat, the default; `decisions` is a decision model (docs/guide/deploy.md).
  api: string;
  contextLength: string;
}

export type Field = keyof FormFields;

// docs/examples/deployment-single.yaml: where each form field lives.
const PATHS: Record<Exclude<Field, 'engine' | 'sourceKind' | 'source' | 'devices'>, string[]> = {
  name: ['name'],
  host: ['host'],
  instances: ['instances'],
  residency: ['residency'],
  memoryRequest: ['engine_config', 'memory', 'request'],
  kvCache: ['engine_config', 'memory', 'kv_cache'],
  api: ['api'],
  contextLength: ['engine_config', 'context_length'],
};

const NUMERIC: Partial<Record<Field, true>> = { instances: true, contextLength: true };

function text(value: unknown): string {
  return value === undefined || value === null ? '' : String(value);
}

// `runtime_profile` is the long form of `engine`.
function engineKey(doc: Document): string {
  return !doc.has('engine') && doc.has('runtime_profile') ? 'runtime_profile' : 'engine';
}

export function readForm(doc: Document): FormFields {
  const model = doc.get('model', true);
  let sourceKind: SourceKind = 'hf';
  let source = '';
  if (isScalar(model)) {
    sourceKind = 'path';
    source = text(model.value);
  } else if (isMap(model)) {
    if (model.has('hf')) {
      source = text(model.get('hf'));
    } else {
      sourceKind = 'path';
      source = text(model.get('path'));
    }
  }
  const devices = doc.get('devices', true);
  const deviceIds = isSeq(devices)
    ? devices.items.map((item) => (isMap(item) ? text(item.get('id')) : isScalar(item) ? text(item.value) : ''))
    : [];
  const fields = Object.fromEntries(
    Object.entries(PATHS).map(([field, path]) => [field, text(doc.getIn(path))]),
  ) as Omit<FormFields, 'engine' | 'sourceKind' | 'source' | 'devices'>;
  return {
    ...fields,
    engine: text(doc.get(engineKey(doc))),
    sourceKind,
    source,
    devices: deviceIds.filter((id) => id !== '').join(', '),
  };
}

function setOrDelete(doc: Document, path: string[], value: unknown): void {
  if (value === '' || value === undefined) {
    if (doc.hasIn(path)) {
      doc.deleteIn(path);
    }
    return;
  }
  doc.setIn(path, value);
}

// Applies one form field to the document and returns the new text.
export function applyField(doc: Document, field: Field, value: string, form: FormFields): string {
  if (doc.contents === null) {
    doc.contents = doc.createNode({});
  }
  const trimmed = value.trim();
  switch (field) {
    case 'engine':
      setOrDelete(doc, [engineKey(doc)], trimmed);
      break;
    case 'sourceKind':
    case 'source': {
      const kind = field === 'sourceKind' ? (value as SourceKind) : form.sourceKind;
      const source = field === 'source' ? trimmed : form.source;
      if (source === '') {
        setOrDelete(doc, ['model'], kind === 'hf' ? doc.createNode({ hf: '' }, { flow: true }) : '');
      } else {
        doc.set('model', kind === 'hf' ? doc.createNode({ hf: source }, { flow: true }) : source);
      }
      break;
    }
    case 'devices': {
      const ids = trimmed.split(',').map((id) => id.trim()).filter((id) => id !== '');
      setOrDelete(doc, ['devices'], ids.length === 0 ? '' : doc.createNode(ids.map((id) => ({ id })), { flow: true }));
      break;
    }
    default: {
      const number = Number(trimmed);
      const typed = NUMERIC[field] && trimmed !== '' && Number.isInteger(number) ? number : trimmed;
      setOrDelete(doc, PATHS[field], typed);
      // An emptied block does not stay behind as `memory: {}`.
      for (let depth = PATHS[field].length - 1; depth > 0; depth -= 1) {
        const parent = PATHS[field].slice(0, depth);
        const node = doc.getIn(parent, true);
        if (isMap(node) && node.items.length === 0) {
          doc.deleteIn(parent);
        }
      }
    }
  }
  return doc.toString();
}

// Whether the text has a comment line that a form edit may move or drop.
export function hasComments(text: string): boolean {
  return /(^|\s)#/.test(text);
}

// What the accepted-source read writes in place of a secret
// (crates/capyctl-management/src/configuration.rs `REDACTED`).
export const REDACTED = '[redacted]';
