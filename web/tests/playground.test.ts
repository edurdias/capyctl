// ADR 0040 §12 (plan U17, SPEC R29–R32, AE7, AE8): the playground, driven
// through the mounted App against the router's chat and refusal shapes.
import { flushSync } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { loadConversation, saveConversation, type ChatMessage } from '../src/lib/history';
import { button, deployment, DEPLOYMENT_ID, open, role, setUp, snapshotWith, tearDown, tick } from './fixtures';
import { json, StreamMock } from './helpers';

const CHAT = '/management/v1/playground/chat/completions';
const KEY = `capyctl.playground.${DEPLOYMENT_ID}`;

beforeEach(() => {
  setUp();
  window.localStorage.clear();
});
afterEach(tearDown);

function chunk(content: string): string {
  return `data: ${JSON.stringify({ choices: [{ delta: { content }, finish_reason: null, index: 0 }], object: 'chat.completion.chunk' })}\n\n`;
}

// A role whose playground chat answers with a stream the test writes into.
function chatRole(observed: 'ready' | 'parked' = 'ready') {
  const answers: StreamMock[] = [];
  const server = role({
    snapshots: [snapshotWith([deployment({ observed_state: observed })])],
    route: (call) => {
      if (call.url === CHAT) {
        const stream = new StreamMock(call.signal);
        answers.push(stream);
        return stream.response;
      }
      return undefined;
    },
  });
  return { ...server, answers };
}

async function say(text: string): Promise<void> {
  const box = document.querySelector<HTMLTextAreaElement>('#playground-message')!;
  box.value = text;
  box.dispatchEvent(new Event('input'));
  flushSync();
  button('Send').click();
  await tick();
}

function messages(): string[] {
  return Array.from(document.querySelectorAll('.messages .content')).map((p) => p.textContent ?? '');
}

describe('playground chat', () => {
  // SPEC R30, AE7
  it('records time to first token as the delay to the first content chunk', async () => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    expect(JSON.parse(server.calls.find((c) => c.url === CHAT)!.body!)).toEqual({
      model: 'chat',
      messages: [{ role: 'user', content: 'Hello' }],
      stream: true,
    });

    await tick(300);
    // The role chunk carries no content and does not count.
    server.answers[0].raw(chunk(''));
    await tick(400);
    server.answers[0].raw(chunk('Hi'));
    await tick(500);
    server.answers[0].raw(chunk(' there'));
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();

    expect(messages()).toEqual(['Hello', 'Hi there']);
    expect(document.querySelector('.timings')?.textContent).toContain('First token 0.70 s');
    expect(document.querySelector('.timings')?.textContent).toContain('2.0 tokens/s');
  });

  // SPEC R32, AE8
  it('reloads a saved conversation identically', async () => {
    const conversation: ChatMessage[] = [
      { role: 'user', content: 'Hello' },
      { role: 'assistant', content: 'Hi\nthere', timings: { ttft_ms: 700, tokens_per_second: 2 } },
    ];
    expect(saveConversation(DEPLOYMENT_ID, conversation)).toBe(true);
    expect(loadConversation(DEPLOYMENT_ID)).toEqual(conversation);

    chatRole();
    await open('/playground');
    expect(messages()).toEqual(['Hello', 'Hi\nthere']);
  });

  it('keeps a streamed exchange across a reload', async () => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    server.answers[0].raw(chunk('Hi'));
    server.answers[0].end();
    await tick();
    tearDown();
    setUp();

    chatRole();
    await open('/playground');
    expect(messages()).toEqual(['Hello', 'Hi']);
  });

  // SPEC R32
  it('deletes a conversation from localStorage', async () => {
    saveConversation(DEPLOYMENT_ID, [{ role: 'user', content: 'Hello' }]);
    chatRole();
    await open('/playground');
    expect(window.localStorage.getItem(KEY)).not.toBeNull();

    button('Delete conversation').click();
    await tick();
    expect(window.localStorage.getItem(KEY)).toBeNull();
    expect(messages()).toEqual([]);
  });

  it('says so when localStorage is full and still chats without saving', async () => {
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new DOMException('full', 'QuotaExceededError');
    });
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    server.answers[0].raw(chunk('Hi'));
    server.answers[0].end();
    await tick();

    expect(document.body.textContent).toContain('storage is full, so this conversation is not being saved');
    expect(messages()).toEqual(['Hello', 'Hi']);
    expect(window.localStorage.getItem(KEY)).toBeNull();
  });

  // ADR 0040 §12 (plan KTD18): model output is text, never HTML.
  it('renders model output containing HTML as literal text', async () => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    const html = '<img src="x" onerror="alert(1)"><b>bold</b>';
    server.answers[0].raw(chunk(html));
    await tick();
    expect(document.querySelector('.messages img, .messages b')).toBeNull();
    server.answers[0].end();
    await tick();

    expect(document.querySelector('.messages img, .messages b')).toBeNull();
    expect(messages()).toContain(html);
  });

  // SPEC R29
  it('shows Start after a deployment_stopped refusal', async () => {
    const server = role({
      snapshots: [snapshotWith([deployment()])],
      route: (call) =>
        call.url === CHAT
          ? json({ code: 'deployment_stopped', message: 'model chat is stopped', retryable: false }, 409)
          : undefined,
    });
    await open('/playground');
    expect(document.body.textContent).not.toContain('is stopped. Start it');
    await say('Hello');

    expect(document.body.textContent).toContain('The model is stopped.');
    button('Start').click();
    await tick();
    const start = server.posts().find((c) => c.url === `/management/v1/deployments/${DEPLOYMENT_ID}/actions`);
    expect(JSON.parse(start!.body!)).toMatchObject({ action: 'start', expected_revision: 3 });
  });

  // SPEC R29, AE7: the pending answer reads "Waking model…" until the first
  // chunk, and Send stays disabled until the response ends.
  it('shows Waking model… for a parked model until the first chunk, with Send disabled until the end', async () => {
    const server = chatRole('parked');
    await open('/playground');
    await say('Hello');

    expect(document.body.textContent).toContain('Waking model…');
    expect(button('Send').disabled).toBe(true);

    server.answers[0].raw(chunk('Hi'));
    await tick();
    expect(document.body.textContent).not.toContain('Waking model…');
    expect(button('Send').disabled).toBe(true);

    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();
    expect(button('Send').disabled).toBe(false);
  });
});
