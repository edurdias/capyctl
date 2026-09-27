// Website spec, Landing page 1 and Technical approach: the hero's
// `mllm list deployments` table is produced by the CLI's own table code at
// build time, never typed by hand.
import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const data = join(repo, 'site', 'src', 'data');
const table = execFileSync('cargo', ['run', '--quiet', '--locked', '--offline', '-p', 'mllm-cli', '--example', 'hero_table', '--',
  join(data, 'hero-deployments.json'), join(data, 'hero-hosts.json')], {
  cwd: repo, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'],
});
if (!/^NAME\s/.test(table)) throw new Error(`unexpected table output:\n${table}`);
const file = join(repo, 'site', 'src', 'generated', 'hero-table.txt');
mkdirSync(dirname(file), { recursive: true });
writeFileSync(file, table);
console.log('generated hero table');
