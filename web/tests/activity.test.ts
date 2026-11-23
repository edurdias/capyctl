// ADR 0040 (plan U13): Activity pages through the management event history.
// Its own file: the live snapshot it starts from is module state.
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { cursor, managementError, StreamMock } from './helpers';
import { button, deployment, DEPLOYMENT_ID, open, role, setUp, snapshotWith, tearDown, tick } from './fixtures';

beforeEach(setUp);
afterEach(tearDown);

describe('Activity', () => {
  const EVENTS = '/management/v1/events';

  function page(stream: StreamMock, from: number, through: number) {
    for (let sequence = from; sequence <= through; sequence += 1) {
      stream.event(cursor(sequence), 'ready_committed', DEPLOYMENT_ID);
    }
  }

  // SPEC R14: docs/operations/data-retention.md.
  it('states that older events are not kept once it reaches the retention boundary', async () => {
    const requests: Array<string | null> = [];
    role({
      snapshots: [snapshotWith([deployment()], 120)],
      route: (call) => {
        if (call.url !== EVENTS) {
          return undefined;
        }
        const after = call.headers.get('Last-Event-ID');
        if (after === cursor(120)) {
          // The shell's live stream.
          return new StreamMock(call.signal).response;
        }
        requests.push(after);
        if (after === cursor(70)) {
          const stream = new StreamMock(call.signal);
          page(stream, 71, 120);
          return stream.response;
        }
        if (after === cursor(20)) {
          return managementError(410, 'cursor_expired');
        }
        if (after === null) {
          // Pruned below 60: the oldest kept event is 60.
          const stream = new StreamMock(call.signal);
          page(stream, 60, 75);
          return stream.response;
        }
        throw new Error(`unexpected cursor ${after}`);
      },
    });
    await open('/activity');
    await tick();
    expect(requests).toEqual([cursor(70)]);
    expect(document.querySelectorAll('.events li')).toHaveLength(50);
    expect(document.body.textContent).not.toContain('Older events are not kept');

    button('Show older events').click();
    await tick();
    expect(requests).toEqual([cursor(70), cursor(20), null]);
    // 71..120, then 60..70 from the oldest kept event.
    expect(document.querySelectorAll('.events li')).toHaveLength(61);
    expect(document.body.textContent).toContain('Older events are not kept');
    expect(document.body.textContent).not.toContain('Show older events');
  });

  // SPEC R14: a short or failed stream is not a completed history page.
  it.each(['ended', 'terminal error'])('retries the same page without skipping after its stream %s', async (ending) => {
    const requests: Array<string | null> = [];
    role({
      snapshots: [snapshotWith([deployment()], 120)],
      route: (call) => {
        if (call.url !== EVENTS || call.headers.get('Last-Event-ID') === cursor(120)) {
          return undefined;
        }
        const after = call.headers.get('Last-Event-ID');
        requests.push(after);
        const stream = new StreamMock(call.signal);
        if (requests.length === 1) {
          page(stream, 71, 85);
          if (ending === 'terminal error') {
            stream.raw('event: management_stream_error\ndata: {"api_version":"1","error":{"code":"internal","resnapshot_required":false,"retryable":true}}\n\n');
          }
          stream.end();
        } else if (after === cursor(70)) {
          page(stream, 71, 120);
        } else if (after === cursor(20)) {
          page(stream, 21, 70);
        } else {
          throw new Error(`unexpected cursor ${after}`);
        }
        return stream.response;
      },
    });
    await open('/activity');
    await tick();
    expect(requests).toEqual([cursor(70)]);
    expect(document.querySelectorAll('.events li')).toHaveLength(0);
    expect(document.querySelector('[role="alert"]')).not.toBeNull();
    expect(document.body.textContent).not.toContain('Older events are not kept');

    button('Show older events').click();
    await tick();
    expect(requests).toEqual([cursor(70), cursor(70)]);
    expect(document.querySelectorAll('.events li')).toHaveLength(50);
    expect(document.querySelector('[role="alert"]')).toBeNull();

    button('Show older events').click();
    await tick();
    expect(requests).toEqual([cursor(70), cursor(70), cursor(20)]);
    expect(document.querySelectorAll('.events li')).toHaveLength(100);
  });
});
