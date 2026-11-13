"""ADR 0039 TF4–TF14: live native TensorFold qualification.

No CPU result from this module is native-engine qualification (SPEC §18).
"""
import concurrent.futures
import contextlib
import http.client
import json
import math
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import threading
import time

from .engines import answered, passing_canary
from .lmcache import live_identities, listening_sockets
from .measurements import isolated_role


CRITERIA = {
    'TF4': 'Native refusals, exactly multi_gpu_unsupported on one host and group_shape_unsupported:tensorfold across hosts, unlisted-family launch diagnostic, 16-stream engine refusal, reserve-pressure comparison and retention-zero compaction refusal.',
    'TF5': 'Eight running streams drain to both work gauges zero before stop; busy engine survives the 30-second bound; parking is refused.',
    'TF6': 'Four-client cuts, sampled every 100 ms, streamed and collected, three repeats each; unanswered cancellation stays held; 32 held requests restart and recover.',
    'TF7': 'Enrolled-host idle/busy reports, per-pool KV metrics, both forwarded histograms, remote cancelled-answer settlement and native request timings.',
    'TF8': 'Mutually exclusive native/vLLM grants switch in turn; fresh native wake finishes inside Initialize and passes its text completion canary.',
    'TF9': 'TensorFold 0.6.6 registers beside native, starts the same checkpoint, reports health ok/busy, drains and stops.',
    'TF10': 'Owned native process has only its IPv4 loopback listener after readiness and controller re-proof.',
    'TF12': '1.1.5 exact command, refused-to-ready health, startup/peak memory and 1/8-stream throughput; reserved idle-unload refusal.',
    'TF13': '1.1.5 eight-stream drain, SIGTERM-to-exit measurement and health 503/draining; busy 30-second bound does not signal.',
    'TF14': '1.1.5 complete TF6 cancellation protocol and TF7 enrolled-host reports and settlement.',
}
ROWS = [dict(id=key, priority=4, budget=3600 if key in ('TF4', 'TF6', 'TF12', 'TF14') else 1800,
             criteria=value, measurement=True, **({'unified': True} if int(key[2:]) < 11 else {}))
        for key, value in CRITERIA.items()]
RUNNING = 'tensorfold:requests_running'
WAITING = 'tensorfold:requests_waiting'


def samples(text, metric):
    """Keep every label set: a missing gauge must never become idle."""
    pattern = re.compile(r'^' + re.escape(metric) + r'(\{[^\n]*\})?\s+([^\s]+)(?:\s+\d+)?$')
    result = []
    for line in text.splitlines():
        match = pattern.fullmatch(line.strip())
        if match:
            value = float(match[2])
            if not math.isfinite(value) or value < 0:
                raise AssertionError('invalid nonnegative native metric: ' + metric)
            result.append(dict(labels=match[1] or '', value=value))
    return result


def work(text):
    values = [samples(text, name) for name in (RUNNING, WAITING)]
    if any(len(items) != 1 for items in values):
        raise AssertionError('native work scrape must contain exactly both work gauges')
    return dict(running=values[0][0]['value'], waiting=values[1][0]['value'])


def endpoint(ctx, name, role):
    rows = ctx.sql('''SELECT e.host,e.port FROM endpoint_leases e JOIN runtime_bindings b ON b.id=e.binding_id
        JOIN deployments d ON d.id=b.deployment_id WHERE d.name=? AND b.state!='released' ''', (name,), role)
    ctx.assertion(len(rows) == 1 and rows[0]['host'] == '127.0.0.1', 'native endpoint is not one leased IPv4 loopback socket')
    return rows[0]['port']


def get(ctx, port, path):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=min(2, ctx.remaining()))
    try:
        connection.request('GET', path)
        response = connection.getresponse()
        raw = response.read(4 * 1024 * 1024 + 1)
        ctx.assertion(len(raw) <= 4 * 1024 * 1024, 'native evidence response exceeded bound')
        return dict(status=response.status, body=raw.decode(errors='replace'), at=time.monotonic())
    except (ConnectionError, socket.timeout, http.client.HTTPException):
        return dict(status=None, body='', at=time.monotonic())
    finally:
        connection.close()


def scrape(ctx, name, role='standalone'):
    identities = live_identities(ctx, name, role)
    port = endpoint(ctx, name, role)
    listeners = listening_sockets(ctx, identities)
    ctx.assertion(listeners == [dict(host='127.0.0.1', port=port)], 'native engine has an unexpected listener')
    response = get(ctx, port, '/metrics')
    ctx.assertion(response['status'] == 200, 'native metrics unavailable')
    return dict(at=response['at'], **work(response['body']), text=response['body'])


def until(ctx, predicate, seconds=120):
    end = time.monotonic() + min(seconds, ctx.remaining())
    next_poll = time.monotonic()
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        next_poll += .1
        time.sleep(max(0, next_poll - time.monotonic()))
    raise AssertionError('bounded native observation did not arrive')


def leases(ctx, name, role):
    return ctx.sql('''SELECT l.id,l.disposition,c.engine_answered AS answered FROM request_leases l
        JOIN deployments d ON d.id=l.deployment_id LEFT JOIN request_lease_cancellations c ON c.lease_id=l.id
        WHERE d.name=? AND l.disposition='inflight' ''', (name,), role)


