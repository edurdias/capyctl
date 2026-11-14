// ADR 0040 §12 (plan U17, SPEC R29–R32, AE7, AE8): the playground, driven
// through the mounted App against the router's chat and refusal shapes.
import { flushSync } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { loadConversation, saveConversation, type ChatMessage } from '../src/lib/history';
import { streamChat } from '../src/lib/playground';
import { navigate } from '../src/lib/router';
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
      stream_options: { include_usage: true },
    });

    await tick(300);
    // The role chunk carries no content and does not count.
    server.answers[0].raw(chunk(''));
    await tick(400);
    server.answers[0].raw(chunk('Hi'));
    await tick(1000);
    server.answers[0].raw(chunk(' there'));
    server.answers[0].raw('data: {"choices":[],"usage":{"completion_tokens":21}}\n\n');
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();

    expect(messages()).toEqual(['Hello', 'Hi there']);
    expect(document.querySelector('.timings')?.textContent).toContain('First token 0.70 s');
    expect(document.querySelector('.timings')?.textContent).toContain('20.0 tokens/s');
  });

  // SPEC R30, AE7: a reasoning model streams its reasoning before its answer,
  // and the engine's token count includes it. Timing from the first answer
  // chunk divided every token by the answer's time alone (found live: 600
  // tokens/s shown for a Qwen3 stream decoding at 63).
  it('times tokens/s and first token from the first reasoning token', async () => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    const reasoning = (text: string) => `data: ${JSON.stringify({ choices: [{ delta: { reasoning_content: text }, index: 0 }] })}\n\n`;

    await tick(300);
    server.answers[0].raw(reasoning('Let me think'));
    await tick(900);
    server.answers[0].raw(reasoning(' about it.'));
    await tick(100);
    server.answers[0].raw(chunk('Hi'));
    server.answers[0].raw(chunk(' there'));
    server.answers[0].raw('data: {"choices":[],"usage":{"completion_tokens":21}}\n\n');
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();

    // Reasoning is timed but not shown as the answer.
    expect(messages()).toEqual(['Hello', 'Hi there']);
    expect(document.querySelector('.timings')?.textContent).toContain('First token 0.30 s');
    expect(document.querySelector('.timings')?.textContent).toContain('20.0 tokens/s');
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
    server.answers[0].raw('data: [DONE]\n\n');
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
    expect(window.localStorage.getItem(KEY)).not.toBeNull();
    expect(document.querySelector('dialog[open]')?.textContent).toContain('Delete conversation?');
    button('Cancel').click();
    await tick();
    expect(messages()).toEqual(['Hello']);
    button('Delete conversation').click();
    await tick();
    button('Delete', document.querySelector('dialog[open]')!).click();
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
    server.answers[0].raw('data: [DONE]\n\n');
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
    server.answers[0].raw('data: [DONE]\n\n');
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

  // SPEC R29, AE7: show the snapshot state and elapsed waiting time.
  it('shows the waking model and elapsed seconds, with Send disabled until the end', async () => {
    const server = chatRole('parked');
    await open('/playground');
    await say('Hello');

    await tick(12000);
    expect(document.querySelector('.pending')?.textContent).toContain('Waking chat · parked · 12 s');
    expect(button('Send').disabled).toBe(true);

    server.answers[0].raw(chunk('Hi'));
    await tick();
    expect(document.querySelector('.pending')).toBeNull();
    expect(button('Send').disabled).toBe(true);

    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();
    expect(button('Send').disabled).toBe(false);
  });

  // SPEC R30, R32, AE7–AE8: EOF cannot turn a partial answer into saved history.
  it.each(['', 'A partial answer'])('rejects EOF without DONE after %j', async (partial) => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    if (partial) {
      server.answers[0].raw(chunk(partial));
      await tick();
      expect(messages()).toContain(partial);
    }
    server.answers[0].end();
    await tick();

    expect(document.querySelector('[role="alert"]')?.textContent).toContain('before it was complete');
    expect(messages()).toEqual(['Hello']);
    expect(loadConversation(DEPLOYMENT_ID)).toEqual([{ role: 'user', content: 'Hello' }]);
    expect(button('Send').disabled).toBe(false);
  });

  // SPEC R30, AE7: token throughput requires reported tokens, not chunk counts.
  it('omits tokens/s when the engine does not report usage', async () => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    server.answers[0].raw(chunk('Several tokens'));
    await tick(500);
    server.answers[0].raw(chunk(' in each chunk'));
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();

    expect(document.querySelector('.timings')?.textContent).toContain('First token');
    expect(document.querySelector('.timings')?.textContent).not.toContain('tokens/s');
    expect(loadConversation(DEPLOYMENT_ID)[1].timings?.tokens_per_second).toBeNull();
  });

  // SPEC R30, AE7: short streams cannot provide a meaningful decode rate.
  it.each([[15, 200], [21, 200], [15, 1000]])('omits tokens/s for %i tokens over %i ms', async (tokens, duration) => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    server.answers[0].raw(chunk('Short'));
    await tick(duration);
    server.answers[0].raw(chunk(' answer'));
    server.answers[0].raw(`data: {"choices":[],"usage":{"completion_tokens":${tokens}}}\n\n`);
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();
    expect(document.querySelector('.timings')?.textContent).toContain('First token');
    expect(document.querySelector('.timings')?.textContent).not.toContain('tokens/s');
  });

  // SPEC R30, R32: ignore outer whitespace from empty thinking blocks only.
  it('trims assistant whitespace while streaming and saving, preserving internal whitespace', async () => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    server.answers[0].raw(chunk('\n\n'));
    server.answers[0].raw(chunk('Hi\n\n  there  \n'));
    await tick();
    expect(messages()).toEqual(['Hello', 'Hi\n\n  there']);
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();
    expect(loadConversation(DEPLOYMENT_ID)[1].content).toBe('Hi\n\n  there');
  });

  // SPEC R30, R35–R36: scroll only messages; the composer stays outside.
  it('keeps the composer outside the scrolling message list and delete in the header', async () => {
    chatRole();
    await open('/playground');
    const scroll = document.querySelector('.playground-scroll')!;
    const composer = document.querySelector('.playground-composer')!;
    expect(scroll.querySelector('.messages')).not.toBeNull();
    expect(scroll.contains(composer)).toBe(false);
    expect(composer.querySelector('textarea')?.rows).toBe(1);
    expect(document.querySelector('.playground-header')?.contains(button('Delete conversation'))).toBe(true);
  });

  // SPEC R30, R35–R36: do not pull the reader away from older messages.
  it('follows new content until the reader scrolls up, and resumes at the bottom', async () => {
    const server = chatRole();
    await open('/playground');
    const scroll = document.querySelector<HTMLElement>('.playground-scroll')!;
    let height = 1000;
    Object.defineProperties(scroll, {
      scrollHeight: { get: () => height },
      clientHeight: { value: 300 },
    });
    await say('Hello');
    expect(scroll.scrollTop).toBe(1000);
    scroll.scrollTop = 100;
    scroll.dispatchEvent(new Event('scroll'));
    height = 1200;
    server.answers[0].raw(chunk('First part'));
    await tick();
    expect(scroll.scrollTop).toBe(100);
    scroll.scrollTop = 900;
    scroll.dispatchEvent(new Event('scroll'));
    height = 1400;
    server.answers[0].raw(chunk(' and more'));
    await tick();
    expect(scroll.scrollTop).toBe(1400);
  });

  // SPEC R30, R35–R36: touch keyboards need Enter for multiline prompts.
  it.each([false, true])('handles Enter with coarse pointer %s', async (coarse) => {
    const matchMedia = vi.fn((query: string) => ({ matches: query === '(pointer: coarse)' && coarse }));
    vi.stubGlobal('matchMedia', matchMedia);
    try {
      const server = chatRole();
      await open('/playground');
      const box = document.querySelector<HTMLTextAreaElement>('#playground-message')!;
      box.value = 'Hello';
      box.dispatchEvent(new Event('input'));
      flushSync();
      const shift = new KeyboardEvent('keydown', { key: 'Enter', shiftKey: true, bubbles: true, cancelable: true });
      box.dispatchEvent(shift);
      expect(shift.defaultPrevented).toBe(false);
      const enter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
      box.dispatchEvent(enter);
      await tick();
      expect(enter.defaultPrevented).toBe(!coarse);
      expect(server.answers).toHaveLength(coarse ? 0 : 1);
      if (coarse) {
        button('Send').click();
        await tick();
        expect(server.answers).toHaveLength(1);
      }
    } finally {
      vi.unstubAllGlobals();
    }
  });

  // SPEC R15, R29–R30: in-band router refusals retain their own envelope.
  it('preserves the code, message, and retryability of an in-band refusal', async () => {
    const server = chatRole();
    const result = streamChat('chat', [], { onContent: () => {} });
    const refused = expect(result).rejects.toMatchObject({
      code: 'deployment_stopped',
      message: 'model chat is stopped',
      retryable: true,
      envelope: 'router',
    });
    await tick();
    server.answers[0].raw('data: {"code":"deployment_stopped","message":"model chat is stopped","retryable":true}\n\n');
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await refused;
  });

  // SPEC R15, R29, R32: a refused stream is not a completed assistant message.
  it('shows an in-band refusal without saving its partial answer', async () => {
    const server = chatRole();
    await open('/playground');
    await say('Hello');
    server.answers[0].raw(chunk('Partial'));
    server.answers[0].raw('data: {"code":"deployment_stopped","message":"model chat is stopped","retryable":false}\n\n');
    server.answers[0].raw('data: [DONE]\n\n');
    server.answers[0].end();
    await tick();

    expect(document.body.textContent).toContain('The model is stopped.');
    expect(button('Start')).toBeDefined();
    expect(loadConversation(DEPLOYMENT_ID)).toEqual([{ role: 'user', content: 'Hello' }]);
    expect(messages()).toEqual(['Hello']);
  });

  // SPEC R15, R30: a failed response body is rendered, not rethrown by the UI.
  it('handles a transport failure while reading the chat stream', async () => {
    role({
      snapshots: [snapshotWith([deployment()])],
      route: (call) => call.url === CHAT
        ? new Response(new ReadableStream({ start(controller) { controller.error(new TypeError('connection lost')); } }))
        : undefined,
    });
    await open('/playground');
    await say('Hello');
    expect(document.querySelector('[role="alert"]')?.textContent).toContain('connection to the model was interrupted');
    expect(button('Send').disabled).toBe(false);
    expect(loadConversation(DEPLOYMENT_ID)).toEqual([{ role: 'user', content: 'Hello' }]);
  });
});

