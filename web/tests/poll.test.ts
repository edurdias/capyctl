// ADR 0040, plan U13 / Success Criteria: event-driven polling stays serial
// and stops when its owning screen is destroyed (SPEC R11).
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { livePoll } from '../src/lib/poll';
import { startLiveState } from '../src/lib/state';
import { cursor } from './helpers';
import { role, setUp, snapshotWith, tearDown, tick } from './fixtures';

let stopState: (() => void) | undefined;
let stopPoll: (() => void) | undefined;

beforeEach(setUp);
afterEach(() => {
  stopPoll?.();
  stopState?.();
  stopPoll = undefined;
  stopState = undefined;
  tearDown();
});

async function liveEvents() {
  const server = role({ snapshots: [snapshotWith([])] });
  stopState = startLiveState();
  await tick();
  return server.streams[0];
}

describe('live polling', () => {
  it('never overlaps reads and coalesces interval ticks and an event burst into one follow-up', async () => {
    const stream = await liveEvents();
    const pending: Array<() => void> = [];
    let active = 0;
    let peak = 0;
    const read = vi.fn(async () => {
      active += 1;
      peak = Math.max(peak, active);
      await new Promise<void>((resolve) => { pending.push(resolve); });
      active -= 1;
    });
    stopPoll = livePoll(read, 100);
    stream.event(cursor(6), 'ready_committed');
    stream.event(cursor(7), 'ready_committed');
    await tick(1000);
    expect(read).toHaveBeenCalledTimes(1);
    expect(active).toBe(1);

    pending.shift()!();
    await tick();
    expect(read).toHaveBeenCalledTimes(2);
    expect(peak).toBe(1);
    stopPoll();
    pending.shift()!();
    await tick(1000);
    expect(read).toHaveBeenCalledTimes(2);
    expect(active).toBe(0);
  });

  it('does not run the queued follow-up after being destroyed during a read', async () => {
    const stream = await liveEvents();
    let finish!: () => void;
    const read = vi.fn(() => new Promise<void>((resolve) => { finish = resolve; }));
    stopPoll = livePoll(read, 100);
    stream.event(cursor(6), 'ready_committed');
    await tick(500);
    expect(read).toHaveBeenCalledTimes(1);

    stopPoll();
    finish();
    stream.event(cursor(7), 'ready_committed');
    await tick(1000);
    expect(read).toHaveBeenCalledTimes(1);
  });

  it('clears its event timer and unsubscribes when destroyed', async () => {
    const stream = await liveEvents();
    const read = vi.fn(async () => {});
    stopPoll = livePoll(read, 1000);
    await tick();
    stream.event(cursor(6), 'ready_committed');
    await tick(499);
    stopPoll();
    stream.event(cursor(7), 'ready_committed');
    await tick(5000);
    expect(read).toHaveBeenCalledTimes(1);
  });
});