class Client:
    """One bounded real router connection, cut without waiting for its answer."""
    def __init__(self, ctx, name, role, stream=True, tokens=8192):
        from v020 import credentials
        info = ctx.role(role)
        self.connection = http.client.HTTPConnection('127.0.0.1', info['port'], timeout=min(300, ctx.remaining()))
        self.output = threading.Event()
        self.finished = threading.Event()
        self.result = {}
        self.error = None
        self.cut = False
        body = dict(model=name, messages=[dict(role='user', content='Write a very long, detailed novel. Continue without stopping. /no_think')],
                    max_tokens=tokens, temperature=0, stream=stream, ignore_eos=True)
        self.connection.request('POST', '/v1/chat/completions', json.dumps(body),
                                {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + credentials(info['cred'])['api_key']})
        self.socket = self.connection.sock
        self.thread = threading.Thread(target=self.read, args=(stream,), daemon=True)
        self.thread.start()

    def read(self, stream):
        try:
            response = self.connection.getresponse()
            self.result['status'] = response.status
            if stream:
                chunks = []
                while True:
                    line = response.readline()
                    if not line:
                        break
                    if line.startswith(b'data:'):
                        chunks.append(line[5:].decode().strip())
                        self.output.set()
                self.result.update(chunks=chunks, done='[DONE]' in chunks)
            else:
                raw = response.read(4 * 1024 * 1024 + 1)
                if raw:
                    self.output.set()
                self.result['body'] = json.loads(raw)
        except (OSError, ValueError, http.client.HTTPException) as error:
            self.error = type(error).__name__
        finally:
            self.connection.close()
            self.finished.set()

    def close(self):
        self.cut = True
        try:
            self.socket.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.connection.close()
        self.thread.join(timeout=3)


def signal_owned(ctx, name, role, value):
    identities = live_identities(ctx, name, role)
    for identity in identities:
        os.kill(identity['pid'], value)
    return identities


def launch(ctx, name, version='1.0.0', role='standalone', **kwargs):
    key = 'TENSORFOLD_' + version.replace('.', '')
    root = Path(ctx.installation(key))
    ctx.assertion((root / 'VERSION').read_text().strip() == version, 'qualification requires exact native version ' + version)
    extra = kwargs.setdefault('extra', {})
    extra.setdefault('resources', dict(gpu='30GiB', ram='2GiB'))
    return ctx.deploy(name, key, model='tensorfold', role=role, **kwargs)


def cut_protocol(ctx, name, role, evidence):
    """ADR 0039 TF6: six independent four-client trials, no early cut."""
    trials = []
    for stream in (True, False):
        for repeat in range(3):
            clients = []
            observations = []
            try:
                clients = [Client(ctx, name, role, stream) for _ in range(4)]
                def four():
                    sample = scrape(ctx, name, role)
                    sample.pop('text')
                    observations.append(sample)
                    return sample['running'] == 4 and sample['waiting'] == 0
                until(ctx, four)
                cut_at = time.monotonic()
                for client in clients:
                    client.close()
                def idle():
                    sample = scrape(ctx, name, role)
                    sample.pop('text')
                    observations.append(sample)
                    return sample['running'] == sample['waiting'] == 0
                until(ctx, idle, 300)
                trial = dict(stream=stream, repeat=repeat + 1, cut_at=cut_at,
                             idle_after_s=observations[-1]['at'] - cut_at, samples=observations)
                trials.append(trial)
                evidence['cuts'] = trials
                # Unanswered collected cuts are held until verified exit, never
                # erased by the gauge returning zero (SPEC §10, ADR 0032 item 6).
                if not stream:
                    held = leases(ctx, name, role)
                    ctx.assertion(any(row['answered'] == 0 for row in held), 'unanswered client cut did not remain cancelling')
                    evidence.setdefault('held_before_stop', []).append(held)
                # T41 / ADR 0032 item 6: a streamed cut can also precede its
                # first output. Reset every trial by verified exit, rather than
                # exhausting admission with eight unanswered cancellations.
                trial['held_after_cut'] = leases(ctx, name, role)
                ctx.stop(name, role)
                ctx.assertion(not leases(ctx, name, role), 'verified stop did not settle held cancellation')
                ctx.lifecycle('start', name, role)
            finally:
                for client in clients:
                    client.close()
    ctx.assertion(len(trials) == 6, 'six cancellation repeats required')


def held_restart(ctx, version, role, evidence):
    name = 'q-' + ctx.row_id.lower() + '-held'
    launch(ctx, name, version, role, extra=dict(engine_config=dict(context_length=8192, max_concurrent_requests=32)))
    before = live_identities(ctx, name, role)
    clients = []
    frozen = False
    try:
        signal_owned(ctx, name, role, signal.SIGSTOP)
        frozen = True
        for number in range(32):
            client = Client(ctx, name, role, stream=False)
            clients.append(client)
            until(ctx, lambda: len(leases(ctx, name, role)) >= number + 1, 10)
            ctx.assertion(not client.output.is_set(), 'held restart fixture unexpectedly received output')
            client.close()
            until(ctx, lambda: len([item for item in leases(ctx, name, role) if item['answered'] == 0]) >= number + 1, 10)
        evidence['held32'] = leases(ctx, name, role)
        signal_owned(ctx, name, role, signal.SIGCONT)
        frozen = False
        def replaced():
            current = ctx.owned(name, role)
            return current and not any(item in before for item in current) and current
        evidence['restart_identities'] = until(ctx, replaced, 600)
        until(ctx, lambda: ctx.status(name, role).get('observed_state', '').lower() == 'ready', 600)
        response = ctx.request(name, role=role)
        answered(ctx, response)
        events = ctx.events(role)
        ctx.assertion(any('held_request' in json.dumps(event) and 'restart' in json.dumps(event) for event in events), 'held-request restart was not journaled')
        ctx.assertion(not leases(ctx, name, role), 'held requests survived verified restart')
        evidence.update(restart_events=events, post_restart=response)
    finally:
        if frozen:
            for item in before:
                from session import matches
                if not matches(item):
                    continue
                try:
                    os.kill(item['pid'], signal.SIGCONT)
                except ProcessLookupError:
                    pass
        for client in clients:
            client.close()
        ctx.stop(name, role)