describe('playground transport lifecycle', () => {
  // SPEC R15, R30–R31: fetch failures reach the refusal UI in both modes.
  it.each(['chat', 'decisions'] as const)('handles a %s request transport failure', async (api) => {
    const endpoint = api === 'chat' ? CHAT : '/management/v1/playground/systemone';
    role({
      snapshots: [snapshotWith([deployment(api === 'decisions' ? { api } : {})])],
      route: (call) => call.url === endpoint ? Promise.reject(new TypeError('fetch failed')) : undefined,
    });
    await open('/playground');
    if (api === 'chat') {
      await say('Hello');
    } else {
      button('Ask').click();
      await tick();
    }

    expect(document.querySelector('[role="alert"]')?.textContent).toContain('connection to the model was interrupted');
    expect(button(api === 'chat' ? 'Send' : 'Ask').disabled).toBe(false);
  });

  // SPEC R31: changing pages destroys Decision and cancels its pending request.
  it('aborts an outstanding decision request when leaving the playground', async () => {
    const endpoint = '/management/v1/playground/systemone';
    const server = role({
      snapshots: [snapshotWith([deployment({ api: 'decisions' })])],
      route: (call) => call.url === endpoint ? new StreamMock(call.signal).response : undefined,
    });
    await open('/playground');
    button('Ask').click();
    await tick();
    const request = server.calls.find((call) => call.url === endpoint)!;
    expect(request.signal?.aborted).toBe(false);

    navigate('/models');
    await tick();
    expect(request.signal?.aborted).toBe(true);
    expect(document.querySelector('[role="alert"]')).toBeNull();
  });
});
