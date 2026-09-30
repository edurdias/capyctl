import { test } from 'node:test';
import assert from 'node:assert/strict';
import { checkInvocation, invocations, pageCommands, words } from '../scripts/lib/commands.mjs';

const HELP = {
  '': 'Usage: capyctl [OPTIONS] <COMMAND>\n\nCommands:\n  engine  Engines\n  list    Lists\n\nOptions:\n      --config <FILE>  The file\n  -h, --help  Help\n',
  engine: 'Usage: capyctl engine <COMMAND>\n\nCommands:\n  add   Add one\n\nOptions:\n      --config <FILE>  The file\n',
  'engine add': 'Usage: capyctl engine add [PATH]\n\nOptions:\n      --name <NAME>  The name; not --made-up\n      --config <FILE>  The file\n',
  list: 'Usage: capyctl list <COMMAND>\n\nCommands:\n  hosts  Hosts\n',
  'list hosts': 'Usage: capyctl list hosts\n\nOptions:\n      --format <FORMAT>  table or json\n',
};
const help = (path) => HELP[path.join(' ')];

test('words keep quoted strings whole', () => {
  assert.deepEqual(words(`curl -d '{"a": 1}' -H "X: y"`), ['curl', '-d', '{"a": 1}', '-H', 'X: y']);
});

test('invocations skip sudo, assignments and comments, and split pipelines', () => {
  assert.deepEqual(invocations('sudo capyctl engine add /opt/vllm --config /etc/h.yaml'), [['engine', 'add', '/opt/vllm', '--config', '/etc/h.yaml']]);
  assert.deepEqual(invocations('CAPYCTL_MODELS_ROOT=~/models capyctl start standalone   # keep it running'), [['start', 'standalone']]);
  assert.deepEqual(invocations('$ capyctl list hosts | head; echo x && capyctl list hosts --format json'), [['list', 'hosts'], ['list', 'hosts', '--format', 'json']]);
  assert.deepEqual(invocations('curl http://x'), []);
});

test('a page yields commands from shell blocks, continuations and inline code only', () => {
  const md = [
    'Run `capyctl list hosts --format json` or `curl x`.',
    '```bash',
    'capyctl engine add ~/v \\',
    '  --name v2',
    '```',
    '```text',
    'capyctl not a command in output',
    '```',
  ].join('\n');
  assert.deepEqual(pageCommands(md), [['list', 'hosts', '--format', 'json'], ['engine', 'add', '~/v', '--name', 'v2']]);
});

test('existing commands and options pass', () => {
  assert.deepEqual(checkInvocation(['engine', 'add', '~/v', '--name', 'x', '--config', 'c'], help), []);
  assert.deepEqual(checkInvocation(['list', 'hosts', '--format=json'], help), []);
});

test('an unknown command, an unfinished group or an unknown option fails', () => {
  assert.equal(checkInvocation(['engines', 'add'], help).length, 1);
  assert.match(checkInvocation(['engine'], help)[0], /needs one of add/);
  assert.match(checkInvocation(['engine', 'add', '--made-up'], help)[0], /has no --made-up/);
  assert.match(checkInvocation(['list', 'hosts', '--config', 'c'], help)[0], /has no --config/);
});