def management(ctx, name, role, kind):
    from v020 import credentials
    info = ctx.role(role)
    deployment = ctx.sql('SELECT id FROM deployments WHERE name=?', (name,), role)[0]['id']
    connection = http.client.HTTPConnection('127.0.0.1', info['port'] + 1, timeout=min(5, ctx.remaining()))
    try:
        connection.request('GET', '/management/v1/metrics/' + kind + '?deployment=' + deployment,
                           headers={'Authorization': 'Bearer ' + credentials(info['cred'])['admin_token']})
        response = connection.getresponse()
        body = response.read(4 * 1024 * 1024 + 1)
        ctx.assertion(response.status == 200 and len(body) <= 4 * 1024 * 1024, 'remote ' + kind + ' report unavailable')
        return json.loads(body)
    finally:
        connection.close()


def host_path(ctx, version, evidence):
    role = 'tf-' + ctx.row_id.lower() + '-server'
    name = 'q-' + ctx.row_id.lower() + '-remote'
    launch(ctx, name, version, role)
    clients = []
    try:
        idle = until(ctx, lambda: report_idle(management(ctx, name, role, 'load')))
        clients = [Client(ctx, name, role, True) for _ in range(4)]
        busy = until(ctx, lambda: report_busy(management(ctx, name, role, 'load')))
        direct = scrape(ctx, name, role)
        pools = samples(direct['text'], 'tensorfold:kv_cache_usage_ratio')
        ctx.assertion(pools and all(item['labels'] for item in pools), 'per-pool native KV samples absent')
        until(ctx, lambda: all(client.output.is_set() for client in clients))
        event_start = len(ctx.events(role))
        deployment_id = ctx.sql('SELECT id FROM deployments WHERE name=?', (name,), role)[0]['id']
        for client in clients:
            client.close()
        held = [item for item in leases(ctx, name, role) if item['answered'] == 1]
        until(ctx, lambda: not leases(ctx, name, role), 300)
        settled = until(ctx, lambda: report_idle(management(ctx, name, role, 'load')))
        receipts = until(ctx, lambda: remote_settlement(ctx.events(role)[event_start:], deployment_id), 30)
        response = ctx.request(name, role=role)
        answered(ctx, response)
        native = response['body'].get('tensorfold', {})
        # T41 / ADR 0039 §11: engine fields retain upstream names and seconds.
        ctx.assertion(all(isinstance(native.get(key), (int, float)) and not isinstance(native[key], bool)
                          and math.isfinite(native[key]) and native[key] >= 0
                          for key in ('time_to_first_token', 'prefill_seconds')), 'native per-request timings absent')
        latency = until(ctx, lambda: forwarded_histograms(management(ctx, name, role, 'latency')))
        evidence.update(remote_idle=idle, remote_busy=busy, pool_samples=pools,
                        cancelled_answered=held, settlement_receipts=receipts,
                        remote_settled=settled, latency=latency, response=response)
    finally:
        for client in clients:
            client.close()
        ctx.stop(name, role)


def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)


def report_idle(report):
    readings = [item['sample']['engine'] for item in objects(report)
                if isinstance(item.get('sample'), dict) and item['sample'].get('fresh') is True
                and isinstance(item['sample'].get('engine'), dict)]
    return report if readings and all(item.get('running') == item.get('waiting') == 0
                                      and item.get('kv_usage_ppm') is not None for item in readings) else None


def report_busy(report):
    readings = [item['sample']['engine'] for item in objects(report)
                if isinstance(item.get('sample'), dict) and item['sample'].get('fresh') is True
                and isinstance(item['sample'].get('engine'), dict)]
    return report if readings and any(isinstance(item.get('running'), int) and item['running'] > 0 for item in readings) else None


def forwarded_histograms(report):
    series = {item.get('name'): item for item in objects(report)
              if item.get('source') == 'engine' and item.get('tier') == 'engine'
              and isinstance(item.get('count'), int) and item['count'] > 0 and item.get('buckets')}
    return report if all(name in series for name in ('engine_time_to_first_token', 'engine_e2e_request_latency')) else None


def remote_settlement(events, deployment_id):
    receipts = []
    for event in events:
        if event.get('kind') != 'request_cancellation_acknowledged':
            continue
        body = event.get('payload_json', {})
        body = json.loads(body) if isinstance(body, str) else body
        if body.get('deployment_id') == deployment_id and isinstance(body.get('count'), int) and body['count'] > 0:
            receipt = body.get('receipt', '')
            if ('the host reported' in receipt and '0 running and 0 waiting and 0 in flight' in receipt
                    and 'after the request was cancelled' in receipt):
                receipts.append(body)
    return receipts if sum(item['count'] for item in receipts) >= 4 else None


def legacy(ctx, evidence):
    name = 'q-tf9-legacy'
    ctx.register('TENSORFOLD_100')
    ctx.deploy(name, 'TENSORFOLD_066', model='tensorfold')
    clients = []
    try:
        port = endpoint(ctx, name, 'standalone')
        health = get(ctx, port, '/health')
        body = json.loads(health['body'])
        ctx.assertion(health['status'] == 200 and body.get('ok') is True and
                      body.get('busy') is False and body.get('requests_running') == 0,
                      '0.6.6 idle health contract changed')
        clients = [Client(ctx, name, 'standalone', True) for _ in range(4)]
        def busy():
            response = get(ctx, port, '/health')
            doc = json.loads(response['body']) if response['status'] == 200 else {}
            return response if doc.get('busy') is True and doc.get('requests_running', 0) > 0 else None
        busy_health = until(ctx, busy)
        for client in clients:
            client.close()
        ctx.stop(name)
        ctx.assertion(not ctx.owned(name), 'legacy stop did not prove process exit')
        evidence.update(idle_health=health, busy_health=busy_health,
                        profiles=ctx.cli('engine', 'list', '--json').stdout, events=ctx.events())
    finally:
        for client in clients:
            client.close()
        if ctx.owned(name):
            ctx.stop(name)


