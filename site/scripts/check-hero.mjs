// Owner feedback 2026-09-25: the hero shows exactly what `mllm list
// deployments` prints. Render the fixture again with the CLI's own table code
// and compare it byte for byte with the block in the built page.
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { heroOutput } from './lib/hero-page.mjs';
import { HERO_COMMAND } from '../src/data/hero.mjs';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const data = join(repo, 'site', 'src', 'data');
const real = execFileSync('cargo', ['run', '--quiet', '--locked', '--offline', '-p', 'mllm-cli', '--example', 'hero_table', '--',
  join(data, 'hero-deployments.json'), join(data, 'hero-hosts.json')], {
  cwd: repo, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'],
});
const page = heroOutput(readFileSync(join(repo, 'site', 'dist', 'index.html'), 'utf8'));
let failed = false;
if (page.command !== `$ ${HERO_COMMAND}`) { console.error(`hero: command line is ${JSON.stringify(page.command)}`); failed = true; }
if (page.output !== real) {
  console.error(`hero: the page differs from the CLI output\n--- CLI\n${real}--- page\n${page.output}`);
  failed = true;
}
if (failed) process.exit(1);
console.log(`hero: byte-identical to mllm list deployments (${real.length} bytes)`);
