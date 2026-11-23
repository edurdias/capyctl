import { vi } from 'vitest';
import type { Snapshot } from '../src/lib/types';

export interface Call {
  url: string;
  method: string;
  headers: Headers;
  body: string | null;
  signal: AbortSignal | null;
  // The address bar's fragment when the request was sent.
  hash: string;
}

export type Handler = (call: Call) => Response | Promise<Response>;

// Replaces fetch with `handler` and records every request.
export function installFetch(handler: Handler): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
      const call: Call = {
        url: String(input),
        method: init.method ?? 'GET',
        headers: new Headers(init.headers),
        body: typeof init.body === 'string' ? init.body : null,
        signal: init.signal ?? null,
        hash: window.location.hash,
      };
      calls.push(call);
      return handler(call);
    }),
  );
  return calls;
}

export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
}

export function managementError(status: number, code: string, retryable = false): Response {
  return json(
    {
      api_version: '1',
      error: { code, message: `server says ${code}`, retryable, operation_id: null, details: {} },
    },
    status,
  );
}

// A cursor in the server's grammar: `<26-character ULID>:<sequence>`.
export function cursor(sequence: number): string {
  return `01JBX7Q4M2N8P0R5S7T9V1W3XY:${sequence}`;
}

export function snapshot(at: string): Snapshot {
  return {
    api_version: '1',
    scope: 'durable_store_foundation',
    cursor: at,
    ledger_epoch: '1',
    session_epoch: '1',
    deployments: [],
    routes: [],
    operations: [],
    runs: [],
    steps: [],
    claims: [],
    bindings: [],
    reservations: [],
    legacy_reservations: [],
    grants: [],
    host_policy_revisions: [],
    observations: [],
  };
}

// One open SSE response the test writes frames into. Aborting the request
// errors the body, as a browser's fetch does.
export class StreamMock {
  #controller!: ReadableStreamDefaultController<Uint8Array>;
  #encoder = new TextEncoder();
  readonly response: Response;

  constructor(signal: AbortSignal | null) {
    const body = new ReadableStream<Uint8Array>({
      start: (controller) => {
        this.#controller = controller;
      },
    });
    signal?.addEventListener('abort', () => {
      try {
        this.#controller.error(new DOMException('The operation was aborted.', 'AbortError'));
      } catch {
        // Already closed.
      }
    });
    this.response = new Response(body, { status: 200, headers: { 'content-type': 'text/event-stream' } });
  }

  event(id: string, kind: string, deploymentId: string | null = null): void {
    const data = {
      api_version: '1',
      recorded_at_ms: '1793150000000',
      deployment_id: deploymentId,
      operation_id: null,
      payload: {},
    };
    this.#controller.enqueue(this.#encoder.encode(`id: ${id}\nevent: ${kind}\ndata: ${JSON.stringify(data)}\n\n`));
  }

  raw(text: string): void {
    this.#controller.enqueue(this.#encoder.encode(text));
  }

  // The server ending the stream at its lifetime.
  end(): void {
    this.#controller.close();
  }
}

export function setVisibility(state: 'visible' | 'hidden'): void {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state });
  document.dispatchEvent(new Event('visibilitychange'));
}

// Lets pending promise continuations run without moving the fake clock.
export async function settle(): Promise<void> {
  await vi.advanceTimersByTimeAsync(0);
}