def trace_signals(text):
    """strace -ttt uses wall seconds; retain only signal and exit timestamps."""
    result = []
    for line in text.splitlines():
        match = re.search(r'(?:^|\s)(\d+\.\d+)\s+(.*)$', line)
        if match and ('--- SIGTERM ' in match[2] or '+++ exited with ' in match[2]
                      or '+++ killed by ' in match[2]):
            result.append(dict(at=float(match[1]), signal='SIGTERM' if '--- SIGTERM ' in match[2] else 'exit'))
    return result


@contextlib.contextmanager
def signal_trace(ctx, name, role, label):
    # The launcher has no timestamped send event. Only signal delivery and exit
    # are traced: never request bytes, command arguments, environment or keys.
    identities = live_identities(ctx, name, role)
    binary = ctx.installation('STRACE')
    path = ctx.root / (label + '-signals.log')
    argv = [binary, '-f', '-ttt', '-e', 'trace=none', '-e', 'signal=TERM', '-o', str(path)]
    for item in identities:
        argv.extend(('-p', str(item['pid'])))
    with (ctx.root / (label + '-trace-attach.log')).open('wb') as log:
        proc = subprocess.Popen(argv, stdout=log, stderr=log)
    try:
        def attached():
            ctx.assertion(proc.poll() is None, 'static signal tracer could not attach; ptrace permission is required')
            return all(re.search(r'TracerPid:\s+[1-9]\d*', (Path('/proc') / str(item['pid']) / 'status').read_text())
                       for item in identities)
        until(ctx, attached, 10)
        yield path
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)


def stop_protocol(ctx, version, evidence):
    name = 'q-' + ctx.row_id.lower() + '-stop'
    launch(ctx, name, version)
    refusal = ctx.cli('park', 'deployment', name, check=False)
    ctx.assertion(refusal.returncode == 5 and 'unsupported_capability' in (refusal.stdout + refusal.stderr),
                  'TensorFold park was not refused as unsupported')
    clients = []
    observations = []
    port = endpoint(ctx, name, 'standalone')
    try:
        clients = [Client(ctx, name, 'standalone', True, tokens=512) for _ in range(8)]
        def eight():
            value = scrape(ctx, name)
            value.pop('text')
            observations.append(value)
            return value['running'] == 8
        until(ctx, eight)
        with signal_trace(ctx, name, 'standalone', name) as trace:
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                stopped = pool.submit(ctx.stop, name)
                while not stopped.done():
                    response = get(ctx, port, '/metrics')
                    health = get(ctx, port, '/health')
                    sample = dict(at=time.time(), health=health)
                    if response['status'] == 200:
                        sample.update(work(response['body']))
                    observations.append(sample)
                    time.sleep(.05)
                stopped.result()
            for client in clients:
                until(ctx, client.finished.is_set, 10)
                ctx.assertion(client.error is None, 'drain interrupted a live stream')
                answered(ctx, client.result, True)
            events = trace_signals(trace.read_text())
            terms = [item['at'] for item in events if item['signal'] == 'SIGTERM']
            exits = [item['at'] for item in events if item['signal'] == 'exit']
            ctx.assertion(terms and exits and min(exits) >= min(terms), 'missing measured SIGTERM delivery and exit')
            zero = [item for item in observations if item.get('running') == item.get('waiting') == 0
                    and item['at'] <= min(terms)]
            ctx.assertion(zero, 'both native gauges were not observed zero before SIGTERM')
            ctx.assertion(not ctx.owned(name), 'native process ownership survived stop')
            if version == '1.1.5':
                ctx.assertion(any(item.get('health', {}).get('status') == 503 and
                                  'draining' in item['health']['body'] for item in observations),
                              '1.1.5 health 503/draining was not observed during stop')
            evidence.update(drain_samples=observations, signal_events=events,
                            signal_to_exit_s=max(exits) - min(terms), park_refusal=refusal.returncode)
    finally:
        for client in clients:
            client.close()
    # Bypass the router so CapyCTL's own leases are drained while the engine
    # still has real work. This is not a fake busy gauge or a stopped HTTP server.
    ctx.lifecycle('start', name)
    port = endpoint(ctx, name, 'standalone')
    busy_client = None
    try:
        busy_client = DirectClient(ctx, port, name, tokens=30000)
        until(ctx, lambda: scrape(ctx, name)['running'] > 0)
        with signal_trace(ctx, name, 'standalone', name + '-busy') as trace:
            start = time.monotonic()
            result = ctx.cli('stop', 'deployment', name, check=False)
            ctx.assertion(result.returncode == 0, 'busy stop fixture was not accepted')
            def uncertain():
                operation = ctx.sql('''SELECT r.state FROM operation_runs r JOIN operations o ON o.id=r.operation_id
                    JOIN deployments d ON d.id=o.deployment_id
                    WHERE d.name=? AND o.kind='stop' ORDER BY o.rowid DESC LIMIT 1''', (name,))
                return operation if operation and operation[0]['state'] == 'uncertain' else None
            operation = until(ctx, uncertain, 60)
            elapsed = time.monotonic() - start
            gauge = scrape(ctx, name)
            events = trace_signals(trace.read_text())
            ctx.assertion(29.5 <= elapsed <= 60, 'busy stop did not reach the 30-second refusal bound')
            ctx.assertion(gauge['running'] + gauge['waiting'] > 0, 'busy bound fixture finished too early')
            ctx.assertion(not any(item['signal'] == 'SIGTERM' for item in events), 'busy engine was signalled at the bound')
            ctx.assertion(ctx.owned(name), 'busy engine lost ownership')
            diagnostic = (result.stdout + result.stderr + json.dumps(ctx.status(name))).lower()
            ctx.assertion('uncertain' in diagnostic, 'busy stop did not report uncertainty')
            evidence.update(busy_bound_s=elapsed, busy_bound_work=gauge, busy_signal_events=events,
                            busy_result=dict(exit_code=result.returncode, diagnostic=diagnostic))
    finally:
        if busy_client is not None:
            busy_client.close()
        until(ctx, lambda: scrape(ctx, name)['running'] == scrape(ctx, name)['waiting'] == 0, 300)
        ctx.stop(name)


