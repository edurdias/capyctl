// T16 / R18: confirmation remains keyboard-accessible and instance-local.
import { mount, unmount } from 'svelte';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import ConfirmDialog from '../src/components/ConfirmDialog.svelte';
import { setUp, tearDown, tick } from './fixtures';

const mounted: Record<string, unknown>[] = [];
beforeEach(setUp);
afterEach(() => { mounted.forEach((component) => unmount(component)); mounted.length = 0; tearDown(); });

it('gives every confirmation its own title and wraps keyboard focus', async () => {
  for (const title of ['Delete first', 'Delete second']) {
    mounted.push(mount(ConfirmDialog, { target: document.body, props: {
      open: true, title, confirmLabel: 'Delete', onconfirm: vi.fn(), oncancel: vi.fn(),
    } }));
  }
  await tick();
  const dialogs = [...document.querySelectorAll('dialog')];
  expect(dialogs[0].getAttribute('aria-labelledby')).not.toBe(dialogs[1].getAttribute('aria-labelledby'));
  for (const dialog of dialogs) {
    expect(document.getElementById(dialog.getAttribute('aria-labelledby')!)?.textContent).toContain('Delete');
    const buttons = dialog.querySelectorAll('button');
    buttons[1].focus();
    buttons[1].dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }));
    expect(document.activeElement).toBe(buttons[0]);
    buttons[0].dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true }));
    expect(document.activeElement).toBe(buttons[1]);
  }
});

it('routes browser Escape cancellation through the owner without submitting', async () => {
  const oncancel = vi.fn();
  const onconfirm = vi.fn();
  mounted.push(mount(ConfirmDialog, { target: document.body, props: {
    open: true, title: 'Delete', confirmLabel: 'Delete', onconfirm, oncancel,
  } }));
  await tick();
  // Browsers dispatch cancel for Escape; jsdom has no native dialog keyboard default.
  const event = new Event('cancel', { cancelable: true });
  document.querySelector('dialog')!.dispatchEvent(event);
  expect(oncancel).toHaveBeenCalledOnce();
  expect(onconfirm).not.toHaveBeenCalled();
  expect(event.defaultPrevented).toBe(true);
});
