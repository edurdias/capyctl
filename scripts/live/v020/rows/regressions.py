"""P1 regressions #225, #236, #235 and #240; actual managed launches only."""
import concurrent.futures
import json
from pathlib import Path
import re
import threading
import time

ROWS = [
    dict(id='ISSUE225', priority=1, budget=1500, discrete=True,
         criteria='vLLM 0.32.0 and 0.33.0 each serve two co-resident models, each rendered utilization <0.75, with positive explicit KV bytes'),
    dict(id='ISSUE236', priority=1, budget=1200,
         criteria='SGLang 0.5.22 and 0.5.23 answer with prefill disabled and decode native in the managed native ServerArgs settings'),
    dict(id='ISSUE235', priority=1, budget=1500,
         criteria='editable SGLang refused before launch by new server/host; old server/new host records engine_import_failed without reconnect or harming another model'),
    dict(id='ISSUE240', priority=1, budget=1800,
         criteria='48 durable in-flight leases coexist on a managed engine; traffic survives server-first then host upgrade from 0.1.12 to 0.2.0'),
]


def answer(ctx, name, role='standalone'):
    result = ctx.request(name, role=role)
    ctx.assertion(result['status'] == 200 and '42' in json.dumps(result['body']), 'managed chat did not answer 42')
    return result


def no_floor(ctx):
    evidence = {}
    for engine in ('VLLM_032', 'VLLM_033'):
        names = ['floor-' + engine.lower() + '-' + str(i) for i in range(2)]
        starts = [ctx.deploy(name, engine, extra={'engine_config': {'memory': {'kv_cache': '2GiB'},
                                                                  'max_concurrent_requests': 2}}) for name in names]
        settings = [ctx.engine_args(name) for name in names]
        for setting in settings:
            flags = setting['flags']
            ctx.assertion(0 < float(flags.get('gpu-memory-utilization', 1)) < .75,
                          'rendered utilization must be positive and below 75%')
            ctx.assertion(int(flags.get('kv-cache-memory-bytes', 0)) > 0, 'explicit KV byte sizing missing')
        replies = [answer(ctx, name) for name in names]
        ctx.assertion(all(ctx.status(name).get('observed_state', '').lower() == 'ready' for name in names),
                      'two deployments did not remain co-resident')
        evidence[engine] = dict(starts=starts, settings=settings, answers=replies,
                                memory=ctx.sample() if not ctx.functional_only else None)
        ctx.cleanup()
    return evidence


def graph_phases(ctx):
    evidence = {}
    for engine in ('SGLANG_0522', 'SGLANG_0523'):
        name = 'phases-' + engine.lower()
        ctx.deploy(name, engine, extra={'engine_config': {'cuda_graphs': {'prefill': False, 'decode': True}}})
        setting = ctx.engine_args(name)
        flags, resolved = setting['flags'], setting['resolved']
        ctx.save(name + '-graph-settings', setting)
        ctx.assertion('disable-prefill-cuda-graph' in flags or flags.get('cuda-graph-backend-prefill') == 'disabled'
                      or resolved.get('disable_prefill_cuda_graph') == 'True',
                      'prefill disable setting missing from argv/native ServerArgs')
        ctx.assertion('disable-decode-cuda-graph' not in flags and flags.get('cuda-graph-backend-decode') != 'disabled',
                      'decode native policy overridden')
        ctx.assertion(resolved.get('disable_prefill_cuda_graph') == 'True' or resolved.get('cuda_graph_backend_prefill') == 'disabled',
                      'engine did not resolve prefill disabled')
        ctx.assertion(resolved.get('disable_decode_cuda_graph') == 'False' and resolved.get('cuda_graph_backend_decode') != 'disabled',
                      'engine did not resolve native decode')
        evidence[engine] = dict(settings=setting, answer=answer(ctx, name))
        ctx.cleanup()
    return evidence


def restart(ctx, role, host=False):
    info = ctx.roles[role]
    key = 'host_process' if host else 'process'
    process = info[key]
    process.terminate()
    process.wait(timeout=min(60, ctx.remaining()))
    binary = ctx.binary()
    if host:
        info['host_binary'] = binary
        info[key] = ctx.spawn([binary, 'start', 'host', '--config', info['host_config']], role + '-host-upgraded')
    else:
        info['binary'] = binary
        info[key] = ctx.spawn([binary, 'start', 'server', '--config', info['config']], role + '-upgraded')
    time.sleep(10)
    ctx.assertion(info[key].poll() is None, 'upgraded role exited')


def install_editable_profile(ctx, role):
    info = ctx.role(role)
    # Configure a profile directly to exercise the launch-time gate, not engine-add's
    # pre-existing editable refusal. Only this session's private host config changes.
    config = info['host_config']
    text = config.read_text()
    profile = '''runtime_profiles:
  editable-test:
    engine: sglang
    revision: 1
    executable: EXECUTABLE
    build_fingerprint: "sglang 0.5.23"
    args: []
    env: {}
    security:
      deep_park: enabled
      installation_drift: warn
'''.replace('EXECUTABLE', json.dumps(str(Path(ctx.engines['SGLANG_EDITABLE']) / 'bin/python')))
    ctx.assertion('runtime_profiles: {}' in text, 'host init config lacks empty runtime_profiles; cannot safely configure editable test')
    config.write_text(text.replace('runtime_profiles: {}', profile.rstrip()))
    process = info['host_process']
    process.terminate()
    process.wait(timeout=min(60, ctx.remaining()))
    info['host_process'] = ctx.spawn([info['host_binary'], 'start', 'host', '--config', config], role + '-host-editable')
    time.sleep(10)
    name = 'editable-' + role
    doc = dict(schema_version=1, kind='deployment', name=name, engine='editable-test',
               host='qualification-host', model='chat', residency='restart_only',
               engine_config=dict(context_length=8192, memory=dict(kv_cache='2GiB')))
    path = ctx.root / (name + '.json')
    path.write_text(json.dumps(doc))
    return name, path