class DirectClient(Client):
    def __init__(self, ctx, port, name, tokens):
        self.connection = http.client.HTTPConnection('127.0.0.1', port, timeout=min(300, ctx.remaining()))
        self.output, self.finished = threading.Event(), threading.Event()
        self.result, self.error, self.cut = {}, None, False
        self.connection.request('POST', '/v1/chat/completions',
                                json.dumps(dict(model=name, messages=[dict(role='user', content='Write a long novel without ending.')],
                                                max_tokens=tokens, stream=True, ignore_eos=True)),
                                {'Content-Type': 'application/json'})
        self.socket = self.connection.sock
        self.thread = threading.Thread(target=self.read, args=(True,), daemon=True)
        self.thread.start()


def configuration(ctx, name, version, extra=None):
    """The ordinary deployment document, without starting on a refusal path."""
    profile = ctx.register('TENSORFOLD_' + version.replace('.', ''))
    value = dict(schema_version=1, kind='deployment', name=name, engine=profile,
                 model='tensorfold', api='chat', residency='restart_only', devices=[dict(id='gpu0')],
                 resources=dict(gpu='30GiB', ram='2GiB'), timeouts=dict(initialize='1800s'),
                 engine_config=dict(context_length=32768, max_concurrent_requests=8))
    def merge(left, right):
        for key, item in right.items():
            if isinstance(item, dict) and isinstance(left.get(key), dict):
                merge(left[key], item)
            else:
                left[key] = item
    merge(value, extra or {})
    return value


def install(ctx, value, check=True):
    from v020 import atomic_json
    path = ctx.root / (value['name'] + '.json')
    atomic_json(path, value)
    result = ctx.cli('deploy', 'model', '--file', path, check=check)
    if result.returncode == 0:
        ctx.deployments.append(('standalone', value['name']))
    return result


def refused(ctx, version, label, extra, needles):
    value = configuration(ctx, 'q-' + ctx.row_id.lower() + '-' + label, version, extra)
    if 'topology' in extra:
        value.pop('devices', None)
        # T41 / ADR 0039 §6: reach the topology gate, not the short-form
        # resource validator (which requires one selected device).
        value['resources'] = dict(ram='2GiB')
    result = install(ctx, value, check=False)
    diagnostic = (result.stdout + result.stderr).lower()
    ctx.assertion(result.returncode != 0, label + ' was not refused at resolution')
    ctx.assertion(all(needle.lower() in diagnostic for needle in needles), label + ' refusal did not name its native criterion')
    if 'topology' in extra:
        code = ('group_shape_unsupported:tensorfold' if len(extra.get('placement', {}).get('hosts', [])) > 1
                else 'multi_gpu_unsupported')
        ctx.assertion(re.search(r'\b' + re.escape(code) + r'(?=:\s|\s|$)', diagnostic) is not None,
                      label + ' refusal did not return exactly ' + code)
    ctx.assertion(not ctx.owned(value['name']), 'resolution refusal created process ownership')
    return dict(exit_code=result.returncode, diagnostic=diagnostic)


