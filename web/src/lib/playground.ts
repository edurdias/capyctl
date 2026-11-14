import { createParser } from 'eventsource-parser';
import { ApiError, parseError, request, send } from './api';
import { PLAYGROUND } from './endpoints';
import type { ChatMessage, Timings } from './history';

// ADR 0040 §12: the playground sends the router's own bodies through the
// management forward and reads its answers: OpenAI chat chunks for a chat
// model (docs/guide/requests.md "Streaming"), and the System One answer for
// a decision model (ADR 0030 §2).

export interface StreamHandlers {
  // Each piece of content as it arrives.
  onContent(text: string): void;
  signal?: AbortSignal;
}

export interface ChatResult {
  content: string;
  timings: Timings | null;
}

// A reasoning model (Qwen3 thinking, for one) streams its reasoning before its
// answer in `reasoning_content` (vLLM, SGLang) or `reasoning`. The engine's
// `completion_tokens` counts both, so both are generated tokens for timing.
interface ChunkChoice {
  delta?: { content?: string | null; reasoning_content?: string | null; reasoning?: string | null };
}

interface Chunk {
  choices?: ChunkChoice[];
  usage?: { completion_tokens?: number } | null;
}

function lostAnswer(): ApiError {
  return new ApiError({
    status: 0,
    code: null,
    message: 'The answer stopped before it was complete.',
    retryable: true,
    operationId: null,
    details: {},
    envelope: 'none',
  });
}

// SPEC R15, R30–R31: transport failures belong in the playground's refusal UI.
export function playgroundError(caught: unknown): ApiError {
  return caught instanceof ApiError
    ? caught
    : new ApiError({
        status: 0,
        code: null,
        message: 'The connection to the model was interrupted. Please try again.',
        retryable: true,
        operationId: null,
        details: {},
        envelope: 'none',
      });
}

async function refusal(response: Response): Promise<ApiError> {
  let body: unknown = undefined;
  try {
    body = JSON.parse(await response.text());
  } catch {
    body = undefined;
  }
  return parseError(response.status, body);
}

// SPEC R30: time to first token is measured here, from send to the first
// chunk carrying content, so it includes any wake or start the request
// caused (AE7).
export async function streamChat(model: string, messages: ChatMessage[], handlers: StreamHandlers): Promise<ChatResult> {
  const sentAt = performance.now();
  const response = await send(PLAYGROUND.chat, {
    method: 'POST',
    body: {
      model,
      messages: messages.map(({ role, content }) => ({ role, content })),
      stream: true,
      stream_options: { include_usage: true },
    },
    headers: { Accept: 'text/event-stream' },
    signal: handlers.signal,
  });
  if (!response.ok) {
    throw await refusal(response);
  }
  if (response.body === null) {
    throw lostAnswer();
  }

  let content = '';
  let firstAt: number | null = null;
  let lastAt: number | null = null;
  let reportedTokens: number | null = null;
  let done = false;
  let failure: ApiError | null = null;

  const parser = createParser({
    onEvent: (event) => {
      if (done || failure !== null) {
        return;
      }
      if (event.data === '[DONE]') {
        done = true;
        return;
      }
      let chunk: Chunk & { code?: unknown; error?: unknown };
      try {
        chunk = JSON.parse(event.data) as Chunk;
      } catch {
        return;
      }
      // A refusal after the stream began arrives as an error envelope.
      if (typeof chunk.code === 'string' || (typeof chunk.error === 'object' && chunk.error !== null)) {
        failure = parseError(response.status, chunk);
        return;
      }
      if (typeof chunk.usage?.completion_tokens === 'number') {
        reportedTokens = chunk.usage.completion_tokens;
      }
      const choices = chunk.choices ?? [];
      const text = choices.map((choice) => choice.delta?.content ?? '').join('');
      const reasoned = choices.some((choice) => (choice.delta?.reasoning_content ?? choice.delta?.reasoning ?? '') !== '');
      if (text.length === 0 && !reasoned) {
        return;
      }
      // SPEC R30: time to first token and the decode rate both start at the
      // first generated token, reasoning included, so the rate divides the
      // engine's token count by the time that produced all of them.
      const now = performance.now();
      firstAt ??= now;
      lastAt = now;
      if (text.length === 0) {
        return;
      }
      content += text;
      handlers.onContent(text);
    },
  });

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  for (;;) {
    const { value, done: ended } = await reader.read();
    if (ended) {
      break;
    }
    parser.feed(decoder.decode(value, { stream: true }));
  }
  if (failure !== null) {
    throw failure;
  }
  // SPEC R30, R32: EOF is not completion; never save a truncated answer.
  if (!done) {
    throw lostAnswer();
  }
  return { content: content.trim(), timings: timings(sentAt, firstAt, lastAt, reportedTokens) };
}

