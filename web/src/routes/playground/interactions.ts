// SPEC R30–R31, R35–R36 (ADR 0040 §12): only the transcript scrolls.
// Follow streamed content until the reader scrolls away from the latest answer.
export function followContent(node: HTMLElement, revision: unknown) {
  let following = true;
  let active = true;
  function scroll(): void {
    following = node.scrollHeight - node.scrollTop - node.clientHeight < 48;
  }
  function update(revision: unknown): void {
    void revision; // The revision signals a DOM update; its shape is immaterial.
    // Svelte actions update before the DOM has finished updating.
    queueMicrotask(() => {
      if (active && following) node.scrollTop = node.scrollHeight;
    });
  }
  node.addEventListener('scroll', scroll, { passive: true });
  const resize = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => update(null));
  resize?.observe(node);
  update(revision);
  return {
    update,
    destroy() { active = false; node.removeEventListener('scroll', scroll); resize?.disconnect(); },
  };
}

// SPEC R30, R35–R36: one line at rest, at most five visible input lines.
export function autoGrow(node: HTMLTextAreaElement, value: string) {
  function update(value: string): void {
    void value; // Bound-value changes, including clearing after Send, resize.
    queueMicrotask(() => {
      node.style.height = 'auto';
      const style = getComputedStyle(node);
      const line = Number.parseFloat(style.lineHeight) || 21;
      const padding = Number.parseFloat(style.paddingTop) + Number.parseFloat(style.paddingBottom) || 16;
      node.style.height = `${Math.min(node.scrollHeight, line * 5 + padding + 2)}px`;
    });
  }
  node.addEventListener('input', resize);
  function resize(): void { update(node.value); }
  update(value);
  return { update, destroy() { node.removeEventListener('input', resize); } };
}

// SPEC R30, R35–R36: Cmd+Enter (Ctrl+Enter off a Mac) sends from any device,
// a phone or tablet with a hardware keyboard included. Plain Enter also sends
// with a fine pointer; on a touch keyboard it stays a newline, and Shift+Enter
// is always a newline. An IME composition is never interrupted.
export function submitOnEnter(event: KeyboardEvent): void {
  if (event.key !== 'Enter' || event.isComposing || event.shiftKey) return;
  const shortcut = event.metaKey || event.ctrlKey;
  if (!shortcut && window.matchMedia?.('(pointer: coarse)').matches) return;
  const field = event.currentTarget;
  if (!(field instanceof HTMLTextAreaElement || field instanceof HTMLInputElement)) return;
  event.preventDefault();
  field.form?.requestSubmit();
}