def start_measurements(ctx, evidence):
    from .measurements import sampling, validate_samples, metric_values, engine_logs, tensorfold_startup_sizes
    name = 'q-tf12-native'
    version = '1.1.5'
    evidence['idle_unload_refusal'] = refused(ctx, version, 'idle-unload',
        dict(engine_config=dict(accept_extra_args=True, extra_args=['--idle-unload', '60'])), ['idle-unload', 'reserved'])
    install(ctx, configuration(ctx, name, version))
    for kind in ('cold', 'warm'):
        progression = []
        baseline = ctx.sample()
        with sampling(ctx) as memory:
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                started_at = time.monotonic()
                start = pool.submit(ctx.cli, 'start', 'deployment', name, '--wait')
                while not start.done():
                    try:
                        port = endpoint(ctx, name, 'standalone')
                    except AssertionError:
                        time.sleep(.05)
                        continue
                    progression.append(get(ctx, port, '/health'))
                    time.sleep(.05)
                start.result()
                ready_s = time.monotonic() - started_at
            port = endpoint(ctx, name, 'standalone')
            ready = get(ctx, port, '/health')
            progression.append(ready)
            health = json.loads(ready['body'])
            ctx.assertion(any(item['status'] is None for item in progression), 'readiness did not demonstrate refused connections')
            ctx.assertion(ready['status'] == 200 and health.get('status') == 'ok' and
                          health.get('max_batch_size') == 8 and health.get('version') == '1.1.5' and
                          name in health.get('model_ids', []), '1.1.5 ready health fields disagree')
            identities = live_identities(ctx, name, 'standalone')
            native = next(item for item in identities if Path('/proc', str(item['pid']), 'exe').resolve().name == 'tensorfold-native')
            argv = Path('/proc', str(native['pid']), 'cmdline').read_bytes().decode().rstrip('\0').split('\0')
            root = Path(ctx.installation('TENSORFOLD_115'))
            expected = [str(root / 'bin/tensorfold-native'), 'serve', str(Path(ctx.installation('MODEL_TENSORFOLD')).resolve()),
                        '--name', name, '--host', '127.0.0.1', '--port', str(port), '--no-update-check',
                        '--backend', 'cuda', '--snapshot-dir', 'none', '--context', '32768', '--parallel', '8']
            # The model-store symlink is canonicalised by resolution; binary
            # symlinks are compared canonically as well, not by display spelling.
            ctx.assertion(Path(argv[0]).resolve() == Path(expected[0]).resolve() and argv[1] == 'serve'
                          and Path(argv[2]).resolve() == Path(expected[2]).resolve() and argv[3:] == expected[3:],
                          'rendered 1.1.5 command is not exactly ADR 0039 §4')
            full = ctx.bench(name, concurrency=8, rounds=1,
                             body=dict(messages=[dict(role='user', content=' hello' * 32700)], max_tokens=32, temperature=0))
            ctx.assertion(full['ok'] == 8 and all(item['body'].get('usage', {}).get('prompt_tokens', 0) >= 32700
                                               for item in full['results']), 'eight requests did not fill the configured context')
            decode = {str(n): ctx.bench(name, concurrency=n, rounds=1, tokens=256) for n in (1, 8)}
            ctx.assertion(all(result['ok'] == result['requests'] and result['tokens_per_s'] > 0
                              for result in decode.values()), '1/8-stream throughput missing')
            metrics = metric_values(get(ctx, port, '/metrics')['body'])
            pids = {item['pid'] for item in live_identities(ctx, name, 'standalone')}
            process_rows = ctx.command(['nvidia-smi', '--query-compute-apps=pid,used_memory', '--format=csv,noheader,nounits']).stdout
            process_memory = []
            for line in process_rows.splitlines():
                fields = [field.strip() for field in line.split(',')]
                if len(fields) == 2 and fields[0].isdigit() and int(fields[0]) in pids:
                    ctx.assertion(fields[1].isdigit(), 'owned native NVML process memory is not numeric')
                    process_memory.append(dict(pid=int(fields[0]), device_bytes=int(fields[1]) * 2**20))
            ctx.assertion(process_memory and sum(item['device_bytes'] for item in process_memory) > 0,
                          'native per-process device memory missing')
        validate_samples(ctx, memory)
        sizing = tensorfold_startup_sizes(engine_logs(ctx, name))
        peaks = [number for key, values in metrics.items() if 'device_memory_peak_bytes' in key
                 for number in (values if isinstance(values, list) else [values])]
        ctx.assertion(sizing['stream_count'] == 8 and abs(sizing['stream_bytes'] - .29 * 2**30) < .05 * 2**30
                      and peaks and max(peaks) > sizing['model_bytes'], 'startup sizing or device peak missing')
        evidence[kind] = dict(ready_s=ready_s, health_progression=progression, argv=argv, startup=sizing,
                              memory=memory, baseline=baseline, process_device_memory=process_memory,
                              device_metrics=metrics, full_context=full, decode=decode)
        ctx.stop(name)
    legacy_name = 'q-tf12-baseline'
    ctx.deploy(legacy_name, 'TENSORFOLD_066', model='tensorfold', extra=dict(resources=dict(gpu='30GiB', ram='2GiB')))
    try:
        baseline_decode = {str(n): ctx.bench(legacy_name, concurrency=n, rounds=1, tokens=256) for n in (1, 8)}
        ctx.assertion(all(value['ok'] == value['requests'] and value['tokens_per_s'] > 0 for value in baseline_decode.values()),
                      '0.6.6 same-checkpoint throughput comparison missing')
        evidence['legacy_decode'] = baseline_decode
    finally:
        ctx.stop(legacy_name)


def reproof(ctx, evidence):
    name = 'q-tf10-native'
    launch(ctx, name)
    before = live_identities(ctx, name, 'standalone')
    port = endpoint(ctx, name, 'standalone')
    sockets_before = listening_sockets(ctx, before)
    ctx.assertion(sockets_before == [dict(host='127.0.0.1', port=port)], 'native listener set differs after readiness')
    info = ctx.role()
    process = info['process']
    # Crash only the recorded controller, preserving its durable engine
    # ownership. Restarting that state invokes adoption and controller re-proof.
    process.kill()
    process.wait(timeout=10)
    info['process'] = ctx.spawn([info['binary'], '--state-dir', info['state'], 'start', 'standalone',
        '--listen', '127.0.0.1:19753', '--management-listen', '127.0.0.1:19754',
        '--engine-ports', '19790-19819', '--models-root', ctx.model_store,
        '--model-sources-path', ctx.root / 'sources', '--json'], 'tf10-reproof')
    def ready():
        ctx.assertion(info['process'].poll() is None, 're-proof controller exited before ready')
        result = ctx.cli('status', 'deployment', name, '--json', check=False)
        if result.returncode:
            ctx.assertion('management_unavailable' in result.stderr,
                          're-proof status failed: ' + result.stderr)
            return None
        return json.loads(result.stdout).get('observed_state', '').lower() == 'ready'
    until(ctx, ready, 300)
    after = live_identities(ctx, name, 'standalone')
    ctx.assertion(before == after, 'controller re-proof replaced rather than adopted the owned launch')
    sockets_after = listening_sockets(ctx, after)
    ctx.assertion(sockets_after == sockets_before, 'native listener set changed at controller re-proof')
    response = ctx.request(name)
    answered(ctx, response)
    evidence.update(identities=after, after_readiness=sockets_before, after_reproof=sockets_after,
                    response=response, events=ctx.events())
    ctx.stop(name)
    # isolated_role owns the replacement controller's lifetime, including
    # failure paths; leave its management socket available for cleanup.


