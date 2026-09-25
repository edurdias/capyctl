import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { HERO_COMMAND, heroSegments } from '../src/data/hero.mjs';

const deployments = JSON.parse(readFileSync(new URL('../src/data/hero-deployments.json', import.meta.url), 'utf8'));
const hosts = JSON.parse(readFileSync(new URL('../src/data/hero-hosts.json', import.meta.url), 'utf8'));

test('the command shown is the real CLI with its default table output', () => {
  assert.equal(HERO_COMMAND, 'mllm list deployments');
});

test('fixture: two ready models and one parked, generic names, on two generic hosts', () => {
  assert.deepEqual(deployments.map((d) => d.observed_state), ['ready', 'ready', 'parked']);
  for (const { name } of deployments) assert.match(name, /^[a-z0-9-]+$/);
  assert.deepEqual(hosts.hosts.map((h) => h.name).sort(), ['gpu-box', 'workstation']);
});

test('heroSegments marks only the state word parked, keeping every character', () => {
  const text = 'NAME     STATE\nparked-x parked\nchat     ready\n';
  const lines = heroSegments(text);
  assert.equal(lines.length, 3);
  assert.equal(lines.map((segs) => segs.map((s) => s.text).join('')).join('\n') + '\n', text);
  assert.deepEqual(lines[1], [{ text: 'parked-x ', parked: false }, { text: 'parked', parked: true }]);
  assert.ok(lines[2].every((s) => !s.parked));
});
