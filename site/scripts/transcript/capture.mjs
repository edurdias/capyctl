// Runs every command the guide shows against a built `mllm` and prints the
// transcript the guide's output blocks are copied from.
//
//   node scripts/transcript/capture.mjs <path/to/mllm> [sandbox-dir]
//
// The engine is scripts/transcript/fake-vllm.py, a stand-in that answers
// mllm's HTTP calls and loads nothing, so this runs on a machine without an
// engine. It shows what mllm prints; it is not a test of any engine.
//
// Everything runs in private sandbox homes under [sandbox-dir] (default
// ~/.cache/mllm-site-docs/capture), with a cleared environment. Listeners
// use ports far from the defaults so a running mllm is never touched. The
// printed transcript replaces sandbox paths with /home/me, the machine's
// host name with gpu-box and the sandbox ports with the defaults, then
// realigns tables so the columns stay as the CLI lays them out.
import { execFileSync, spawn } from 'node:child_process';
import { chmodSync, closeSync, cpSync, openSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { homedir, hostname } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const [binArg, rootArg] = process.argv.slice(2);
if (!binArg) { console.error('usage: capture.mjs <path/to/mllm> [sandbox-dir]'); process.exit(2); }
const bin = resolve(binArg);
const root = resolve(rootArg ?? join(homedir(), '.cache', 'mllm-site-docs', 'capture'));

// Sandbox ports -> the defaults the guide shows.
const PORTS = { 18443: 8443, 17443: 7443, 37443: 7443, 38443: 8443, 37444: 7444, 37445: 7445, 38444: 8444 };
const children = [];
// Each block is printed as soon as it is captured.
const emit = (block) => console.log(block);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function privateDir(p) { mkdirSync(p, { recursive: true }); chmodSync(p, 0o700); }

// The state lock refuses a group- or other-writable ancestor.
for (let p = root; p !== dirname(p) && p.startsWith(homedir()) && p !== homedir(); p = dirname(p)) {
  if (existsSync(p)) chmodSync(p, 0o700);
}
// Engines outlive their role by design, and a client a timeout abandoned
// keeps waiting: stop whatever an earlier run left under the sandbox, so its
// ports are free.
function killLeftovers() {
  try { execFileSync('pkill', ['-f', `${root}/`]); } catch { /* none left */ }
}
killLeftovers();
rmSync(root, { recursive: true, force: true });
privateDir(root);

function home(name) { return join(root, name); }

// Each sandbox home listens on its own loopback ports, far from the defaults.
const LISTEN = {
  one: { MLLM_INFERENCE_ADDR: '127.0.0.1:18443', MLLM_MANAGEMENT_ADDR: '127.0.0.1:17443', MLLM_ENGINE_PORTS: '18100-18107' },
  server: { MLLM_INFERENCE_ADDR: '127.0.0.1:38443', MLLM_MANAGEMENT_ADDR: '127.0.0.1:37443' },
  'gpu-box': { MLLM_ENGINE_PORTS: '38100-38199' },
};

// `extra` adds variables; a null value leaves that variable out.
function env(name, extra = {}) {
  const h = home(name);
  const all = { HOME: h, PATH: `${h}/.local/bin:/usr/bin:/bin`, USER: process.env.USER ?? 'me', LANG: 'C.UTF-8', TERM: 'dumb',
    ...LISTEN[name], ...extra };
  return Object.fromEntries(Object.entries(all).filter(([, v]) => v !== null));
}

function makeHome(name) {
  const h = home(name);
  privateDir(h);
  for (const d of ['.local', '.local/bin', '.local/state', '.config', 'models', 'venvs']) privateDir(join(h, d));
  cpSync(bin, join(h, '.local/bin/mllm'));
  chmodSync(join(h, '.local/bin/mllm'), 0o755);
  return h;
}

// A venv that looks like a vLLM installation: package metadata, `bin/vllm`,
// and a `bin/python3` that answers the capability probe or runs the stand-in.
function fakeVllm(h, dir, version) {
  const v = join(h, 'venvs', dir);
  const site = join(v, 'lib/python3.12/site-packages');
  mkdirSync(join(site, 'vllm'), { recursive: true });
  mkdirSync(join(site, `vllm-${version}.dist-info`), { recursive: true });
  writeFileSync(join(site, `vllm-${version}.dist-info/METADATA`), `Metadata-Version: 2.1\nName: vllm\nVersion: ${version}\n`);
  writeFileSync(join(v, 'pyvenv.cfg'), 'home = /usr/bin\n');
  mkdirSync(join(v, 'bin'), { recursive: true });
  const fake = join(v, 'lib/fake_vllm.py');
  writeFileSync(fake, readFileSync(join(here, 'fake-vllm.py'), 'utf8').replace('VERSION = "0.29.0"', `VERSION = "${version}"`));
  writeFileSync(join(v, 'bin/vllm'), `#!/bin/sh\nexec /usr/bin/python3 ${fake} "$@"\n`, { mode: 0o755 });
  const probe = JSON.stringify({ schema: 'mllm/engine-capabilities/v1', engine: 'vllm', capabilities: { core: [], deep_park: [], metrics: [] } });
  writeFileSync(join(v, 'bin/python3'), `#!/bin/sh
if [ "$1" = "-B" ]; then shift; fi
case "$1" in
  *vllm_entry.py) shift; exec /usr/bin/python3 ${fake} "$@";;
esac
echo '${probe}'
`, { mode: 0o755 });
}

function checkpoint(h, dir) {
  const m = join(h, 'models', dir);
  mkdirSync(m, { recursive: true });
  writeFileSync(join(m, 'config.json'), JSON.stringify({ architectures: ['Qwen3ForCausalLM'], model_type: 'qwen3',
    max_position_embeddings: 40960, hidden_size: 2560, num_hidden_layers: 36, num_attention_heads: 32,
    num_key_value_heads: 8, head_dim: 128, torch_dtype: 'bfloat16' }));
  writeFileSync(join(m, 'model.safetensors'), Buffer.alloc(1 << 20));
}

function sanitize(text) {
  let t = text.split(root + '/server').join('/home/me').split(root + '/gpu-box').join('/home/me')
    .split(root + '/one').join('/home/me');
  // The sandbox serves inference on loopback; a real start serves it on
  // every interface, the default the guide shows.
  t = t.replaceAll('inference listener 127.0.0.1:18443', 'inference listener 0.0.0.0:8443')
    .replaceAll('"inference":"127.0.0.1:38443"', '"inference":"0.0.0.0:8443"');
  for (const [from, to] of Object.entries(PORTS)) t = t.replaceAll(`:${from}`, `:${to}`);
  t = t.replace(new RegExp(`\\b${hostname()}\\b`, 'g'), 'gpu-box');
  return realign(t);
}

// The CLI pads every column but the last to its widest cell plus three
// spaces. Cells never hold three spaces in a row.
function realign(text) {
  const lines = text.split('\n');
  const isHeader = (l) => /^[A-Z][A-Z0-9 ()/]*\S$/.test(l) && / {3,}/.test(l);
  for (let i = 0; i < lines.length; i++) {
    if (!isHeader(lines[i])) continue;
    let j = i + 1;
    while (j < lines.length && lines[j].trim() !== '' && / {3,}/.test(lines[j]) && !isHeader(lines[j])) j++;
    const rows = lines.slice(i, j).map((l) => l.split(/ {3,}/));
    const n = Math.max(...rows.map((r) => r.length));
    const widths = [...Array(n).keys()].map((c) => Math.max(...rows.map((r) => (r[c] ?? '').length)));
    rows.forEach((r, k) => { lines[i + k] = r.map((cell, c) => (c === r.length - 1 ? cell : cell.padEnd(widths[c] + 3))).join(''); });
    i = j - 1;
  }
  return lines.join('\n');
}

function run(name, label, command, extra = {}) {
  let text;
  try {
    text = execFileSync('sh', ['-c', `cd "$HOME" && ${command} 2>&1`], { env: env(name, extra), encoding: 'utf8', timeout: 90_000 });
  } catch (e) {
    text = `${e.stdout ?? ''}[exit ${e.status}]\n`;
  }
  emit(`### ${label}\n$ ${command}\n${sanitize(text)}`);
  return text;
}

// A role's output goes to a file: a pipe nobody drains while a command
// runs synchronously would fill and stall the role.
function background(name, label, command, extra = {}) {
  const log = join(root, `${name}-${children.length}.log`);
  const fd = openSync(log, 'w');
  const child = spawn('sh', ['-c', `cd "$HOME" && ${command}`], { env: env(name, extra), stdio: ['ignore', fd, fd], detached: true });
  closeSync(fd);
  children.push(child);
  return async (ms) => {
    await sleep(ms);
    const first = readFileSync(log, 'utf8').split('\n').filter((l) => !l.startsWith('{"candidates"')).join('\n');
    emit(`### ${label}\n$ ${command}\n${sanitize(first)}`);
  };
}

// A role runs under `sh -c`; its process group is signalled as a whole.
function stop(child) { try { process.kill(-child.pid, 'SIGTERM'); } catch { /* already gone */ } }


// A file the guide shows, as a ```yaml title="<name>" block of a page.
function guideBlock(page, name) {
  const text = readFileSync(join(here, '..', '..', '..', 'docs', 'guide', page), 'utf8');
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const m = text.match(new RegExp('```yaml title="' + escaped + '"\\n([\\s\\S]*?)```'));
  if (!m) throw new Error(`${page} shows no ${name}`);
  return m[1];
}

// The deployment file is the one the guide shows.
const guideFile = guideBlock('deploy.md', 'my-model.yaml');
const deployment = (name, path, extra = '') => guideFile
  .replaceAll('my-model', name).replaceAll('Qwen3-4B', path) + extra;

const KEY = "$(sed -n 's/^api_key: //p' ~/.local/state/mllm/identity/credentials)";
const SERVER_KEY = `$(sed -n 's/.*"api_key": *"\\([^"]*\\)".*/\\1/p' ~/.local/state/mllm/identity/server-credentials.json)`;
const curlChat = (port, model, stream = false, key = KEY) => `curl -s http://127.0.0.1:${port}/v1/chat/completions -H "Authorization: Bearer ${key}" -H 'Content-Type: application/json' -d '{"model": "${model}", "messages": [{"role": "user", "content": "Hello"}]${stream ? ', "stream": true' : ''}}'; echo`;

async function oneMachine() {
  const h = makeHome('one');
  fakeVllm(h, 'vllm', '0.29.0');
  fakeVllm(h, 'vllm-nightly', '0.30.0rc1');
  checkpoint(h, 'Qwen3-4B');
  checkpoint(h, 'Llama-3.1-8B');
  writeFileSync(join(h, 'my-model.yaml'), deployment('my-model', 'Qwen3-4B'));
  writeFileSync(join(h, 'other-model.yaml'), deployment('other-model', 'Llama-3.1-8B'));

  run('one', 'engine detect', 'mllm engine detect');
  run('one', 'engine add before start', 'mllm engine add ~/venvs/vllm');
  run('one', 'engine list before start', 'mllm engine list');
  const started = background('one', 'start standalone', 'mllm start standalone');
  await started(8000);
  run('one', 'config show', 'mllm config show', { MLLM_INFERENCE_ADDR: null, MLLM_MANAGEMENT_ADDR: null, MLLM_ENGINE_PORTS: null });
  run('one', 'config show set', 'mllm config show --set server.switching.drain_timeout=45s', { MLLM_INFERENCE_ADDR: null, MLLM_MANAGEMENT_ADDR: null, MLLM_ENGINE_PORTS: null });
  run('one', 'engine add custom', 'mllm engine add ~/venvs/vllm-nightly --name vllm-nightly');
  run('one', 'engine list', 'mllm engine list');
  run('one', 'engine remove', 'mllm engine remove vllm-nightly');
  run('one', 'validate', 'mllm validate config --file my-model.yaml');
  run('one', 'deploy and start', 'mllm deploy model --file my-model.yaml --activate --wait > /dev/null; echo exit $?');
  run('one', 'list deployments', 'mllm list deployments');
  run('one', 'status', 'mllm status deployment my-model');
  run('one', 'models', `curl -s http://127.0.0.1:18443/v1/models -H "Authorization: Bearer ${KEY}"; echo`);
  run('one', 'models no key', `curl -s -o /dev/null -w '%{http_code}\\n' http://127.0.0.1:18443/v1/models`);
  run('one', 'chat', curlChat(18443, 'my-model'));
  run('one', 'stream', curlChat(18443, 'my-model', true));
  const py = join(root, 'client.py');
  writeFileSync(py, `from openai import OpenAI
from pathlib import Path

key = next(line.split(": ", 1)[1] for line in
           (Path.home() / ".local/state/mllm/identity/credentials").read_text().splitlines()
           if line.startswith("api_key: "))
client = OpenAI(base_url="http://127.0.0.1:18443/v1", api_key=key)

reply = client.chat.completions.create(
    model="my-model",
    messages=[{"role": "user", "content": "Hello"}],
)
print(reply.choices[0].message.content)

for chunk in client.chat.completions.create(
    model="my-model",
    messages=[{"role": "user", "content": "Hello"}],
    stream=True,
):
    print(chunk.choices[0].delta.content or "", end="", flush=True)
print()
`);
  const client = join(homedir(), '.cache', 'mllm-site-docs', 'client-venv', 'bin', 'python');
  if (existsSync(client)) run('one', 'python', `${client} ${py}`);
  // Parking and switching.
  run('one', 'park', 'mllm park deployment my-model');
  await sleep(3000);
  run('one', 'list parked', 'mllm list deployments');
  run('one', 'wake by request', curlChat(18443, 'my-model'));
  run('one', 'list woken', 'mllm list deployments');
  run('one', 'deploy other', 'mllm deploy model --file other-model.yaml');
  run('one', 'start evict', 'mllm start deployment other-model --evict');
  await sleep(6000);
  run('one', 'list after evict', 'mllm list deployments');
  run('one', 'switch back by request', curlChat(18443, 'my-model'));
  run('one', 'list switched', 'mllm list deployments');
  run('one', 'stop', 'mllm stop deployment other-model');
  await sleep(3000);
  run('one', 'list stopped', 'mllm list deployments');
  run('one', 'stopped request', curlChat(18443, 'other-model'));
  run('one', 'delete', 'mllm delete deployment other-model --stop');
  run('one', 'list after delete', 'mllm list deployments');
  run('one', 'stop my-model', 'mllm stop deployment my-model > /dev/null; echo exit $?');
  await sleep(4000);
  run('one', 'start my-model', 'mllm start deployment my-model --wait > /dev/null; echo exit $?');
  writeFileSync(join(h, 'my-model.yaml'), deployment('my-model', 'Qwen3-4B', 'residency: deep\n'));
  run('one', 'update', 'mllm deploy model --file my-model.yaml --revision 1');
  await sleep(6000);
  run('one', 'list after update', 'mllm list deployments');
  run('one', 'delete my-model', 'mllm delete deployment my-model --stop > /dev/null; echo exit $?');
}

async function severalMachines() {
  const s = makeHome('server');
  const g = makeHome('gpu-box');
  fakeVllm(g, 'vllm', '0.29.0');
  checkpoint(g, 'Qwen3-4B');
  run('server', 'init server', 'mllm init server --output server.yaml');
  // The server and host files are the ones the guide shows, moved onto
  // loopback ports and the sandbox home.
  writeFileSync(join(s, 'server.yaml'), guideBlock('several-machines.md', 'server.yaml')
    .replaceAll('/home/me', s).replaceAll('127.0.0.1:7443', '127.0.0.1:37443').replaceAll('0.0.0.0:8443', '127.0.0.1:38443')
    .replaceAll('100.64.0.10:7444', '127.0.0.1:37444').replaceAll('100.64.0.10:7445', '127.0.0.1:37445'));
  const server = background('server', 'start server', 'mllm start server --config ~/server.yaml');
  await server(6000);
  run('server', 'invite', 'mllm invite host gpu-box --output gpu-box.join --config ~/server.yaml');
  cpSync(join(s, 'gpu-box.join'), join(g, 'gpu-box.join'));
  run('gpu-box', 'init host', 'mllm init host --output host.yaml');
  const hostFile = (block) => guideBlock('several-machines.md', block)
    .replaceAll('/home/me', g).replaceAll('100.64.0.21:8444', '127.0.0.1:38444');
  writeFileSync(join(g, 'host-unified.yaml'), hostFile('host.yaml (unified memory)'));
  run('gpu-box', 'validate unified host', 'mllm validate config --file ~/host-unified.yaml');
  writeFileSync(join(g, 'host.yaml'), hostFile('host.yaml'));
  run('gpu-box', 'validate host', 'mllm validate config --file ~/host.yaml');
  run('gpu-box', 'join', 'mllm join host --join-file gpu-box.join --config ~/host.yaml');
  const host = background('gpu-box', 'start host', 'mllm start host --config ~/host.yaml');
  await host(8000);
  run('gpu-box', 'engine add on host', 'mllm engine add ~/venvs/vllm --config ~/host.yaml');
  await sleep(2000);
  run('server', 'list hosts', 'mllm list hosts --config ~/server.yaml');
  run('server', 'list engines', 'mllm list engines --config ~/server.yaml');
  writeFileSync(join(s, 'my-model.yaml'), deployment('my-model', 'Qwen3-4B'));
  run('server', 'deploy on host', 'mllm deploy model --file my-model.yaml --activate --wait --config ~/server.yaml > /dev/null; echo exit $?');
  run('server', 'list on server', 'mllm list deployments --config ~/server.yaml');
  run('server', 'request on server', curlChat(38443, 'my-model', false, SERVER_KEY));
  run('server', 'park on server', 'mllm park deployment my-model --config ~/server.yaml > /dev/null; sleep 3; mllm list deployments --config ~/server.yaml');
}

try {
  await oneMachine();
  for (const c of children.splice(0)) stop(c);
  await sleep(4000);
  await severalMachines();
} finally {
  for (const c of children) stop(c);
  await sleep(3000);
  killLeftovers();
}
