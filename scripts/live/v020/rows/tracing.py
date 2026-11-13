"""ADR 0037 OT1–OT10: single-machine wire and exported-span qualification.

The loopback OTLP/HTTP receiver uses protobuf from the fresh tracing environment,
not a mock exporter. No engine environment is changed by a row.
"""
import base64
import concurrent.futures
import datetime
import gzip
import importlib.util
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import threading
import time
import uuid

ENDPOINT = 'http://127.0.0.1:14318/v1/traces'
TRACE = '1234567890abcdef1234567890abcdef'
PARENT = '1234567890abcdef'
SAMPLED = '00-' + TRACE + '-' + PARENT + '-01'
ROWS = [dict(id='OT'+str(number), priority=4, budget=budget, criteria=criteria, measurement=number in (4, 7))
        for number, budget, criteria in (
            (1, 1800, 'ADR0037 OT1: default off; no tracing launch options, environment names or collector connections.'),
            (2, 2400, 'ADR0037 OT2: streamed/collected chat and decisions, caller/new/malformed traces and request IDs.'),
            (3, 2400, 'ADR0037 OT3: all content sentinels absent from CapyCTL and both engines, including quoted validation errors.'),
            (4, 2400, 'ADR0037 OT4: cold launch, linked wake/readiness/canary, forced-switch drain/release and <=1ms timing agreement.'),
            (5, 2400, 'ADR0037 OT5: vLLM/SGLang span parentage, SGLang request ID and llama.cpp no trace header.'),
            (6, 3000, 'ADR0037 OT6: separately enrolled server/host on loopback; ingress, wake and endpoint-free propagation.'),
            (7, 1800, 'ADR0037 OT7: collector outage under traffic, OT8 noise comparison, bounded notices, dropped counters and resumed export.'),
            (9, 1800, 'ADR0037 OT9: retention zero disables engine tracing and trace headers, while CapyCTL exports.'),
            (10, 1200, 'ADR0037 OT10: independently verified SDK-free SGLang serves with not_installed.'))]


def span_tools():
    # run.sh stages this shared matrix helper beside v020.py on the rented VM.
    path = Path(__file__).resolve().parents[1] / 'otel_spans.py'
    if not path.is_file():
        path = Path(__file__).resolve().parents[2] / 'matrix/otel_spans.py'
    spec = importlib.util.spec_from_file_location('qualification_otel_spans', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def receiver_main(output):
    """Real protobuf decoding; keep complete OTLP data for the privacy assertion."""
    from http.server import BaseHTTPRequestHandler, HTTPServer
    from google.protobuf.json_format import MessageToDict
    from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import ExportTraceServiceRequest

    def hex_ids(value):
        if isinstance(value, dict):
            return {key: base64.b64decode(item).hex() if key in ('traceId', 'spanId', 'parentSpanId')
                    else hex_ids(item) for key, item in value.items()}
        if isinstance(value, list):
            return [hex_ids(item) for item in value]
        return value

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            try:
                if self.path != '/v1/traces':
                    raise ValueError('unexpected OTLP path')
                length = int(self.headers['Content-Length'])
                if not 0 < length <= 16 * 1024 * 1024:
                    raise ValueError('OTLP payload size')
                raw = self.rfile.read(length)
                if self.headers.get('Content-Encoding') == 'gzip':
                    raw = gzip.decompress(raw)
                request = ExportTraceServiceRequest()
                request.ParseFromString(raw)
                doc = hex_ids(MessageToDict(request))
                with open(output, 'a', encoding='utf-8') as stream:
                    stream.write(json.dumps(doc, separators=(',', ':')) + '\n')
                self.send_response(200)
                self.send_header('Content-Type', 'application/x-protobuf')
                self.send_header('Content-Length', '0')
                self.end_headers()
            except Exception:
                self.send_error(400, 'invalid OTLP request')

    HTTPServer(('127.0.0.1', 14318), Handler).serve_forever()


class Collector:
    def __init__(self, ctx, label):
        self.ctx, self.label = ctx, label
        self.path = ctx.evidence / (ctx.row_id + '-' + label + '-otlp.jsonl')
        self.process = None

    def start(self):
        python = Path(self.ctx.installation('SGLANG_0523_TRACING')) / 'bin/python'
        self.process = self.ctx.spawn([python, Path(__file__), '--receiver', self.path], self.label + '-receiver')
        while True:
            self.ctx.assertion(self.process.poll() is None, 'OTLP receiver exited')
            try:
                with socket.create_connection(('127.0.0.1', 14318), timeout=.2):
                    break
            except OSError:
                self.ctx.remaining()
                time.sleep(.1)
        return self

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=min(10, self.ctx.remaining()))

    def spans(self, wait=True):
        if wait:
            time.sleep(min(12, self.ctx.remaining()))
        tools = span_tools()
        spans, problems = tools.load_spans([str(self.path)])
        self.ctx.assertion(not problems, 'collector capture missing or malformed')
        return spans

    def __enter__(self):
        return self.start()

    def __exit__(self, *_args):
        self.stop()


