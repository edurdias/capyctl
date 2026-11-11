#!/usr/bin/env python3
"""Bounded, standard-library qualification runner; CPU checks are not qualification."""
import argparse
import ast
import concurrent.futures
import hashlib
import http.client
import importlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import sqlite3
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
MODULES = ('regressions', 'lmcache', 'measurements', 'engines', 'functional')


class SessionInterrupted(BaseException):
    """Stop the session, rather than treating a signal as an individual row failure."""

class UnavailableEngine(Exception):
    """A supplied shared-host mapping explicitly leaves an installation absent."""



def rows():
    order = {name: index for index, name in enumerate(('ISSUE225', 'LC13', 'LC14', 'LC15', 'LC16',
             'ISSUE236', 'ISSUE235', 'ISSUE240', 'EM1', 'EM8', 'OT8', 'TF2', 'TF3', 'TR3'))}
    return sorted([dict(row, module=name) for name in MODULES
                   for row in importlib.import_module('rows.' + name).ROWS],
                  key=lambda row: (row['priority'], order.get(row['id'], 100)))


def select_rows(value, catalog=None):
    catalog = rows() if catalog is None else catalog
    if value.lower() == 'all':
        return catalog
    wanted = set(value.upper().split(','))
    known = {row['id'].upper() for row in catalog}
    if not wanted or wanted - known:
        raise ValueError('unknown row selection: ' + ','.join(sorted(wanted - known)))
    return [row for row in catalog if row['id'].upper() in wanted]


def skip_reason(row, functional_only=False, unified=False):
    if row.get('discrete') and unified:
        return 'needs a discrete GPU'
    if row.get('unified') and not unified:
        return 'needs unified memory'
    if row.get('measurement') and functional_only:
        return 'measurement not allowed on this host'
    return None


def row_deadline(now, session_deadline, seconds):
    if seconds <= 0:
        raise ValueError('row budget must be positive')
    return min(session_deadline, now + seconds)


def atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    tmp = path.with_suffix(path.suffix + '.partial')
    with tmp.open('w') as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    tmp.replace(path)


def manifest(root):
    root = Path(root)
    entries = []
    for path in sorted(root.rglob('*')):
        if path.is_symlink() or not path.is_file() or path.name in ('SHA256SUMS', 'SHA256SUMS.partial'):
            continue
        digest = hashlib.sha256()
        with path.open('rb') as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                digest.update(chunk)
        entries.append(f'{digest.hexdigest()}  {path.relative_to(root).as_posix()}')
    target = root / 'SHA256SUMS'
    tmp = root / 'SHA256SUMS.partial'
    tmp.write_text('\n'.join(entries) + '\n')
    tmp.replace(target)
    return entries


def aggregate(catalog, records, selected):
    result = []
    for row in catalog:
        found = [record for record in records if record.get('id') == row['id']]
        if row['id'] not in selected:
            verdict, reason = 'skip', 'not selected'
        elif not found:
            verdict, reason = 'fail', 'no completed evidence (interrupted or deadline)'
        elif any(record.get('verdict') not in ('pass', 'fail', 'skip') for record in found):
            verdict, reason = 'fail', 'invalid verdict'
        elif any(record['verdict'] == 'fail' for record in found):
            verdict, reason = 'fail', next(record['reason'] for record in found if record['verdict'] == 'fail')
        elif any(record['verdict'] == 'skip' for record in found):
            verdict, reason = 'skip', next(record['reason'] for record in found if record['verdict'] == 'skip')
        else:
            verdict, reason = 'pass', found[-1]['reason']
        result.append(dict(id=row['id'], priority=row['priority'], budget=row['budget'],
                           criteria=row['criteria'], verdict=verdict, reason=reason))
    return dict(rows=result, selected_pass=all(row['verdict'] == 'pass' for row in result if row['id'] in selected),
                qualified=all(row['verdict'] == 'pass' for row in result))


def credentials(path):
    text = Path(path).read_text()
    if text.lstrip().startswith('{'):
        data = json.loads(text)
    else:
        data = dict((key.strip(), value.strip()) for line in text.splitlines()
                    for key, separator, value in [line.partition(':')] if separator)
    if not all(isinstance(data.get(name), str) and data[name] for name in ('api_key', 'admin_token')):
        raise ValueError('credentials file has missing fields')
    return {name: data[name] for name in ('api_key', 'admin_token')}


