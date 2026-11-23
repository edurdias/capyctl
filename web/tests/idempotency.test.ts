// ADR 0040 (plan KTD16, U12): idempotency keys work on a plain-HTTP origin.
import { describe, expect, it, vi } from 'vitest';
import { newIdempotencyKey } from '../src/lib/idempotency';

describe('idempotency keys', () => {
  it('are generated from getRandomValues, never crypto.randomUUID', () => {
    // On a plain-HTTP origin other than localhost randomUUID does not exist.
    vi.stubGlobal('crypto', { getRandomValues: crypto.getRandomValues.bind(crypto) });
    const first = newIdempotencyKey();
    const second = newIdempotencyKey();
    expect(first).toMatch(/^[0-9a-f]{32}$/);
    expect(second).toMatch(/^[0-9a-f]{32}$/);
    expect(first).not.toBe(second);
  });
});