def switching(ctx, evidence):
    name, other = 'q-tf8-native', 'q-tf8-vllm'
    # T41 / ADR 0039 TF8: size against the published managed pool, not
    # physical RAM. Each grant fits alone and their sum cannot fit.
    limits = []
    for row in ctx.sql('SELECT policy_json FROM host_resource_policies'):
        for domain in json.loads(row['policy_json']).get('domains', {}).values():
            if domain.get('memory') == 'unified':
                value = domain['managed_limit']
                limits.append(value if isinstance(value, int) else int(value.removesuffix('B')))
    ctx.assertion(len(limits) == 1, 'one unified managed memory domain required')
    gpu_bytes = limits[0] * 3 // 5 - 2 * 2**30
    ctx.assertion(gpu_bytes >= 30 * 2**30, 'managed pool is too small for the TF8 checkpoints')
    resources = dict(gpu=str(gpu_bytes) + 'B', ram='2GiB')
    evidence['managed_limit_bytes'] = limits[0]
    evidence['each_grant_bytes'] = gpu_bytes + 2 * 2**30
    launch(ctx, name, extra=dict(resources=resources))
    before = live_identities(ctx, name, 'standalone')
    other_spec = configuration(ctx, other, '1.0.0')
    other_spec.update(engine=ctx.register('vllm032'), model='chat', resources=resources,
                      engine_config=dict(context_length=32768, max_concurrent_requests=8, memory=dict(kv_cache='5GiB')))
    install(ctx, other_spec)
    first = ctx.request(other)
    answered(ctx, first)
    ctx.assertion(not ctx.owned(name), 'mutually exclusive vLLM grant retained native ownership')
    start = time.monotonic()
    second = ctx.request(name)
    answered(ctx, second)
    elapsed = time.monotonic() - start
    after = live_identities(ctx, name, 'standalone')
    ctx.assertion(not ctx.owned(other) and all(item not in before for item in after),
                  'native wake was not a fresh mutually exclusive launch')
    ctx.assertion(elapsed < 1800, 'fresh native wake exceeded Initialize')
    canary = passing_canary(ctx, name)
    text_reference = ctx.sql('''SELECT w.kind,w.tokens_json,w.text FROM wake_canaries w
        JOIN deployments d ON d.id=w.deployment_id WHERE d.name=? ORDER BY w.generation DESC LIMIT 1''', (name,))
    ctx.assertion(text_reference and text_reference[0]['kind'] == 'completion' and
                  json.loads(text_reference[0]['tokens_json']) == [] and text_reference[0]['text'],
                  'native completion canary did not use text without token IDs')
    canary['text_reference'] = text_reference
    events = ctx.events()
    evidence.update(first=first, second=second, before=before, after=after, wake_s=elapsed, canary=canary, events=events)
    ctx.stop(name)
    ctx.stop(other)


