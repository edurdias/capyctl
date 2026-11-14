import { writable, type Readable } from 'svelte/store';

// Path-based routing under the base the role serves the UI from
// (plan KTD1: `/ui/`). The role answers every extension-less path under it
// with index.html, so a reload on any screen lands back here.
export const BASE = '/ui';

function currentPath(): string {
  const path = window.location.pathname;
  if (path === BASE || path === `${BASE}/`) {
    return '/';
  }
  if (path.startsWith(`${BASE}/`)) {
    return path.slice(BASE.length).replace(/\/+$/, '') || '/';
  }
  return '/';
}

const path = writable(currentPath());

export const route: Readable<string> = { subscribe: path.subscribe };

export function href(to: string): string {
  return to === '/' ? `${BASE}/` : `${BASE}${to}`;
}

export function navigate(to: string, options: { replace?: boolean } = {}): void {
  const url = href(to);
  if (options.replace) {
    window.history.replaceState(null, '', url);
  } else {
    window.history.pushState(null, '', url);
  }
  path.set(currentPath());
}

window.addEventListener('popstate', () => path.set(currentPath()));

// Same-document navigation for in-app links; modified clicks keep the
// browser's own behavior (new tab, new window).
export function link(node: HTMLAnchorElement): { destroy(): void } {
  const onClick = (event: MouseEvent) => {
    if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
      return;
    }
    const target = node.getAttribute('href');
    if (!target || !target.startsWith(`${BASE}/`)) {
      return;
    }
    event.preventDefault();
    navigate(target.slice(BASE.length) || '/');
  };
  node.addEventListener('click', onClick);
  return { destroy: () => node.removeEventListener('click', onClick) };
}