def load_env(path):
    result = {}
    for line in Path(path).read_text().splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        key, sep, value = line.partition('=')
        if not sep or not re.fullmatch('[A-Z][A-Z0-9_]*', key):
            raise ValueError('invalid engines file entry')
        parts = shlex.split(value)
        if len(parts) > 1:
            raise ValueError('engine values must be one shell-quoted path')
        result[key] = os.path.expandvars(os.path.expanduser(parts[0])) if parts else ''
    return result


class Context:
    def __init__(self, root, engines, functional_only=False, unified=False):
        self.root = Path(root)
        self.evidence = self.root / 'evidence'
        self.evidence.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.engines = engines
        if engines.get('CUDA_HOME'):
            os.environ['CUDA_HOME'] = engines['CUDA_HOME']
        os.environ.update(HF_HOME=str(self.root / 'cache/hf'),
                          XDG_CACHE_HOME=str(self.root / 'cache'),
                          XDG_CONFIG_HOME=str(self.root / 'state/config'),
                          XDG_DATA_HOME=str(self.root / 'data'),
                          CUDA_CACHE_PATH=str(self.root / 'cache/cuda'),
                          VLLM_CACHE_ROOT=str(self.root / 'cache/vllm'),
                          FLASHINFER_WORKSPACE_BASE=str(self.root / 'cache/flashinfer'),
                          PYTHONDONTWRITEBYTECODE='1',
                          UV_CACHE_DIR=str(self.root / 'cache/uv'),
                          TORCH_EXTENSIONS_DIR=str(self.root / 'cache/torch-extensions'),
                          TRITON_CACHE_DIR=str(self.root / 'cache/triton'),
                          DO_NOT_TRACK='1')
        self.functional_only, self.unified = functional_only, unified
        self.deadline = time.monotonic() + 3600
        self.roles, self.deployments, self.registered = {}, [], set()
        self.secrets = []
        self.index = 0
        self.row_id = 'session'
        self.model_store = self.root / 'models-local'
        self.model_store.mkdir(exist_ok=True, mode=0o700)
        for key, value in engines.items():
            if key.startswith('MODEL_') and value and Path(value).exists():
                link = self.model_store / key[6:].lower()
                target = Path(value).parent if key.endswith('_GGUF') and Path(value).is_file() else Path(value)
                if not link.exists():
                    link.symlink_to(target)

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('row time budget exhausted')
        return remaining

    def clean(self, value):
        if isinstance(value, dict):
            return {key: '<redacted>' if re.search(r'(?i)(key|token|password|secret)$', key)
                    else self.clean(item) for key, item in value.items()}
        if isinstance(value, list):
            return [self.clean(item) for item in value]
        if not isinstance(value, str):
            return value
        for secret in self.secrets:
            value = value.replace(secret, '<redacted>')
        value = re.sub(r'(?i)(bearer\s+)[\w.~+/=-]+', r'\1<redacted>', value)
        value = re.sub(
            r'(?i)((?:api[_-]?key|admin[_-]?token|secret|password)[\"\x27]?\s*[:=]\s*)(\"[^\"]*\"|\x27[^\x27]*\x27|[^,\s}\]]+)',
            lambda match: match[1] + (match[2][0] + '<redacted>' + match[2][0]
                                     if match[2][0] in "'\"" else "'<redacted>'"),
            value)
        return re.sub(r'/(?:home/[^/\s\"\x27]+|root)/', '/home/me/', value)

    def save(self, name, data):
        if not re.fullmatch(r'[A-Za-z0-9_.-]+', name):
            raise ValueError('unsafe evidence name')
        atomic_json(self.evidence / (self.row_id + '-' + name + '.json'), self.clean(data))

    def command(self, argv, check=True, env=None):
        result = subprocess.run([str(arg) for arg in argv], capture_output=True, text=True,
                                timeout=self.remaining(), env=env)
        self.index += 1
        # Command identity and environments deliberately do not enter evidence.
        self.save(f'operation-{self.index}', dict(exit_code=result.returncode,
                  stdout=result.stdout, stderr=result.stderr))
        if check and result.returncode:
            raise AssertionError(f'operation {self.index} failed (exit {result.returncode}); see evidence')
        return result

    def binary(self, old=False):
        return Path(self.engines.get('CAPYCTL_OLD', '')) if old and self.engines.get('CAPYCTL_OLD') else self.root / 'bin' / ('capyctl-old' if old else 'capyctl')

    def spawn(self, argv, name):
        log = (self.root / (name + '.log')).open('ab', buffering=0)
        proc = subprocess.Popen([str(arg) for arg in argv], stdout=log, stderr=subprocess.STDOUT,
                                start_new_session=True)
        log.close()
        from session import identity
        record = identity(proc.pid)
        if record:
            with (self.root / 'processes.jsonl').open('a') as stream:
                stream.write(json.dumps(record) + '\n')
        return proc

    def spawn_host(self, binary, config, name):
        self.command([binary, 'validate', 'config', '--file', config])
        return self.spawn([binary, 'start', 'host', '--config', config], name)

    def role(self, role='standalone', old=False):
        if role in self.roles:
            return self.roles[role]
        state = self.root / 'state' / role
        state.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(state, 0o700)
        binary = self.binary(old)
        if role == 'standalone':
            port = 19743
            proc = self.spawn([binary, '--state-dir', state, 'start', 'standalone',
                              '--listen', f'127.0.0.1:{port}', '--management-listen', '127.0.0.1:19744',
                              '--engine-ports', '19760-19789', '--models-root', self.model_store,
                              '--model-sources-path', self.root / 'sources', '--json'], role)
            info = dict(state=state, binary=binary, process=proc, port=port,
                        cred=state / 'identity/credentials', db=state / 'server/srv.sqlite3')
        else:
            port = 19843 + sum(name != 'standalone' for name in self.roles) * 100
            init = state / 'init.json'
            self.command([binary, 'init', 'server', '--output', init], env={**os.environ, 'CAPYCTL_STATE_DIR': str(state)})
            doc = json.loads(init.read_text())
            for kind, number in [('inference', port), ('management', port + 1), ('bootstrap', port + 2), ('control', port + 3)]:
                doc['listeners'][kind]['bind'] = f'127.0.0.1:{number}'
            doc['enrollment'] = dict(bootstrap_address=f'https://127.0.0.1:{port+2}',
                                     control_address=f'https://127.0.0.1:{port+3}')
            config = state / 'server.json'
            atomic_json(config, doc)
            proc = self.spawn([binary, 'start', 'server', '--config', config], role)
            info = dict(state=state, binary=binary, process=proc, port=port, config=config,
                        cred=state / 'identity/server-credentials.json', db=state / 'srv.sqlite3')
        self.roles[role] = info
        while time.monotonic() < self.deadline:
            if proc.poll() is not None:
                raise AssertionError('role failed before ready')
            if info['cred'].exists() and 'ready' in (self.root / (role + '.log')).read_text(errors='replace'):
                self.secrets.extend(credentials(info['cred']).values())
                break
            time.sleep(1)
        else:
            raise TimeoutError('role readiness deadline')
        if role != 'standalone':
            self.enroll(role)
        return info

    def enroll(self, role, old=False):
        info = self.roles[role]
        host = info['state'] / 'host'
        host.mkdir(mode=0o700, exist_ok=True)
        join, config = host / 'join.json', host / 'host.yaml'
        self.cli('invite', 'host', 'qualification-host', '--output', str(join), role=role)
        binary = self.binary(old)
        self.command([binary, 'init', 'host', '--output', config],
                     env={**os.environ, 'CAPYCTL_STATE_DIR': str(host / 'state'), 'XDG_CONFIG_HOME': str(host / 'cfg')})
        text = config.read_text()
        text = re.sub(r'(?m)^name:.*$', 'name: qualification-host', text)
        text += f'\nmodel_store:\n  path: {self.model_store}\ningress:\n  transport: trusted_private_link\n  address: http://127.0.0.1:{info["port"]+4}\n  bind: 127.0.0.1:{info["port"]+4}\n'
        config.write_text(text)
        self.command([binary, 'join', 'host', '--join-file', join, '--config', config])
        info['host_config'], info['host_binary'] = config, binary
        info['host_process'] = self.spawn_host(binary, config, role + '-host')
        time.sleep(5)

    def cli(self, *args, role='standalone', check=True, host=False, env=None):
        info = self.role(role)
        if host:
            argv = [info['host_binary'], '--config', info['host_config']]
        else:
            argv = [info['binary'], '--config', info['config']] if 'config' in info else [info['binary'], '--state-dir', info['state']]
        return self.command([*argv, *args], check=check, env=env)

    def installation(self, key):
        path = self.engines.get(key)
        if not path:
            if self.functional_only:
                raise UnavailableEngine('not installed on this host; qualified on the VM')
            raise AssertionError('engine installation unavailable: ' + key)
        if not Path(path).exists():
            raise AssertionError('mapped engine installation does not exist: ' + key)
        return path

    def register(self, engine, role='standalone'):
        key = engine.upper().replace('.', '').replace('-', '_')
        aliases = {'VLLM033': 'VLLM_033', 'VLLM032': 'VLLM_032', 'SGLANG0522': 'SGLANG_0522',
                   'SGLANG0523': 'SGLANG_0523', 'LLAMACPP090': 'LLAMACPP',
                   'TENSORFOLD126': 'TENSORFOLD_126', 'TENSORFOLD115': 'TENSORFOLD_115'}
        key = aliases.get(key, key)
        path = self.installation(key)
        name = 'q-' + key.lower().replace('_', '-')
        if (role, name) not in self.registered:
            cuda_key = {'TENSORRT_LLM_121': 'CUDA_HOME_TRT121',
                        'TENSORRT_LLM_140RC3': 'CUDA_HOME_TRT140RC3',
                        'TENSORRT_LLM': 'CUDA_HOME_TRT121'}.get(key, 'CUDA_HOME')
            env = dict(os.environ)
            if self.engines.get(cuda_key):
                env['CUDA_HOME'] = self.engines[cuda_key]
            self.cli('engine', 'add', path, '--name', name, role=role, host=role != 'standalone', env=env)
            self.registered.add((role, name))
        return name

    def deploy(self, name, engine, model='chat', api='chat', residency='restart_only', extra=None, role='standalone'):
        self.role(role)
        model = {'embedding': 'embed', 'embedding8b': 'embed8'}.get(model, model)
        if 'llamacpp' in engine.lower() and not model.endswith('_gguf'):
            model += '_gguf'
        profile = self.register(engine, role)
        config = dict(schema_version=1, kind='deployment', name=name, engine=profile,
                      model=model, api=api, residency=residency, devices=[dict(id='gpu0')],
                      timeouts=dict(initialize='1800s'), request_deadline='1800s',
                      engine_config=dict(context_length=32768, max_concurrent_requests=8,
                                         memory=dict(kv_cache='5GiB')))
        if 'llamacpp' in engine.lower() and api == 'embeddings':
            # Preserve the qualification context/slots; GGUF metadata sizes native KV.
            config['engine_config'].pop('memory')
        if role != 'standalone':
            config['host'] = 'qualification-host'
        if 'tensorfold' in engine.lower():
            config['resources'] = dict(gpu='32GiB', ram='2GiB')
            config['engine_config'].pop('memory', None)
        def merge(left, right):
            for key, value in right.items():
                if isinstance(value, dict) and isinstance(left.get(key), dict):
                    merge(left[key], value)
                else:
                    left[key] = value
        merge(config, extra or {})
        path = self.root / (name + '.json')
        atomic_json(path, config)
        self.cli('deploy', 'model', '--file', str(path), role=role)
        self.deployments.append((role, name))
        start = time.monotonic()
        try:
            self.cli('start', 'deployment', name, '--wait', role=role)
        except (AssertionError, OSError, subprocess.SubprocessError):
            self.save(name + '-engine-failure', dict(log=self.logs(name)))
            raise
        status = self.status(name, role)
        self.save(name + '-status', status)
        return dict(status=status) if self.functional_only else dict(status=status, start_s=time.monotonic() - start)

    def status(self, name, role='standalone'):
        return json.loads(self.cli('status', 'deployment', name, '--json', role=role).stdout)

    def lifecycle(self, action, name, role='standalone'):
        start = time.monotonic()
        self.cli('start' if action == 'wake' else action, 'deployment', name, role=role)
        target = dict(park='parked', wake='ready', stop='stopped', start='ready').get(action)
        while time.monotonic() < self.deadline:
            status = self.status(name, role)
            if status.get('observed_state', '').lower() == target:
                return dict(status=status) if self.functional_only else dict(seconds=time.monotonic() - start, status=status)
            time.sleep(1)
        raise TimeoutError(action + ' did not settle')

    def stop(self, name, role='standalone'):
        return self.lifecycle('stop', name, role)

    def request(self, name, path='/v1/chat/completions', body=None, stream=False, role='standalone', headers=None, method='POST'):
        info = self.role(role)
        payload = dict(model=name, messages=[dict(role='user', content='What is 17+25? Answer only 42. /no_think')],
                       temperature=0, max_tokens=32) if body is None else dict(body, model=name)
        if stream:
            payload['stream'] = True
        auth = credentials(info['cred'])['api_key']
        conn = http.client.HTTPConnection('127.0.0.1', info['port'], timeout=self.remaining())
        start = time.monotonic()
        conn.request(method, path, json.dumps(payload) if method != 'GET' else None, {'Content-Type': 'application/json',
                     'Authorization': 'Bearer ' + auth, **(headers or {})})
        response = conn.getresponse()
        chunks, first, raw = [], None, bytearray()
        timing = None
        while True:
            chunk = response.readline() if stream else response.read(65536)
            if not chunk:
                break
            if first is None and not stream:
                first = time.monotonic() - start
            raw.extend(chunk)
            if stream and chunk.startswith(b': x-capyctl-timing '):
                timing = json.loads(chunk[len(b': x-capyctl-timing '):])
            if stream and chunk.startswith(b'data:'):
                chunks.append(chunk[5:].decode().strip())
                if first is None and chunks[-1] != '[DONE]':
                    try:
                        choices = json.loads(chunks[-1]).get('choices', [])
                        if any(choice.get('text') or any(choice.get('delta', {}).get(key)
                               for key in ('content', 'reasoning_content', 'tool_calls')) for choice in choices):
                            first = time.monotonic() - start
                    except (ValueError, AttributeError):
                        pass
        response_headers = dict(response.getheaders())
        conn.close()
        try:
            data = json.loads(raw)
        except ValueError:
            data = raw.decode(errors='replace')
        out = dict(status=response.status, body=data, chunks=chunks, done='[DONE]' in chunks,
                   response_headers=response_headers)
        if timing is not None:
            out['timing'] = timing
        if not self.functional_only:
            out.update(seconds=time.monotonic() - start, ttft_s=first)
        return out

    def bench(self, name, concurrency=1, tokens=256, rounds=3, **kwargs):
        self.assertion(not self.functional_only, 'measurement not allowed on this host')
        start = time.monotonic()
        def one(_):
            body = kwargs.get('body', dict(messages=[dict(role='user', content='Write a detailed story. /no_think')],
                                           max_tokens=tokens, temperature=0))
            return self.request(name, body=body, **{key: value for key, value in kwargs.items() if key != 'body'})
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
            results = list(pool.map(one, range(concurrency * rounds)))
        seconds = time.monotonic() - start
        count = sum(result['body'].get('usage', {}).get('completion_tokens', 0)
                    for result in results if isinstance(result['body'], dict))
        return dict(requests=len(results), ok=sum(result['status'] == 200 for result in results),
                    seconds=seconds, tokens=count, tokens_per_s=count / seconds, results=results)

    def sql(self, query, args=(), role='standalone'):
        if not re.match(r'^\s*SELECT\b', query, re.I):
            raise ValueError('read-only queries required')
        db = self.role(role)['db']
        with sqlite3.connect(f'file:{db}?mode=ro', uri=True) as connection:
            connection.row_factory = sqlite3.Row
            return [dict(row) for row in connection.execute(query, args)]

    def events(self, role='standalone'):
        events = self.sql('SELECT kind,payload_json FROM management_events ORDER BY sequence', role=role)
        return self.clean(events)

    def owned(self, name, role='standalone'):
        bindings = self.sql("SELECT identities_json FROM runtime_bindings WHERE deployment_id=(SELECT id FROM deployments WHERE name=?) AND state IN ('live','uncertain')", (name,), role)
        return [identity for binding in bindings for identity in json.loads(binding['identities_json'] or '[]')]

    def sample(self):
        if self.functional_only:
            raise AssertionError('measurement not allowed on this host')
        result = self.command(['nvidia-smi', '--query-gpu=memory.used', '--format=csv,noheader,nounits']).stdout
        memory = dict((key.rstrip(':'), int(value.split()[0]) * 1024)
                      for key, value in (line.split(':', 1) for line in Path('/proc/meminfo').read_text().splitlines()))
        rss = 0
        for role in self.roles.values():
            path = Path('/proc') / str(role['process'].pid) / 'status'
            if path.exists():
                match = re.search(r'VmRSS:\s+(\d+)', path.read_text())
                rss += int(match[1]) * 1024 if match else 0
        return dict(gpu_used_bytes=sum(int(line) for line in result.splitlines()) * 2**20,
                    mem_available_bytes=memory['MemAvailable'], router_rss_bytes=rss)

    def logs(self, name=None):
        if name is None:
            paths = list(self.root.glob('*.log')) + list((self.root / 'state').rglob('*.log'))
        else:
            paths = []
            for role, info in self.roles.items():
                try:
                    bindings = self.sql('SELECT b.incarnation FROM runtime_bindings b JOIN deployments d ON d.id=b.deployment_id WHERE d.name=? ORDER BY b.rowid DESC LIMIT 1', (name,), role)
                except sqlite3.Error:
                    continue
                for binding in bindings:
                    paths.extend(info['state'].rglob(str(binding['incarnation']) + '.log'))
        return self.clean('\n'.join(path.read_text(errors='replace') for path in sorted(set(paths))))

    def engine_args(self, name):
        # Never export raw argv: only the owned settings tested by these rows.
        allowed = {'gpu-memory-utilization', 'kv-cache-memory-bytes', 'disable-prefill-cuda-graph',
                   'disable-decode-cuda-graph', 'cuda-graph-backend-prefill', 'cuda-graph-backend-decode',
                   'supported-transfer-mode', 'kv-transfer-config', 'is-embedding'}
        flags = {}
        for identity in self.owned(name):
            path = Path('/proc') / str(identity['pid']) / 'cmdline'
            if not path.exists():
                continue
            args = path.read_bytes().decode(errors='replace').split('\0')
            for index, arg in enumerate(args):
                key, _, value = arg.removeprefix('--').partition('=')
                if arg.startswith('--') and key in allowed:
                    flags[key] = value or (args[index+1] if index+1 < len(args) and not args[index+1].startswith('--') else True)
        resolved = {}
        for line in self.logs(name).splitlines():
            if 'server_args=ServerArgs(' in line:
                for key, value in re.findall(r'(disable_prefill_cuda_graph|disable_decode_cuda_graph|cuda_graph_backend_prefill|cuda_graph_backend_decode)=([^, )]+)', line):
                    resolved[key] = value.strip("'\"")
            if 'server_args={' in line:
                try:
                    doc = ast.literal_eval(line.split('server_args=', 1)[1].strip())
                    resolved.update({key: str(value) for key, value in doc.items()
                                     if key in {item.replace('-', '_') for item in allowed}})
                    for phase, settings in doc.get('cuda_graph_config', {}).items():
                        if phase in ('prefill', 'decode') and isinstance(settings, dict):
                            resolved['cuda_graph_backend_' + phase] = settings.get('backend')
                except (SyntaxError, ValueError):
                    pass
            if 'non-default args:' in line:
                try:
                    doc = ast.literal_eval(line.split('non-default args:', 1)[1].strip())
                    resolved.update({key: value for key, value in doc.items() if key in {item.replace('-', '_') for item in allowed}})
                except (SyntaxError, ValueError):
                    pass
        return dict(flags=flags, resolved=resolved)

    @staticmethod
    def assertion(condition, reason):
        if not condition:
            raise AssertionError(reason)

    def cleanup(self):
        failures = []
        self.deadline = max(self.deadline, time.monotonic() + 120)
        for role, name in self.deployments[:]:
            try:
                self.cli('delete', 'deployment', name, '--stop', role=role)
                self.deployments.remove((role, name))
            except (AssertionError, subprocess.SubprocessError) as error:
                failures.append(type(error).__name__)
        if failures:
            raise AssertionError('managed cleanup failed: ' + ','.join(failures))

    def down(self):
        self.cleanup()
        for info in self.roles.values():
            for key in ('host_process', 'process'):
                proc = info.get(key)
                if proc and proc.poll() is None:
                    proc.terminate()  # only child processes this harness started
                    try:
                        proc.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        raise AssertionError('owned role did not stop; purge refused')


