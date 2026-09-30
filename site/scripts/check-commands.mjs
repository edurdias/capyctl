// Owner feedback 2026-09-25: every `capyctl` command the guide and the landing
// page show must exist in the CLI built from this tree: each subcommand and
// each long option is looked up in that binary's own `--help`.
import { execFileSync } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { checkInvocation, invocations, pageCommands } from './lib/commands.mjs';
import { HERO_COMMAND } from '../src/data/hero.mjs';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const build = execFileSync('cargo', ['build', '--quiet', '--locked', '--offline', '-p', 'capyctl-cli', '--bin', 'capyctl', '--message-format=json'], {
  cwd: repo, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 64 << 20,
});
const bin = build.split('\n').filter(Boolean).map((l) => JSON.parse(l))
  .find((m) => m.reason === 'compiler-artifact' && m.target?.name === 'capyctl' && m.executable)?.executable;
if (!bin) throw new Error('cargo did not report the capyctl binary');

const cache = new Map();
const help = (path) => {
  const key = path.join(' ');
  if (!cache.has(key)) cache.set(key, execFileSync(bin, [...path, '--help'], { encoding: 'utf8' }));
  return cache.get(key);
};

const sources = readdirSync(join(repo, 'docs', 'guide')).filter((f) => f.endsWith('.md')).map((f) => join('docs', 'guide', f));
const commands = sources.flatMap((source) => pageCommands(readFileSync(join(repo, source), 'utf8')).map((args) => ({ source, args })));
// The landing page's terminal lines: the ones after a `$ ` prompt.
const landing = join(repo, 'site', 'src', 'components', 'landing');
for (const f of readdirSync(landing).filter((x) => x.endsWith('.astro'))) {
  for (const line of readFileSync(join(landing, f), 'utf8').split('\n')) {
    const at = line.indexOf('<span class="prompt">$ </span>');
    if (at < 0) continue;
    const text = line.slice(at).replace(/<[^>]+>/g, '');
    for (const args of invocations(text)) commands.push({ source: `site/src/components/landing/${f}`, args });
  }
}
for (const args of invocations(HERO_COMMAND)) commands.push({ source: 'site/src/data/hero.mjs', args });

const problems = commands.flatMap(({ source, args }) => checkInvocation(args, help).map((p) => `${source}: ${p}`));
if (problems.length) { console.error(problems.join('\n')); process.exit(1); }
console.log(`commands: ${commands.length} shown commands exist in capyctl --help`);
