// Owner feedback 2026-09-25: every command the guide shows must exist in the
// built CLI. These helpers find the `mllm` invocations in a page and check
// each against the CLI's own `--help`.

const FENCE = /^(`{3,}|~{3,})(\S*)/;
const SHELL = new Set(['bash', 'sh', 'shell', 'console']);

// Split a shell line into words, keeping quoted strings whole. Enough for
// the commands a guide shows; it does not expand anything.
export function words(line) {
  const out = [];
  let cur = '';
  let quote = null;
  let started = false;
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = null; else cur += ch;
    } else if (ch === '"' || ch === "'") {
      quote = ch; started = true;
    } else if (/\s/.test(ch)) {
      if (started) { out.push(cur); cur = ''; started = false; }
    } else {
      cur += ch; started = true;
    }
  }
  if (started) out.push(cur);
  return out;
}

// The `mllm` invocations in one shell line: after `sudo`, environment
// assignments, `$(`, `|`, `;` or `&&`, and before a comment.
export function invocations(line) {
  const text = line.replace(/^\s*\$\s+/, '').replace(/\s+#.*$/, '');
  const found = [];
  for (const part of text.split(/\|\||&&|[;|]|\$\(/)) {
    const w = words(part.trim());
    while (w.length && (w[0] === 'sudo' || /^[A-Z_][A-Z0-9_]*=/.test(w[0]))) w.shift();
    if (w[0] === 'mllm') found.push(w.slice(1).map((x) => x.replace(/\)+$/, '')).filter(Boolean));
  }
  return found;
}

// Every `mllm` invocation a Markdown page shows: in shell code blocks (with
// `\` continuations joined) and in inline code spans.
export function pageCommands(md) {
  const found = [];
  let open = null;
  let lang = '';
  let pending = '';
  for (const line of md.split('\n')) {
    const f = line.match(FENCE);
    if (f && !open) { open = f[1]; lang = f[2]; continue; }
    if (f && open && line.trim() === open) { open = null; continue; }
    if (open) {
      if (!SHELL.has(lang)) continue;
      const joined = pending + line;
      if (/\\\s*$/.test(line)) { pending = joined.replace(/\\\s*$/, ' '); continue; }
      pending = '';
      found.push(...invocations(joined));
    } else {
      for (const m of line.matchAll(/`([^`]+)`/g)) {
        if (/^(sudo\s+|[A-Z_]+=\S+\s+)*mllm\s/.test(m[1])) found.push(...invocations(m[1]));
      }
    }
  }
  return found;
}

// The subcommand names and long options a `--help` text lists.
export function parseHelp(text) {
  const commands = new Set();
  const options = new Set();
  let section = '';
  for (const line of text.split('\n')) {
    if (/^\S.*:$/.test(line)) { section = line; continue; }
    if (section === 'Commands:') {
      const m = line.match(/^ {2}([a-z][a-z0-9-]*)\b/);
      if (m) commands.add(m[1]);
    }
    const o = line.match(/^\s+(?:-[A-Za-z], )?(--[a-z][a-z0-9-]*)/);
    if (o && section === 'Options:') options.add(o[1]);
  }
  return { commands, options };
}

// Check one invocation with `help(path)`, which returns the `--help` text of
// `mllm <path...>`. Returns the problems found.
export function checkInvocation(args, help) {
  const path = [];
  let info = parseHelp(help(path));
  let i = 0;
  while (i < args.length && info.commands.has(args[i])) {
    path.push(args[i]);
    info = parseHelp(help(path));
    i++;
  }
  const problems = [];
  const shown = `mllm ${args.join(' ')}`;
  const informational = args.some((a) => ['--help', '-h', '--version', '-V'].includes(a));
  if (!path.length && !informational) {
    problems.push(`${shown}: "${args[0]}" is not a command`);
  } else if (info.commands.size && !informational) {
    problems.push(`${shown}: "mllm ${path.join(' ')}" needs one of ${[...info.commands].join(', ')}`);
  }
  for (const a of args.slice(i)) {
    if (!a.startsWith('--')) continue;
    const flag = a.split('=')[0];
    if (flag === '--help' || flag === '--version') continue;
    if (!info.options.has(flag)) problems.push(`${shown}: "mllm ${path.join(' ')}" has no ${flag}`);
  }
  return problems;
}