def summary(root, selection):
    root = Path(root)
    catalog = rows()
    selected = {row['id'] for row in select_rows(selection, catalog)}
    records = []
    for path in (root / 'evidence').glob('result-*.json'):
        try:
            records.append(json.loads(path.read_text()))
        except ValueError:
            pass
    report = aggregate(catalog, records, selected)
    atomic_json(root / 'summary.json', report)
    (root / 'summary.txt').write_text('\n'.join(f"{row['id']}: {row['verdict']}: {row['reason']}" for row in report['rows']) + '\n')
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['plan', 'run', 'summary', 'manifest'])
    parser.add_argument('--root', type=Path, default=Path.home() / 'capyctl-v020')
    parser.add_argument('--engines', type=Path)
    parser.add_argument('--only', default='all')
    parser.add_argument('--functional-only', action='store_true')
    parser.add_argument('--unified', action='store_true')
    parser.add_argument('--max-seconds', type=int, default=25200)
    args = parser.parse_args()
    selected = select_rows(args.only)
    if args.command == 'plan':
        print('Priority order; each budget includes managed startup and cleanup:')
        for row in selected:
            print(f"P{row['priority']} {row['id']} [{row['budget']}s]: {row['criteria']}")
        return 0
    if args.command == 'manifest':
        manifest(args.root)
        return 0
    if args.command == 'summary':
        return 0 if summary(args.root, args.only)['selected_pass'] else 1
    ctx = Context(args.root, load_env(args.engines or args.root / 'engines.env'), args.functional_only, args.unified)
    session_deadline = time.monotonic() + args.max_seconds
    def interrupted(_signum, _frame):
        raise SessionInterrupted('session interrupted')
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    from session import identity
    atomic_json(args.root / 'runner.json', identity(os.getpid()))
    try:
        for row in selected:
            ctx.row_id = row['id']
            reason = skip_reason(row, args.functional_only, args.unified)
            result = dict(verdict='skip', reason=reason, evidence={})
            if not reason:
                ctx.deadline = row_deadline(time.monotonic(), session_deadline, row['budget'])
                if ctx.deadline <= time.monotonic():
                    result = dict(verdict='fail', reason='session deadline exhausted', evidence={})
                else:
                    try:
                        result = importlib.import_module('rows.' + row['module']).run(ctx, row)
                    except UnavailableEngine as error:
                        result = dict(verdict='skip', reason=str(error), evidence={})
                    except (AssertionError, OSError, ValueError, KeyError, TimeoutError, subprocess.SubprocessError) as error:
                        result = dict(verdict='fail', reason=ctx.clean(str(error))[:300] or type(error).__name__, evidence={})
                    finally:
                        try:
                            ctx.cleanup()
                        except (AssertionError, subprocess.SubprocessError) as error:
                            result = dict(verdict='fail', reason=str(error), evidence=result.get('evidence', {}))
            atomic_json(ctx.evidence / ('result-' + row['id'] + '.json'), ctx.clean(dict(id=row['id'], **result)))
            print(f"{row['id']}: {result['verdict']}: {result['reason']}", flush=True)
            summary(args.root, args.only)
    finally:
        ctx.deadline = time.monotonic() + 180
        ctx.down()
        summary(args.root, args.only)
    return 0 if summary(args.root, args.only)['selected_pass'] else 1


if __name__ == '__main__':
    sys.exit(main())