def editable(ctx):
    evidence = {}
    ctx.installation('SGLANG_EDITABLE')
    # Registration must refuse without executing the engine in standalone.
    before = ctx.logs()
    result = ctx.cli('engine', 'add', ctx.engines['SGLANG_EDITABLE'], '--name', 'editable-refusal', check=False)
    ctx.assertion(result.returncode != 0 and 'editable' in result.stdout + result.stderr, 'standalone did not refuse editable add')
    evidence['standalone_refusal'] = dict(exit_code=result.returncode)
    for role, old in [('server', False), ('old-server', True)]:
        ctx.role(role, old=old)
        name, path = install_editable_profile(ctx, role)
        # A separate real model must remain healthy across the failed start.
        good = 'control-' + role
        ctx.deploy(good, 'VLLM_033', role=role, extra={'engine_config': {'memory': {'kv_cache': '2GiB'}}})
        good_binding = ctx.owned(good, role)
        events_before = ctx.events(role)
        deploy = ctx.cli('deploy', 'model', '--file', str(path), role=role, check=False)
        start = ctx.cli('start', 'deployment', name, '--wait', role=role, check=False)
        ctx.deployments.append((role, name))
        observed = start.stdout + start.stderr + deploy.stdout + deploy.stderr + json.dumps(ctx.events(role))
        code = 'engine_import_failed' if old else 'installation_unsupported'
        ctx.assertion(code in observed and start.returncode != 0, 'missing editable refusal code ' + code)
        ctx.assertion(not ctx.owned(name, role), 'editable engine still has live ownership')
        if not old:
            ctx.assertion('editable' in observed, 'new server refusal did not name editable installation')
        events_after = ctx.events(role)
        new_events = events_after[len(events_before):]
        ctx.assertion(not any('disconnect' in json.dumps(event).lower() or 'session_ready' in json.dumps(event).lower()
                              for event in new_events), 'control session reconnected after editable refusal')
        ctx.assertion(ctx.owned(good, role) == good_binding, 'other model ownership changed')
        evidence[role] = dict(code=code, events=new_events, control_answer=answer(ctx, good, role))
        ctx.cleanup()
    ctx.assertion('sglang.launch_server' not in ctx.logs()[len(before):] or evidence['old-server'], 'unexpected editable launch')
    return evidence


def concurrency(ctx):
    role = 'upgrade-server'
    info = ctx.role(role, old=True)
    # Begin with both roles on the published old binary, retaining enrollment/state.
    host = info['host_process']
    host.terminate()
    host.wait(timeout=min(60, ctx.remaining()))
    info['host_binary'] = ctx.binary(old=True)
    info['host_process'] = ctx.spawn([info['host_binary'], 'start', 'host', '--config', info['host_config']], role + '-host-old')
    time.sleep(10)
    name = 'concurrency-48'
    ctx.deploy(name, 'VLLM_033', role=role, extra={'engine_config': {'max_concurrent_requests': 48,
                'context_length': 8192, 'memory': {'kv_cache': '12GiB'}}})
    traffic, done = [], threading.Event()
    def drive():
        while not done.is_set():
            try:
                traffic.append(ctx.request(name, role=role))
            except (OSError, TimeoutError) as error:
                traffic.append(dict(status=0, error=type(error).__name__))
    thread = threading.Thread(target=drive, daemon=True)
    thread.start()
    try:
        restart(ctx, role)
        answer(ctx, name, role)
        # New server must honor all 48 slots even while the host is old.
        peak = 0
        barrier = threading.Barrier(48)
        def one(_):
            barrier.wait(timeout=30)
            return ctx.request(name, role=role, body=dict(messages=[dict(role='user', content='Write a very long story. /no_think')],
                                                         max_tokens=2048, temperature=0, ignore_eos=True))
        with concurrent.futures.ThreadPoolExecutor(max_workers=48) as pool:
            futures = [pool.submit(one, index) for index in range(48)]
            while not all(future.done() for future in futures):
                reading = ctx.sql('SELECT COUNT(*) AS n FROM request_leases WHERE deployment_id=(SELECT id FROM deployments WHERE name=?)', (name,), role)
                peak = max(peak, reading[0]['n'])
                if peak >= 48:
                    break
                time.sleep(.05)
            results = [future.result(timeout=ctx.remaining()) for future in futures]
        ctx.assertion(peak >= 48, 'never observed 48 simultaneous durable request leases')
        ctx.assertion(all(result['status'] == 200 for result in results), 'one of 48 managed requests failed')
        restart(ctx, role, host=True)
        final = answer(ctx, name, role)
    finally:
        done.set()
        thread.join(timeout=min(60, ctx.remaining()))
    ctx.assertion(any(result['status'] == 200 for result in traffic), 'no background traffic completed during rolling upgrade')
    return dict(peak_in_flight=peak, requests=results, traffic=traffic, final=final,
                upgrade_order=['server 0.1.12 -> 0.2.0', 'host 0.1.12 -> 0.2.0'])


def run(ctx, row):
    evidence = {'ISSUE225': no_floor, 'ISSUE236': graph_phases,
                'ISSUE235': editable, 'ISSUE240': concurrency}[row['id']](ctx)
    return dict(verdict='pass', reason=row['criteria'], evidence=evidence)