function timings(sentAt: number, firstAt: number | null, lastAt: number | null, tokens: number | null): Timings | null {
  if (firstAt === null || lastAt === null) {
    return null;
  }
  const generating = (lastAt - firstAt) / 1000;
  return {
    ttft_ms: firstAt - sentAt,
    // SPEC R30: SSE chunks are not tokens. Short samples amplify transport
    // batching and tokens with no visible delta (e.g. an empty think block),
    // so show a rate only with >=16 reported tokens over >=0.5 s of decode.
    tokens_per_second: tokens !== null && generating >= 0.5 && tokens >= 16 ? (tokens - 1) / generating : null,
  };
}

// docs/guide/requests.md "Decision models": one question of each type.
export type QuestionType = 'noul' | 'choice' | 'score';

export interface DecisionQuestion {
  type: QuestionType;
  instructions: string;
  // `choice`: option names; `score`: levels from low to high.
  options: string[];
}

export interface DecisionAnswer {
  type: string;
  // The pick: an option for `choice`, the expected level for `score`, the
  // probability of yes for `noul`.
  value: string;
  probabilities: { label: string; probability: number }[];
}

export interface DecisionResult {
  answers: Record<string, DecisionAnswer>;
  total_ms: number;
}

function questionBody(question: DecisionQuestion): Record<string, unknown> {
  const body: Record<string, unknown> = { type: question.type, instructions: question.instructions };
  if (question.type === 'choice') {
    body.criteria = Object.fromEntries(question.options.map((option) => [option, null]));
  } else if (question.type === 'score') {
    body.criteria = question.options;
  }
  return body;
}

// The engine's answer to one question (docs/guide/requests.md): the pick is
// under the key named by its type.
interface RawAnswer {
  type?: string;
  noul?: number;
  probabilities?: Record<string, number>;
  legend?: Record<string, string>;
  [pick: string]: unknown;
}

function answerOf(answer: RawAnswer): DecisionAnswer {
  const type = typeof answer.type === 'string' ? answer.type : 'unknown';
  const legend = answer.legend ?? {};
  const probabilities = Object.entries(answer.probabilities ?? {})
    .filter((entry): entry is [string, number] => typeof entry[1] === 'number')
    .map(([label, probability]) => ({ label: legend[label] ?? label, probability }));
  if (type === 'noul' && typeof answer.noul === 'number') {
    return {
      type,
      value: answer.noul >= 0.5 ? 'yes' : 'no',
      probabilities: [
        { label: 'yes', probability: answer.noul },
        { label: 'no', probability: 1 - answer.noul },
      ],
    };
  }
  const pick = answer[type];
  return { type, value: pick === undefined ? '' : String(pick), probabilities };
}

// SPEC R31: a typed question to `/v1/systemone`; the answer is never
// streamed.
export async function decide(model: string, state: string, question: DecisionQuestion, signal?: AbortSignal): Promise<DecisionResult> {
  const sentAt = performance.now();
  const answer = await request<{ answers?: Record<string, RawAnswer> }>(PLAYGROUND.decisions, {
    method: 'POST',
    body: { model, state, questions: { question: questionBody(question) } },
    signal,
  });
  const answers = Object.fromEntries(Object.entries(answer?.answers ?? {}).map(([id, raw]) => [id, answerOf(raw)]));
  return { answers, total_ms: performance.now() - sentAt };
}