def settings(engines=False):
    result = ['server.observability.tracing.endpoint=' + ENDPOINT,
              'server.observability.timing_header=true']
    if engines:
        result.append('host.observability.tracing.engines=on')
    return result


def engine_status(ctx, name, reason=None, role='standalone'):
    status = ctx.status(name, role)
    instances = status.get('deployment', status).get('instances', [])
    decisions = [item.get('engine_tracing') for item in instances]
    ctx.assertion(decisions and all(isinstance(item, dict) for item in decisions), 'missing structured engine tracing status')
    if reason:
        ctx.assertion(all(item.get('reason') == reason and item.get('state') == 'off'
                          for item in decisions), 'wrong engine tracing suppression reason: ' + str(reason))
    else:
        ctx.assertion(all(item.get('state') == 'on' for item in decisions), 'engine tracing not enabled')
    return status


def request(ctx, name, records, label, stream=False, parent=SAMPLED, role='standalone', body=None, decision=False, headers=None, expected=200):
    sent = dict(headers or {})
    sampled = parent == SAMPLED
    caller_trace = uuid.uuid4().hex
    if sampled:
        parent = '00-' + caller_trace + '-' + PARENT + '-01'
    if parent is not None:
        sent['traceparent'] = parent
    if decision:
        body = body or dict(state='The customer was billed twice.', questions={'billing': {'type': 'noul', 'instructions': 'Is this about billing?'}})
    response = ctx.request(name, path='/v1/systemone' if decision else '/v1/chat/completions',
                           body=body, stream=stream, headers=sent, role=role)
    ctx.assertion(response['status'] in (expected if isinstance(expected, tuple) else (expected,)), label + ': unexpected HTTP status ' + str(response['status']))
    if response['status'] == 200:
        if decision:
            ctx.assertion('billing' in response.get('body', {}).get('answers', {}), 'decision answer missing')
        else:
            from .engines import answered
            answered(ctx, response, stream)
    rid = {key.lower(): value for key, value in response['response_headers'].items()}.get('x-capyctl-request-id')
    ctx.assertion(rid, 'answer has no request ID')
    from .measurements import timing_phases
    record = dict(label=label, request_id=rid, status=response['status'], timing=timing_phases(response),
                  caller=dict(mode='sampled' if sampled else 'none',
                              **({'trace_id': caller_trace, 'span_id': PARENT} if sampled else {})))
    records.append(record)
    return response


def summarize(ctx, spans, records):
    tools = span_tools()
    result = tools.summarize(spans, records)
    ctx.assertion(not result['attribute_keys_outside_allow_list'] and not result['resource_keys_outside_allow_list'], 'CapyCTL exported non-allowlisted metadata')
    for record in result['requests']:
        ctx.assertion(record['request_spans'] == 1, 'missing or duplicate request span: ' + record['label'])
    return result


def check_request_tree(ctx, entry, parent):
    ctx.assertion(entry['request_spans'] == 1, 'request span missing')
    if parent:
        ctx.assertion(entry['in_caller_trace'] and entry['parent_is_caller_span'], 'caller parentage lost')
    else:
        ctx.assertion(not entry['parent_span_id'] and entry['trace_id'] != TRACE, 'missing/malformed context did not start a root')
    names = {item['name'] for item in entry['tree']}
    ctx.assertion({'capyctl.request', 'capyctl.admission', 'capyctl.queue_wait', 'capyctl.dispatch', 'capyctl.forward', 'capyctl.settle'} <= names, 'request phase spans missing')
    ctx.assertion(entry['first_progress'] and all(entry['first_progress']), 'forward first_progress event missing')