def admission(ctx, evidence):
    from .measurements import engine_logs, tensorfold_startup_sizes
    version = '1.0.0'
    failures = {
        'kv-dtype': (dict(engine_config=dict(kv_cache_dtype='int8')), ['kv_cache_dtype', 'native']),
        'draft': (dict(model=dict(path='tensorfold', draft=dict(type='local', path=str(ctx.model_store / 'chat')))), ['model.draft', 'native']),
        'topology-one-host': (dict(topology=dict(tensor_parallel=2)), ['multi_gpu_unsupported']),
        'topology-multi-host': (dict(topology=dict(tensor_parallel=2), placement=dict(hosts=['a', 'b'])),
                                ['group_shape_unsupported:tensorfold']),
        'parking': (dict(residency='deep'), ['residency', 'restart']),
        'zero-compaction': (dict(retention='zero', engine_config=dict(accept_extra_args=True, extra_args=['--compact-at', 'auto'])),
                            ['retention_zero:compaction']),
    }
    evidence['resolution_refusals'] = {key: refused(ctx, version, key, extra, needles)
                                       for key, (extra, needles) in failures.items()}
    for label, extra, needles in (
        ('family', dict(model='chat'), ['does not list', '0.6']),
        ('sixteen', dict(engine_config=dict(context_length=262144, max_concurrent_requests=16)),
         ['max_concurrent_requests', 'context_length', 'resources']),
    ):
        name = 'q-tf4-' + label
        install(ctx, configuration(ctx, name, version, extra))
        status = ctx.status(name)
        if label == 'family':
            text = json.dumps(status).lower()
            ctx.assertion('uncertified' in text and ('does not list' in text or 'unlisted' in text),
                          'unlisted BF16 family did not resolve uncertified with its note')
            model = json.loads((Path(ctx.installation('MODEL_CHAT')) / 'config.json').read_text())
            ctx.assertion(model.get('model_type') == 'qwen3' and
                          model.get('torch_dtype', model.get('dtype')) in ('bfloat16', 'bf16'),
                          'unlisted-family fixture is not Qwen3 BF16')
        result = ctx.cli('start', 'deployment', name, '--wait', check=False)
        diagnostic = (result.stdout + result.stderr + ctx.logs(name) + json.dumps(ctx.status(name))).lower()
        ctx.assertion(result.returncode != 0 and all(needle in diagnostic for needle in needles),
                      label + ' engine refusal lacks its native fix/note')
        ctx.assertion(not ctx.owned(name), label + ' failed launch retained a live process')
        evidence[label] = dict(exit_code=result.returncode, diagnostic=diagnostic)
    compact = 'q-tf4-compact'
    launch(ctx, compact, extra=dict(resources=dict(gpu='30GiB', ram='2GiB'),
        engine_config=dict(accept_extra_args=True, extra_args=['--compact-at', 'auto'])))
    response = ctx.request(compact)
    answered(ctx, response)
    evidence['compact_auto'] = response
    ctx.stop(compact)
    ordinary = 'q-tf4-reserve-baseline'
    launch(ctx, ordinary, extra=dict(resources=dict(gpu='30GiB', ram='2GiB')))
    sizing = tensorfold_startup_sizes(engine_logs(ctx, ordinary))
    identities = live_identities(ctx, ordinary, 'standalone')
    native = next(item for item in identities if Path('/proc', str(item['pid']), 'exe').resolve().name == 'tensorfold-native')
    command = Path('/proc', str(native['pid']), 'cmdline').read_bytes().decode().rstrip('\0').split('\0')
    environment = dict(item.split('=', 1) for item in Path('/proc', str(native['pid']), 'environ').read_bytes().decode().split('\0') if '=' in item)
    ctx.assertion(environment.get('TENSORFOLD_MEMORY_RESERVE_GIB') == '2', 'managed reserve 2 was not rendered')
    ctx.assertion('--compact-at' not in command, 'ordinary launch unexpectedly enables compaction')
    ctx.stop(ordinary)
    need = sizing['model_bytes'] + sizing['stream_count'] * sizing['stream_bytes']
    available = ctx.sample()['mem_available_bytes']
    # A real, CapyCTL-owned vLLM deployment holds the pressure. No external
    # allocation, installed-environment mutation, or fake memory evidence.
    target = need + 7 * 2**30
    chat_root = Path(ctx.installation('MODEL_CHAT'))
    weight_bytes = sum(path.stat().st_size for path in chat_root.glob('*.safetensors'))
    kv = int(available - target - weight_bytes - 2 * 2**30)
    ctx.assertion(kv > 2**30, 'not enough unified memory for the bounded admission fixture')
    holder = 'q-tf4-holder'
    ctx.deploy(holder, 'vllm032', extra=dict(resources=dict(gpu=str(kv + weight_bytes + 4 * 2**30) + 'B', ram='2GiB'),
               engine_config=dict(memory=dict(kv_cache=str(kv) + 'B'))))
    try:
        free = ctx.sample()['mem_available_bytes']
        ctx.assertion(need + 2 * 2**30 < free < need + 12 * 2**30,
                      'holder did not establish memory between managed and default-reserve launch needs')
        # The scratch comparison is the ADR's explicit by-hand launch, in its
        # own session process group and with identity recorded by Context.spawn.
        # It is never permitted to keep running on a timeout.
        command[command.index('--name') + 1] = 'q-tf4-scratch'
        scratch_port = 19827
        command[command.index('--port') + 1] = str(scratch_port)
        environment.pop('TENSORFOLD_MEMORY_RESERVE_GIB', None)
        scratch_log = ctx.root / 'tf4-scratch.log'
        with scratch_log.open('wb') as log:
            scratch = subprocess.Popen(command, stdout=log, stderr=log, env=environment, start_new_session=True)
        from session import identity
        owned_scratch = identity(scratch.pid)
        ctx.assertion(owned_scratch, 'scratch launch identity missing')
        with (ctx.root / 'processes.jsonl').open('a') as stream:
            stream.write(json.dumps(owned_scratch) + '\n')
        try:
            try:
                code = scratch.wait(timeout=min(180, ctx.remaining()))
            except subprocess.TimeoutExpired:
                raise AssertionError('default-reserve scratch launch did not refuse within its budget')
            diagnostic = scratch_log.read_text(errors='replace')
            ctx.assertion(code != 0 and 'memory' in diagnostic.lower() and
                          ('reserve' in diagnostic.lower() or 'available' in diagnostic.lower()),
                          'scratch failure was not the default reserve refusal')
        finally:
            if scratch.poll() is None:
                current = identity(scratch.pid)
                if current == owned_scratch:
                    scratch.terminate()
                    scratch.wait(timeout=30)
        launch(ctx, ordinary, extra=dict(resources=dict(gpu='30GiB', ram='2GiB')))
        ctx.assertion(ctx.owned(holder), 'admission pressure holder was evicted rather than retained')
        response = ctx.request(ordinary)
        answered(ctx, response)
        evidence['reserve'] = dict(need_bytes=need, free_bytes=free, sizing=sizing, reserve_gib=2,
                                   scratch_exit=code, scratch_diagnostic=diagnostic, managed_answer=response,
                                   holder_identities=ctx.owned(holder))
        ctx.stop(ordinary)
    finally:
        ctx.stop(holder)


def run(ctx, row):
    evidence = {}
    try:
        version = '1.1.5' if row['id'] in ('TF12', 'TF13', 'TF14') else '1.0.0'
        with isolated_role(ctx, row['id'].lower()):
            if row['id'] == 'TF9':
                legacy(ctx, evidence)
            elif row['id'] in ('TF6', 'TF14'):
                name = 'q-' + row['id'].lower() + '-cuts'
                launch(ctx, name, version)
                cut_protocol(ctx, name, 'standalone', evidence)
                ctx.stop(name)
                held_restart(ctx, version, 'standalone', evidence)
                if row['id'] == 'TF14':
                    host_path(ctx, version, evidence)
            elif row['id'] == 'TF7':
                host_path(ctx, version, evidence)
            elif row['id'] in ('TF5', 'TF13'):
                stop_protocol(ctx, version, evidence)
            elif row['id'] == 'TF10':
                reproof(ctx, evidence)
            elif row['id'] == 'TF12':
                start_measurements(ctx, evidence)
            elif row['id'] == 'TF8':
                switching(ctx, evidence)
            elif row['id'] == 'TF4':
                admission(ctx, evidence)
        ctx.save('native', evidence)
        return dict(verdict='pass', reason='All ADR 0039 ' + row['id'] + ' live criteria observed', evidence=evidence)
    except (AssertionError, OSError, ValueError, KeyError, TimeoutError) as error:
        ctx.save('native-failure', evidence)
        return dict(verdict='fail', reason=str(error), evidence=evidence)
