// ADR 0040 (plan KTD16): the Clipboard API exists only in secure contexts,
// and a role served over plain HTTP on a non-loopback address is not one.
// Without it, the text is selected so the owner can copy it by hand.
export type CopyOutcome = 'copied' | 'selected';

export async function copyText(text: string, element: HTMLElement): Promise<CopyOutcome> {
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return 'copied';
    } catch {
      // Permission refused: fall through to selecting the text.
    }
  }
  const selection = window.getSelection();
  if (selection) {
    const range = document.createRange();
    range.selectNodeContents(element);
    selection.removeAllRanges();
    selection.addRange(range);
  }
  return 'selected';
}