class HeaderCapture:
    """Observe only engine-bound HTTP header *names*; never store raw packets/keys.

    AF_PACKET requires CAP_NET_RAW on the rented VM. A permission failure fails the
    row explicitly; absence of engine spans is not proof of header suppression.
    """
    def __init__(self, ports):
        self.ports = set(ports)
        self.records, self.errors = [], []
        self.done = threading.Event()

    def __enter__(self):
        try:
            self.sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0800))
        except PermissionError as error:
            raise PermissionError('CAP_NET_RAW is required for engine-header qualification') from error
        self.sock.bind(('lo', 0))
        self.sock.settimeout(.1)
        self.thread = threading.Thread(target=self.read, daemon=True)
        self.thread.start()
        return self.records

    def read(self):
        buffers = {}
        try:
            while not self.done.is_set():
                try:
                    packet = self.sock.recv(65535)
                except socket.timeout:
                    continue
                if len(packet) < 54 or packet[23] != 6:
                    continue
                ip_length = (packet[14] & 15) * 4
                offset = 14 + ip_length
                src, dst, sequence = struct.unpack('!HHI', packet[offset:offset+8])
                if dst not in self.ports:
                    continue
                header_length = (packet[offset+12] >> 4) * 4
                payload = packet[offset+header_length:]
                key = (src, dst)
                if payload.startswith(b'POST '):
                    buffers[key] = (sequence, payload)
                elif key in buffers:
                    start, previous = buffers[key]
                    if sequence == start + len(previous):
                        buffers[key] = (start, previous + payload)
                if key not in buffers:
                    continue
                start, data = buffers[key]
                if b'\r\n\r\n' in data:
                    names = [line.split(b':', 1)[0].decode('ascii', errors='replace').lower()
                             for line in data.split(b'\r\n\r\n', 1)[0].split(b'\r\n')[1:]]
                    self.records.append(dict(port=dst, header_names=names,
                                             traceparent='traceparent' in names, tracestate='tracestate' in names))
                    del buffers[key]
                elif len(data) > 65536:
                    raise AssertionError('engine HTTP headers exceeded capture bound')
        except Exception as error:
            self.errors.append(str(error))

    def __exit__(self, *_args):
        self.done.set()
        self.thread.join(timeout=2)
        self.sock.close()
        if self.errors:
            raise AssertionError('header capture failed: ' + ','.join(self.errors))


def ports(ctx, name, role='standalone'):
    from .lmcache import live_identities, listening_sockets
    values = listening_sockets(ctx, live_identities(ctx, name, role))
    ctx.assertion(values, 'owned engine has no listening socket')
    return [item['port'] for item in values]


def tracing_launch(ctx, name, role='standalone'):
    from .lmcache import live_identities
    records = []
    for identity in live_identities(ctx, name, role):
        directory = Path('/proc') / str(identity['pid'])
        args = directory.joinpath('cmdline').read_bytes().decode(errors='replace').split('\0')
        names = [item.split(b'=', 1)[0].decode(errors='replace') for item in directory.joinpath('environ').read_bytes().split(b'\0')]
        flags = [arg.split('=', 1)[0] for arg in args if arg.startswith('--') and any(word in arg.lower() for word in ('trac', 'otlp'))]
        records.append(dict(pid=identity['pid'], tracing_flags=flags,
                            tracing_environment_names=[key for key in names if key.startswith('OTEL_') or key.startswith('SGLANG_TRACE')]))
    return records


def collector_connections(identities):
    inodes = set()
    for item in identities:
        for fd in (Path('/proc') / str(item['pid']) / 'fd').iterdir():
            try:
                match = re.fullmatch(r'socket:\[(\d+)\]', os.readlink(fd))
                if match:
                    inodes.add(match[1])
            except FileNotFoundError:
                pass
    connections = []
    for filename in ('/proc/net/tcp', '/proc/net/tcp6'):
        for line in Path(filename).read_text().splitlines()[1:]:
            fields = line.split()
            if fields[9] in inodes and int(fields[2].rsplit(':', 1)[1], 16) in (4317, 4318, 14318):
                connections.append(dict(remote_port=int(fields[2].rsplit(':', 1)[1], 16), state=fields[3]))
    return connections


def ot1(ctx, evidence):
    from .measurements import isolated_role
    from .engines import stopped
    from .lmcache import live_identities
    with Collector(ctx, 'ot1') as collector, isolated_role(ctx, 'ot1') as info:
        for engine in ('vllm033', 'sglang0523', 'llamacpp090'):
            name = 'ot1-' + engine
            ctx.deploy(name, engine, extra={'lmcache': 'off'})
            records = []
            identities = live_identities(ctx, name, 'standalone') + [dict(pid=info['process'].pid)]
            snapshots = []
            sample_errors = []
            done = threading.Event()
            def sample():
                try:
                    while not done.is_set():
                        snapshots.extend(collector_connections(identities))
                        done.wait(.02)
                except OSError as error:
                    sample_errors.append(str(error))
            thread = threading.Thread(target=sample, daemon=True)
            thread.start()
            try:
                for stream in (False, True):
                    request(ctx, name, records, name + str(stream), stream=stream)
            finally:
                done.set()
                thread.join(timeout=2)
            launches = tracing_launch(ctx, name)
            evidence[engine] = dict(launches=launches, connections=snapshots, requests=records)
            ctx.assertion(not thread.is_alive() and not sample_errors, 'owned collector socket sampler failed')
            ctx.assertion(all(not item['tracing_flags'] and not item['tracing_environment_names'] for item in launches), 'default-off launch carries tracing settings')
            ctx.assertion(not snapshots, 'default-off process connected to collector')
            stopped(ctx, name)
        time.sleep(min(6, ctx.remaining()))
        ctx.assertion(not collector.path.exists(), 'default-off emitted OTLP')


