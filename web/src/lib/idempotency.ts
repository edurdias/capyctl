// ADR 0040 (plan KTD16): idempotency keys come from crypto.getRandomValues.
// crypto.randomUUID exists only in secure contexts, and a role served over
// plain HTTP on a non-loopback address (a Tailscale address, say) is not one.
// 128 random bits as 32 lowercase hex characters, which both management key
// grammars accept (configuration.rs `identifier`, drain.rs `valid_key`).
export function newIdempotencyKey(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
}
