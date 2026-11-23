// ADR 0040 §12 (SPEC R32): playground conversations live only in this
// browser's localStorage, one per deployment. CapyCTL stores no prompt or
// response bodies.

export interface Timings {
  // Client-measured, from send to the first generated token (reasoning
  // included).
  ttft_ms: number;
  // The engine's completion token count per second after the first
  // generated token; null when the engine sends no count.
  tokens_per_second: number | null;
}

export interface ChatMessage {
  role: 'user' | 'assistant';
  content: string;
  timings?: Timings;
}

const PREFIX = 'capyctl.playground.';

function key(deploymentId: string): string {
  return `${PREFIX}${deploymentId}`;
}

function isMessage(value: unknown): value is ChatMessage {
  if (typeof value !== 'object' || value === null) {
    return false;
  }
  const message = value as Record<string, unknown>;
  return (message.role === 'user' || message.role === 'assistant') && typeof message.content === 'string';
}

export function loadConversation(deploymentId: string): ChatMessage[] {
  let raw: string | null;
  try {
    raw = window.localStorage.getItem(key(deploymentId));
  } catch {
    return [];
  }
  if (raw === null) {
    return [];
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter(isMessage) : [];
  } catch {
    return [];
  }
}

// Returns false when the browser would not keep it (storage full or
// disabled); the conversation goes on unsaved.
export function saveConversation(deploymentId: string, messages: ChatMessage[]): boolean {
  try {
    window.localStorage.setItem(key(deploymentId), JSON.stringify(messages));
    return true;
  } catch {
    return false;
  }
}

export function deleteConversation(deploymentId: string): void {
  try {
    window.localStorage.removeItem(key(deploymentId));
  } catch {
    // Storage unavailable: there is nothing kept to delete.
  }
}