def ot2(ctx, evidence):
    from .measurements import isolated_role
    from .engines import stopped
    records = []
    with Collector(ctx, 'ot2') as collector, isolated_role(ctx, 'ot2', settings()):
        for decision in (False, True):
            name = 'ot2-decision' if decision else 'ot2-chat'
            ctx.deploy(name, 'sglang0523', model='clef' if decision else 'chat', api='decisions' if decision else 'chat', extra={'lmcache': 'off'})
            request(ctx, name, records, name + '-sampled', decision=decision, headers={'tracestate': 'capyctl=ot2'})
            if not decision:
                request(ctx, name, records, name + '-stream', stream=True)
            request(ctx, name, records, name + '-none', parent=None, decision=decision)
            if not decision:
                for index, malformed in enumerate((SAMPLED.upper(), '00-'+'0'*32+'-'+PARENT+'-01', '00-'+TRACE+'-'+'0'*16+'-01', 'ff-'+TRACE+'-'+PARENT+'-01', 'short', SAMPLED+', '+SAMPLED)):
                    request(ctx, name, records, name + '-malformed-' + str(index), parent=malformed)
            stopped(ctx, name)
        evidence['summary'] = summarize(ctx, collector.spans(), records)
        for entry in evidence['summary']['requests']:
            check_request_tree(ctx, entry, entry['caller_mode'] == 'sampled')


def engine_parent(ctx, entry, engine, parent='capyctl.forward'):
    spans = entry['engine_spans']
    candidates = [item for item in spans if item['parent'] == parent]
    if engine.startswith('VLLM'):
        candidates = [item for item in candidates if item['name'] == 'llm_request']
    ctx.assertion(candidates, engine + ': engine request span has wrong/missing parent')
    if engine.startswith('SGLANG'):
        ctx.assertion(any(item.get('gen_ai.request.id_equals_request_id') is True for item in candidates), 'SGLang request ID differs from CapyCTL answer ID')


def ot5(ctx, evidence):
    from .measurements import isolated_role
    from .engines import stopped
    with Collector(ctx, 'ot5') as collector, isolated_role(ctx, 'ot5', settings(True)):
        for engine in ('VLLM_033_TRACING', 'SGLANG_0523_TRACING', 'llamacpp090'):
            name = 'ot5-' + engine.lower().replace('_', '-')
            ctx.deploy(name, engine, extra={'lmcache': 'off', 'engine_config': {'context_length': 8192}})
            status = engine_status(ctx, name, 'engine_unsupported' if engine == 'llamacpp090' else None)
            records = []
            with HeaderCapture(ports(ctx, name)) as headers:
                for stream in (False, True):
                    request(ctx, name, records, name + str(stream), stream=stream)
            evidence[engine] = dict(status=status, headers=headers)
            summary = summarize(ctx, collector.spans(), records)
            evidence[engine]['summary'] = summary
            ctx.assertion(headers, 'engine-bound headers were not observed')
            for entry in summary['requests']:
                if engine == 'llamacpp090':
                    ctx.assertion(not entry['engine_spans'], 'unsupported engine exported spans')
                else:
                    engine_parent(ctx, entry, engine)
            ctx.assertion(all(item['traceparent'] == (engine != 'llamacpp090') for item in headers), 'engine trace header suppression/propagation incorrect')
            stopped(ctx, name)


def ot9(ctx, evidence):
    from .measurements import isolated_role
    from .engines import stopped
    with Collector(ctx, 'ot9') as collector, isolated_role(ctx, 'ot9', settings(True)):
        for engine in ('VLLM_033_TRACING', 'SGLANG_0523_TRACING'):
            name = 'ot9-' + engine.lower().replace('_', '-')
            ctx.deploy(name, engine, extra={'retention': 'zero', 'lmcache': 'off'})
            status = engine_status(ctx, name, 'retention_zero')
            records = []
            with HeaderCapture(ports(ctx, name)) as headers:
                request(ctx, name, records, name)
            summary = summarize(ctx, collector.spans(), records)
            evidence[engine] = dict(status=status, headers=headers, summary=summary)
            ctx.assertion(headers and all(not item['traceparent'] and not item['tracestate'] for item in headers), 'zero-retention engine got trace context or capture missing')
            ctx.assertion(all(not entry['engine_spans'] for entry in summary['requests']), 'zero-retention engine emitted request spans')
            stopped(ctx, name)


