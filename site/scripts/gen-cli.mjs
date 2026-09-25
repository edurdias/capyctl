import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { cliPage } from './lib/cli-page.mjs';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const raw = execFileSync('cargo', ['run', '--quiet', '--locked', '-p', 'mllm-cli', '--example', 'cli_reference'], {
  cwd: repo, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'],
});
const file = join(repo, 'site', 'src', 'content', 'docs', 'docs', 'reference', 'cli.md');
mkdirSync(dirname(file), { recursive: true });
writeFileSync(file, cliPage(raw));
console.log('generated CLI reference');
