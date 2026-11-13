import { navigate } from './router';

// ADR 0040 (plan KTD5): every cookie-authenticated request carries this
// header. A page on another origin cannot send it without a CORS preflight,
// which the role never answers.
export const UI_HEADER = 'X-CapyCTL-UI';

export type Envelope = 'management' | 'router' | 'none';

// One refusal from either error envelope:
// management `{api_version, error:{code, message, retryable, operation_id, details}}`
// (crates/capyctl-management/src/lib.rs `error`) and router
// `{code, message, retryable}` (crates/capyctl-router/src/chat.rs `err`).
export class ApiError extends Error {
  readonly status: number;
  readonly code: string | null;
  readonly retryable: boolean;
  readonly operationId: string | null;
  readonly details: Record<string, unknown>;
  readonly envelope: Envelope;
  // C1: check refusals carry parser errors beside the management envelope.
  readonly body: unknown;

  constructor(init: {
    status: number;
    code: string | null;
    message: string;
    retryable: boolean;
    operationId: string | null;
    details: Record<string, unknown>;
    envelope: Envelope;
    body?: unknown;
  }) {
    super(init.message);
    this.name = 'ApiError';
    this.status = init.status;
    this.code = init.code;
    this.retryable = init.retryable;
    this.operationId = init.operationId;
    this.details = init.details;
    this.envelope = init.envelope;
    this.body = init.body;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function parseError(status: number, body: unknown): ApiError {
  if (isRecord(body) && isRecord(body.error) && typeof body.error.code === 'string') {
    const error = body.error;
    return new ApiError({
      status,
      code: error.code as string,
      message: typeof error.message === 'string' ? error.message : '',
      retryable: error.retryable === true,
      operationId: typeof error.operation_id === 'string' ? error.operation_id : null,
      details: isRecord(error.details) ? error.details : {},
      envelope: 'management',
      body,
    });
  }
  if (isRecord(body) && typeof body.code === 'string') {
    return new ApiError({
      status,
      code: body.code,
      message: typeof body.message === 'string' ? body.message : '',
      retryable: body.retryable === true,
      operationId: null,
      details: {},
      envelope: 'router',
      body,
    });
  }
  return new ApiError({
    status,
    code: null,
    message: `The server answered HTTP ${status}.`,
    retryable: false,
    operationId: null,
    details: {},
    envelope: 'none',
    body,
  });
}

// SPEC R9: a browser without a valid session sees the sign-in instructions.
export function signedOut(): void {
  navigate('/sign-in', { replace: true });
}

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  body?: unknown;
  headers?: Record<string, string>;
  signal?: AbortSignal;
  // The session exchange answers 401 for a used or expired link and shows
  // that itself; every other 401 means the session is gone.
  signInOn401?: boolean;
}

export function uiHeaders(extra: Record<string, string> = {}): Record<string, string> {
  return { [UI_HEADER]: '1', ...extra };
}

export async function send(path: string, options: RequestOptions = {}): Promise<Response> {
  const headers = uiHeaders({ Accept: 'application/json', ...options.headers });
  let body: string | undefined;
  if (options.body !== undefined) {
    // ADR 0040 (plan KTD5): a request with a body declares JSON.
    headers['Content-Type'] = 'application/json';
    body = JSON.stringify(options.body);
  }
  const response = await fetch(path, {
    method: options.method ?? (body === undefined ? 'GET' : 'POST'),
    headers,
    body,
    credentials: 'same-origin',
    cache: 'no-store',
    signal: options.signal,
  });
  if (response.status === 401 && options.signInOn401 !== false) {
    signedOut();
  }
  return response;
}

export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const response = await send(path, options);
  const raw = await response.text();
  let parsed: unknown = undefined;
  if (raw.length > 0) {
    try {
      parsed = JSON.parse(raw);
    } catch {
      parsed = undefined;
    }
  }
  if (!response.ok) {
    throw parseError(response.status, parsed);
  }
  return parsed as T;
}