def ot10(ctx, evidence):
    from .measurements import isolated_role
    from .engines import stopped
    python = Path(ctx.installation('SGLANG_0523_SDK_FREE')) / 'bin/python'
    result = ctx.command([python, '-c', 'import importlib.metadata as m,json; names={d.metadata["Name"].lower().replace("_","-") for d in m.distributions()}; print(json.dumps({name:name in names for name in ("opentelemetry-sdk","opentelemetry-exporter-otlp","opentelemetry-exporter-otlp-proto-http","opentelemetry-exporter-otlp-proto-grpc")}))'])
    evidence['packages'] = json.loads(result.stdout)
    evidence['sdk_free_engine_choice'] = 'SGLang 0.5.23 without tracing extra; vLLM 0.33.0 common requirements mandate API/SDK/OTLP exporter >=1.27.0.'
    ctx.assertion(not any(evidence['packages'].values()), 'OT10 environment is not SDK/exporter-free')
    with Collector(ctx, 'ot10') as collector, isolated_role(ctx, 'ot10', settings(True)):
        name = 'ot10-sdk-free'
        ctx.deploy(name, 'SGLANG_0523_SDK_FREE', extra={'lmcache': 'off'})
        evidence['status'] = engine_status(ctx, name, 'not_installed')
        records = []
        request(ctx, name, records, name)
        evidence['summary'] = summarize(ctx, collector.spans(), records)
        stopped(ctx, name)


def ot3(ctx, evidence):
    from .measurements import isolated_role
    from .engines import stopped
    sentinels = {kind: 'OT3' + kind.upper() + uuid.uuid4().hex for kind in ('message', 'system', 'tool', 'salt', 'xrid', 'model', 'engerr', 'decision')}
    evidence['sentinels'] = sentinels
    records, engine_errors = [], []
    evidence['engine_errors'] = engine_errors
    with Collector(ctx, 'ot3') as collector, isolated_role(ctx, 'ot3', settings(True)):
        request(ctx, sentinels['model'], records, 'unknown-model', expected=404)
        for engine in ('VLLM_033_TRACING', 'SGLANG_0523_TRACING'):
            name = 'ot3-' + engine.lower().replace('_', '-')
            ctx.deploy(name, engine, extra={'lmcache': 'off'})
            engine_status(ctx, name)
            base = dict(messages=[dict(role='user', content=sentinels['message'] + ' Say hello. /no_think')], max_tokens=32, temperature=0)
            for stream in (False, True):
                request(ctx, name, records, name + '-message-' + str(stream), body=base, stream=stream)
            request(ctx, name, records, name + '-system', body={**base, 'messages': [dict(role='system', content=sentinels['system']), *base['messages']]})
            tool = dict(type='function', function=dict(name='lookup', description=sentinels['tool'], parameters=dict(type='object', properties=dict(value=dict(type='string')))))
            tool_body = {**base, 'tools': [tool], 'messages': [dict(role='user', content='Look up a value.'), dict(role='assistant', content=None, tool_calls=[dict(id='call_1', type='function', function=dict(name='lookup', arguments=json.dumps({'value': sentinels['tool']})))]), dict(role='tool', tool_call_id='call_1', content=sentinels['tool'])]}
            request(ctx, name, records, name + '-tool', body=tool_body)
            # Engines may refuse cache_salt; still require that the request reached tracing.
            request(ctx, name, records, name + '-salt', body={**base, 'cache_salt': sentinels['salt']}, expected=(200, 400))
            request(ctx, name, records, name + '-xrid', body=base, headers={'X-Request-Id': sentinels['xrid']})
            quoted = False
            for kind, fields in (
                ('tool', {'tools': [tool], 'tool_choice': {'type': 'function', 'function': {'name': sentinels['engerr']}}}),
                ('schema', {'response_format': {'type': 'json_schema', 'json_schema': {'name': 'bad', 'schema': {'type': sentinels['engerr']}}}})):
                result = request(ctx, name, records, name + '-error-' + kind, body={**base, **fields}, expected=(200, 400, 422, 500))
                is_quoted = result['status'] >= 400 and sentinels['engerr'] in json.dumps(result['body'])
                quoted |= is_quoted
                engine_errors.append(dict(engine=engine, label=name + '-error-' + kind, kind=kind, status=result['status'], quotes_input=is_quoted, response=result['body']))
            ctx.assertion(quoted, 'no engine validation error quoted the input sentinel: ' + engine)
            stopped(ctx, name)
        name = 'ot3-decision'
        ctx.deploy(name, 'SGLANG_0523_TRACING', model='clef', api='decisions', extra={'lmcache': 'off'})
        request(ctx, name, records, name, decision=True, body=dict(state=sentinels['decision'], questions={'billing': {'type': 'noul', 'instructions': sentinels['decision'] + ' Is this about billing?'}}))
        spans = collector.spans()
        evidence['summary'] = summarize(ctx, spans, records)
        evidence['engine_errors'] = engine_errors
        for engine in ('vllm-033-tracing', 'sglang-0523-tracing'):
            entries = [entry for entry in evidence['summary']['requests'] if entry['label'].startswith('ot3-' + engine + '-message')]
            ctx.assertion(entries and all(entry['engine_spans'] for entry in entries), 'privacy capture has no engine request spans: ' + engine)
        for error in engine_errors:
            if error['quotes_input']:
                entry = next(item for item in evidence['summary']['requests'] if item['label'] == error['label'])
                ctx.assertion(any(item['name'] == 'capyctl.forward' for item in entry['tree']), 'quoted validation refusal never reached engine forwarding')
        raw = collector.path.read_bytes().lower()
        evidence['sentinel_counts'] = {kind: raw.count(value.lower().encode()) for kind, value in sentinels.items()}
        ctx.assertion(not any(evidence['sentinel_counts'].values()), 'request content escaped into OTLP')
        stopped(ctx, name)


def deferred(ctx, name, engine, memory):
    profile = ctx.register(engine)
    config = dict(schema_version=1, kind='deployment', name=name, engine=profile, model='chat', residency='deep', lmcache='off',
                  devices=[dict(id='gpu0')], request_deadline='1800s', timeouts=dict(initialize='1800s'),
                  engine_config=dict(context_length=8192, max_concurrent_requests=2, memory=dict(request=memory)))
    path = ctx.root / (name + '.json')
    path.write_text(json.dumps(config))
    ctx.cli('deploy', 'model', '--file', path)
    ctx.deployments.append(('standalone', name))
    ctx.cli('start', 'deployment', name)


def check_activation(ctx, entry, required):
    activations = entry.get('activations', [])
    ctx.assertion(activations, 'waiting request lacks activation link')
    ctx.assertion(any(required <= set(item['steps']) and item['linked_span'] for item in activations), 'linked activation lacks required steps: ' + ','.join(sorted(required)))
    comparisons = entry['timing_vs_spans']
    ctx.assertion({item['phase'] for item in comparisons} >= {'activation_wait_ms', 'queue_wait_ms', 'total_ms'}, 'timing/span phase comparison incomplete')
    ctx.assertion(all(abs(item['diff_ms']) <= 1 for item in comparisons), 'timing and span duration disagree by >1ms')


def ot4(ctx, evidence):
    from .measurements import isolated_role
    from .engines import stopped, passing_canary
    if ctx.unified:
        memory = ctx.engines.get('OT4_REQUEST', '24GiB')
        limits = ['host.resource_policy.memory.system.managed_limit=40GiB']
    else:
        total = int(ctx.command(['nvidia-smi', '--query-gpu=memory.total', '--format=csv,noheader,nounits']).stdout.splitlines()[0])
        memory = str(int(total * .55)) + 'MiB'
        limits = []
    evidence['request_per_deployment'] = memory
    records = []
    with Collector(ctx, 'ot4') as collector, isolated_role(ctx, 'ot4', settings() + limits):
        deferred(ctx, 'ot4-v', 'vllm033', memory)
        request(ctx, 'ot4-v', records, 'cold', stream=True)
        ctx.lifecycle('park', 'ot4-v')
        request(ctx, 'ot4-v', records, 'wake', stream=True)
        evidence['canary'] = passing_canary(ctx, 'ot4-v', restore=True)
        deferred(ctx, 'ot4-s', 'sglang0523', memory)
        request(ctx, 'ot4-s', records, 'switch-s', stream=True)
        evidence['victim_after_switch'] = ctx.status('ot4-v')
        ctx.assertion(evidence['victim_after_switch'].get('observed_state') != 'ready', 'two grants fit; no forced switch occurred')
        request(ctx, 'ot4-v', records, 'switch-v', stream=True)
        evidence['summary'] = summarize(ctx, collector.spans(), records)
        for entry in evidence['summary']['requests']:
            required = {'capyctl.launch', 'capyctl.readiness'} if entry['label'] == 'cold' else ({'capyctl.wake', 'capyctl.readiness', 'capyctl.canary'} if entry['label'] == 'wake' else {'capyctl.drain', 'capyctl.release'})
            check_activation(ctx, entry, required)
        stopped(ctx, 'ot4-s')
        stopped(ctx, 'ot4-v')


def restart(ctx, info, host, label):
    key = 'host_process' if host else 'process'
    info[key].terminate()
    info[key].wait(timeout=min(30, ctx.remaining()))
    if host:
        info[key] = ctx.spawn_host(info['host_binary'], info['host_config'], label)
    else:
        info[key] = ctx.spawn([info['binary'], 'start', 'server', '--config', info['config']], label)
    time.sleep(min(5, ctx.remaining()))
    ctx.assertion(info[key].poll() is None, 'tracing role restart failed')


def host_tracing(ctx, path, enabled):
    python = Path(ctx.installation('SGLANG_0523_TRACING')) / 'bin/python'
    script = ('import pathlib,sys,yaml; p=pathlib.Path(sys.argv[1]); d=yaml.safe_load(p.read_text()); '
              'd.setdefault("observability",{})["tracing"]='
              + repr({'endpoint': ENDPOINT, 'engines': 'on'} if enabled else {}) +
              '; p.write_text(yaml.safe_dump(d))')
    ctx.command([python, '-c', script, path])


def host_inventory(ctx, role, enabled):
    deadline = time.monotonic() + min(60, ctx.remaining())
    while time.monotonic() < deadline:
        inventory = json.loads(ctx.cli('list', 'hosts', '--json', role=role).stdout)
        hosts = inventory.get('hosts', [])
        if any(host.get('online') and host.get('name') == 'qualification-host'
               and (host.get('tracing') or {}).get('on') is enabled for host in hosts):
            return inventory
        time.sleep(.5)
    raise AssertionError('host did not publish the expected tracing state')


def ot6(ctx, evidence):
    tools = span_tools()
    with Collector(ctx, 'ot6') as collector:
        role = 'ot6-server'
        info = ctx.role(role)
        config = json.loads(info['config'].read_text())
        config['observability'] = {'tracing': {'endpoint': ENDPOINT}, 'timing_header': True}
        info['config'].write_text(json.dumps(config))
        restart(ctx, info, False, 'ot6-server-traced')
        host_tracing(ctx, info['host_config'], True)
        restart(ctx, info, True, 'ot6-host-traced')
        try:
            for engine in ('VLLM_033_TRACING', 'SGLANG_0523_TRACING'):
                name = 'ot6-' + engine.lower().replace('_', '-')
                ctx.deploy(name, engine, role=role, residency='deep', extra={'lmcache': 'off'})
                status = engine_status(ctx, name, role=role)
                inventory_on = host_inventory(ctx, role, True)
                records = []
                request(ctx, name, records, name + '-host-on', role=role)
                ctx.lifecycle('park', name, role)
                request(ctx, name, records, name + '-wake', role=role, stream=True)
                spans = collector.spans()
                summary = summarize(ctx, spans, records)
                evidence[engine] = {'on': summary, 'status_on': status, 'inventory_on': inventory_on}
                for entry in summary['requests']:
                    engine_parent(ctx, entry, engine, 'capyctl.ingress')
                    ingress = [item for item in entry['tree'] if item['name'] == 'capyctl.ingress']
                    ctx.assertion(ingress and all(item['parent'] == 'capyctl.forward' and item['service'] == 'capyctl-host' for item in ingress), 'host ingress parentage wrong')
                index = tools.Index(spans)
                wake_entry = next(item for item in summary['requests'] if item['label'].endswith('-wake'))
                activation_traces = {item['trace_id'] for item in wake_entry['activations']}
                wakes = [item for item in spans if item['name'] == 'capyctl.host.wake' and item['trace'] in activation_traces]
                ctx.assertion(any((index.parent_of(item) or {}).get('name') == 'capyctl.wake' for item in wakes), 'host wake is not a child of server wake')
                retained = ctx.owned(name, role)
                host_tracing(ctx, info['host_config'], False)
                restart(ctx, info, True, 'ot6-host-untraced-' + engine.lower())
                ctx.assertion(ctx.owned(name, role) == retained, 'host restart did not retain launch')
                evidence[engine]['inventory_off'] = host_inventory(ctx, role, False)
                records = []
                request(ctx, name, records, name + '-host-off', role=role)
                summary = summarize(ctx, collector.spans(), records)
                evidence[engine]['off'] = summary
                for entry in summary['requests']:
                    ctx.assertion(not any(item['name'] == 'capyctl.ingress' for item in entry['tree']), 'endpoint-free host emitted ingress')
                    engine_parent(ctx, entry, engine)
                ctx.cli('delete', 'deployment', name, '--stop', role=role)
                ctx.deployments.remove((role, name))
                host_tracing(ctx, info['host_config'], True)
                restart(ctx, info, True, 'ot6-host-retraced-' + engine.lower())
        finally:
            ctx.cleanup()
            for key in ('host_process', 'process'):
                info[key].terminate()
                info[key].wait(timeout=min(30, ctx.remaining()))
            ctx.roles.pop(role, None)


def ot7(ctx, evidence):
    from .measurements import isolated_role, load_chat
    from .engines import stopped
    # OT8's actual recorded jitter, not an invented universal overhead bound.
    path = ctx.evidence / 'result-OT8.json'
    ctx.assertion(path.exists(), 'OT7 requires a completed OT8 measurement receipt')
    receipt = json.loads(path.read_text())
    if receipt.get('verdict') == 'skip':
        # ADR 0037 OT7 needs OT8's measured noise, not an invented bound.
        from v020 import UnavailableEngine
        raise UnavailableEngine('OT7 prerequisite OT8 skipped: ' + receipt.get('reason', 'unavailable'))
    ctx.assertion(receipt.get('verdict') == 'pass', 'OT8 baseline did not pass')
    arms = receipt['evidence']['vllm033']
    off = arms['off']['requests']
    sampled = arms['sampled']['requests']
    phase_names = ('selection_ms', 'lease_grant_ms', 'pre_forward_ms')
    noise = {phase: max(abs(a['phases'][phase] - b['phases'][phase]) for a, b in zip(off, sampled)) for phase in phase_names}
    evidence['ot8_noise_ms'] = noise
    collector = Collector(ctx, 'ot7').start()
    try:
        with isolated_role(ctx, 'ot7', settings()):
            name = 'ot7-chat'
            ctx.deploy(name, 'vllm033', extra={'lmcache': 'off'})
            load_chat(ctx, name, n=8, concurrency=4, headers={'traceparent': SAMPLED})
            evidence['up'] = load_chat(ctx, name, n=60, concurrency=4, headers={'traceparent': SAMPLED})
            collector.spans()
            # Stop during active traffic rather than between two loads.
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                pending = pool.submit(load_chat, ctx, name, 60, 4, {'traceparent': SAMPLED})
                time.sleep(1)
                collector.stop()
                evidence['down'] = pending.result()
            end = time.monotonic() + 125
            evidence['down_keepalive'] = []
            while time.monotonic() < end:
                evidence['down_keepalive'].append(load_chat(ctx, name, n=4, concurrency=4, headers={'traceparent': SAMPLED}))
            evidence['status_down'] = ctx.status(name)
            tracing = evidence['status_down'].get('tracing', {})
            ctx.assertion(tracing.get('spans_dropped', 0) > 0 and tracing.get('last_export_error'), 'status lacks dropped spans/export error during outage')
            logs = (ctx.root / 'ot7.log').read_text(errors='replace')
            notices = [line for line in logs.splitlines() if 'spans dropped and' in line]
            stamps = []
            for line in notices:
                try:
                    doc = json.loads(line)
                    stamps.append(datetime.datetime.fromisoformat(doc['timestamp'].replace('Z', '+00:00')).timestamp())
                except (ValueError, KeyError):
                    match = re.search(r'\d{4}-\d\d-\d\dT[0-9:.]+Z', line)
                    ctx.assertion(match, 'export notice lacks parseable timestamp')
                    stamps.append(datetime.datetime.fromisoformat(match[0].replace('Z', '+00:00')).timestamp())
            evidence['notice_timestamps'] = stamps
            ctx.assertion(stamps and all(b-a >= 60 for a, b in zip(stamps, stamps[1:])), 'export failure notices are absent or exceed once/minute')
            collector = Collector(ctx, 'ot7-resumed').start()
            evidence['resumed'] = load_chat(ctx, name, n=60, concurrency=4, headers={'traceparent': SAMPLED})
            records = []
            request(ctx, name, records, 'resumed-proof')
            evidence['summary_resumed'] = summarize(ctx, collector.spans(), records)
            for phase in phase_names:
                up = sum(item['phases'][phase] for item in evidence['up']['requests']) / 60
                down = sum(item['phases'][phase] for item in evidence['down']['requests']) / 60
                ctx.assertion(abs(down-up) <= noise[phase], 'collector-down router phase exceeds OT8 observed noise: ' + phase)
            stopped(ctx, name)
    finally:
        collector.stop()


def run(ctx, row):
    evidence = {}
    try:
        globals()[row['id'].lower()](ctx, evidence)
        return dict(verdict='pass', reason='Every ADR 0037 criterion has live evidence', evidence=evidence)
    except (AssertionError, OSError, ValueError, KeyError, TimeoutError, subprocess.SubprocessError) as error:
        return dict(verdict='fail', reason=str(error), evidence=evidence)


if __name__ == '__main__':
    import sys
    if len(sys.argv) == 3 and sys.argv[1] == '--receiver':
        receiver_main(sys.argv[2])
    else:
        raise SystemExit('use --receiver OUTPUT')
