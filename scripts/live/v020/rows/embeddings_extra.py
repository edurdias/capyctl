"""T47/T17: ADR 0036 EM3, EM5, EM6, EM7 and EM9, on real owned engines."""
import concurrent.futures
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import time

from .engines import answered, passing_canary, stopped
from .functional import EMBEDDING_ENGINES, vectors
from .lmcache import live_identities
from .measurements import isolated_role, metric_values, process_memory

ROWS = [dict(id=key, priority=4, budget=budget, measurement=True, criteria=criteria)
        for key, budget, criteria in (
    ('EM3', 2400, 'ADR0036: 20 start/restart/wake probes and 20 64-input batches at eight clients; measured tolerance plus margin <=0.02; 100 pinned cross-engine sentences.'),
    ('EM5', 3600, 'ADR0036: 2048-input running gauges, hangup/deadline engine connections and cancelling leases, 32 answer settlements without restart, killed-answer verified-stop settlement.'),
    ('EM6', 1500, 'ADR0036: vLLM-only separately enrolled host ingress validation and incremental relay, echoed request ID absent from engine headers (owner decision 2026-11-11).'),
    ('EM7', 2400, 'ADR0036/SPEC10: mutually exclusive embedding/chat grants, automatic pool switches, deep wake canary or llama.cpp restart.'),
    ('EM9', 1800, 'ADR0036: five deep park/wake cycles plus parked-arrival wake; per-launch release proof, measured residue, passing canary and timings.'))]
PROBE = 'CapyCTL readiness check: a quiet library with tall shelves of old books.'
FIXTURE = Path(__file__).with_name('embedding_sentences.json')


def similarity(reference, observed):
    if len(reference) != len(observed) or not reference:
        raise AssertionError('embedding dimension changed')
    if not all(isinstance(x, (float, int)) and math.isfinite(x) for x in (*reference, *observed)):
        raise AssertionError('embedding contains non-finite values')
    rn = math.sqrt(sum(x*x for x in reference))
    on = math.sqrt(sum(x*x for x in observed))
    if min(rn, on) <= 1e-6:
        raise AssertionError('embedding norm is zero')
    return dict(relative_distance=math.sqrt(sum((a-b)**2 for a, b in zip(reference, observed)))/rn,
                cosine=sum(a*b for a, b in zip(reference, observed))/(rn*on))


def tolerance(observations):
    largest = max(item['relative_distance'] for item in observations)
    margin = max(.001, largest*.10)
    return dict(largest_relative_distance=largest,
                smallest_cosine=min(item['cosine'] for item in observations),
                margin=margin, margin_rule='max(0.001, 10% of measured maximum)',
                measured_tolerance=largest+margin, qualification_ceiling=.02)


def reference(ctx, name, role='standalone'):
    records = ctx.sql('''SELECT w.kind,w.probe_version,w.dimension,w.tolerance,hex(w.vector) AS vector_hex,
        w.incarnation,w.generation FROM wake_canaries w JOIN deployments d ON d.id=w.deployment_id
        WHERE d.name=? AND w.kind='embedding' ORDER BY w.generation DESC LIMIT 1''', (name,), role=role)
    ctx.assertion(len(records) == 1, 'missing stored embedding reference')
    item = records[0]
    raw = bytes.fromhex(item.pop('vector_hex'))
    ctx.assertion(len(raw) == item['dimension']*4, 'stored embedding reference dimension mismatch')
    return dict(item, vector=list(struct.unpack('<'+'f'*item['dimension'], raw)))


def probe(ctx, name, inputs=PROBE, role='standalone'):
    # Concurrent EM3 requests must not race the shared evidence counter.
    response = ctx.request(name, path='/v1/embeddings', body=dict(input=inputs, encoding_format='float'), role=role)
    return vectors(ctx, response, name, len(inputs) if isinstance(inputs, list) else 1)


def em3(ctx, evidence):
    raw = FIXTURE.read_bytes()
    sentences = json.loads(raw)
    ctx.assertion(len(sentences) == 100 and len(set(sentences)) == 100,
                  'cross-engine fixture must contain exactly 100 distinct pinned sentences')
    evidence['fixture'] = dict(name=FIXTURE.name, sha256=hashlib.sha256(raw).hexdigest(), sentences=sentences)
    cross = {}
    for engine in EMBEDDING_ENGINES:
        name = 'em3-'+engine
        ctx.deploy(name, engine, model='embedding', api='embeddings',
                   residency='restart_only' if engine.startswith('llama') else 'deep')
        stored = reference(ctx, name)
        baseline = stored['vector']
        stages = {}
        evidence[engine] = dict(reference=stored, stages=stages)
        stages['after_start'] = [similarity(baseline, probe(ctx, name)[0]) for _ in range(20)]
        stopped(ctx, name)
        ctx.lifecycle('start', name)
        stages['after_restart'] = [similarity(baseline, probe(ctx, name)[0]) for _ in range(20)]
        if not engine.startswith('llama'):
            ctx.lifecycle('park', name)
            ctx.lifecycle('wake', name)
            stages['after_wake'] = [similarity(baseline, probe(ctx, name)[0]) for _ in range(20)]
            evidence[engine]['wake_canary'] = passing_canary(ctx, name)
        # Twenty probe-bearing batches, eight actual simultaneous clients. A
        # barrier on the first wave proves this is not a serial batch loop.
        import threading
        barrier = threading.Barrier(8)
        def batch(index):
            if index < 8:
                barrier.wait(timeout=min(30, ctx.remaining()))
            position = index % 64
            inputs = [sentences[(index*63+i) % 100] for i in range(64)]
            inputs[position] = PROBE
            return dict(batch=index, probe_index=position,
                        **similarity(baseline, probe(ctx, name, inputs)[position]))
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            stages['concurrent_64_inputs'] = list(pool.map(batch, range(20)))
        measured = tolerance([item for records in stages.values() for item in records])
        evidence[engine]['tolerance'] = measured
        ctx.save(name+'-tolerance', evidence[engine])
        ctx.assertion(measured['measured_tolerance'] <= .02, 'measured canary tolerance including margin exceeds 0.02')
        ctx.assertion(measured['largest_relative_distance'] <= stored['tolerance'],
                      'observed embeddings exceed the binary canary tolerance')
        cross[engine] = probe(ctx, name, sentences)
        stopped(ctx, name)
    evidence['cross_engine'] = {}
    for left, right in (('vllm033', 'sglang0523'), ('llamacpp090', 'vllm033'), ('llamacpp090', 'sglang0523')):
        values = [dict(index=index, **similarity(a, b)) for index, (a, b) in enumerate(zip(cross[left], cross[right]))]
        evidence['cross_engine'][left+'-vs-'+right] = dict(required=False, observations=values,
            largest_relative_distance=max(item['relative_distance'] for item in values),
            smallest_cosine=min(item['cosine'] for item in values))
    ctx.save('em3-cross-engine-vectors', cross)


def latest_park(ctx, name, role='standalone'):
    records = ctx.sql('''SELECT s.id,s.state,e.evidence_json,e.committed_epoch FROM lifecycle_steps s
        JOIN operations o ON o.id=s.operation_id JOIN deployments d ON d.id=o.deployment_id
        JOIN lifecycle_evidence e ON e.step_id=s.id WHERE d.name=? AND o.kind='park'
        AND o.id=(SELECT o2.id FROM operations o2 WHERE o2.deployment_id=d.id AND o2.kind='park'
                  ORDER BY o2.rowid DESC LIMIT 1)
        ORDER BY e.committed_epoch DESC LIMIT 8''', (name,), role=role)
    return [dict(item, evidence=json.loads(item['evidence_json'])) for item in records]


def parked_proof(ctx, name, engine, role='standalone'):
    records = latest_park(ctx, name, role)
    ctx.assertion(any(item['state'] == 'completed' and 'memory_released' in item['evidence'].get('milestones', [])
                      for item in records), 'park lacks committed per-launch MemoryReleased evidence')
    residues = ctx.sql('''SELECT r.generation,r.domain,r.first_bytes,r.last_bytes,r.parks,r.last_step_id,
        r.measured_at_ms,r.stop_operation_id FROM parked_launch_residues r JOIN deployments d ON d.id=r.deployment_id
        JOIN deployment_instances i ON i.deployment_id=d.id AND i.instance_index=r.instance_index
        AND i.generation=r.generation WHERE d.name=?''', (name,), role=role)
    ctx.assertion(bool(residues) and all(item['stop_operation_id'] is None for item in residues),
                  'missing same-launch measured parked residues or park became stop')
    ctx.assertion(all(item['last_step_id'] in {record['id'] for record in records} for item in residues),
                  'parked residue is stale: it does not belong to the latest park')
    memory = process_memory(ctx)
    own_pids = {str(item['pid']) for item in ctx.owned(name, role)}
    memory = {key: {pid: value for pid, value in readings.items() if pid in own_pids}
              for key, readings in memory.items()}
    gpu = memory['owned_gpu_bytes_by_pid']
    result = dict(committed_release=records, residues=residues, process_memory=memory)
    effective = ctx.sql('''SELECT e.effective_json FROM effective_revisions e JOIN deployments d ON d.id=e.deployment_id
        WHERE d.name=? AND e.revision=d.revision''', (name,), role=role)
    config = json.loads(effective[0]['effective_json'])
    allocations = config['resources']['parked']['allocations']
    domains = {item['domain'] for item in residues if 'gpu' in item['domain'].lower()}
    if engine.startswith('vllm'):
        # SPEC9.1: compare only this launch's NVML bytes, never global usage.
        budgets = [item['bytes'] for item in allocations if item['domain'] in domains]
        if ctx.unified:
            budgets = [item['bytes'] for item in allocations]
        ctx.assertion(bool(gpu) and bool(budgets), 'missing per-launch GPU sample or parked budget')
        result.update(gpu_bytes=sum(gpu.values()), parked_budget_bytes=sum(budgets))
        ctx.assertion(result['gpu_bytes'] <= result['parked_budget_bytes'], 'vLLM launch exceeds its parked GPU budget')
    elif role != 'standalone':
        measured = host_saver_evidence(ctx, name, role)
        ctx.assertion(measured['before_mapped_bytes'] > 0 and measured['after_mapped_bytes'] == 0,
                      'SGLang saver map did not go from mapped to fully unmapped')
        result['saver_map'] = measured
    else:
        raise AssertionError('SGLang saver-map measurement requires enrolled host execution evidence')
    return result


def protobuf_fields(data):
    """Decode only wire primitives, never importing an engine/protobuf package."""
    index = 0
    def varint():
        nonlocal index
        value = 0
        for shift in range(0, 70, 7):
            if index >= len(data):
                raise ValueError('truncated residency protobuf')
            byte = data[index]
            index += 1
            value |= (byte & 127) << shift
            if byte < 128:
                return value
        raise ValueError('oversized residency protobuf varint')
    while index < len(data):
        tag = varint()
        field, wire = tag >> 3, tag & 7
        if field == 0:
            raise ValueError('invalid residency protobuf tag')
        if wire == 0:
            value = varint()
        elif wire in (1, 2, 5):
            length = varint() if wire == 2 else (8 if wire == 1 else 4)
            value = data[index:index+length]
            if len(value) != length:
                raise ValueError('truncated residency protobuf field')
            index += length
        else:
            raise ValueError('unsupported residency protobuf wire type')
        yield field, value


def host_saver_evidence(ctx, name, role):
    import sqlite3
    # native_results contains MemberExecutionResult, not command descriptors
    # or credentials. Field 10 is ResidencyEvidence; its field 4 is milestones.
    # Read only the current deployment's latest successful host park.
    deployment = ctx.status(name, role)['id']
    info = ctx.role(role)
    databases = list((info['state']/'host').rglob('commands.sqlite'))
    ctx.assertion(len(databases) == 1, 'enrolled host native journal unavailable')
    with sqlite3.connect('file:'+str(databases[0])+'?mode=ro', uri=True) as db:
        rows = db.execute('''SELECT c.command_id,n.result FROM commands c JOIN native_results n
            ON n.command_id=c.command_id WHERE c.deployment=? ORDER BY c.sequence DESC LIMIT 32''',
            (deployment,)).fetchall()
    for command, data in rows:
        for field, value in protobuf_fields(data):
            if field != 10:
                continue
            fields = list(protobuf_fields(value))
            if not any(number == 1 and content == b'parked' for number, content in fields):
                continue
            milestones = [content.decode('ascii') for number, content in fields if number == 4]
            def measured(prefix):
                values = [int(item[len(prefix):]) for item in milestones if item.startswith(prefix)]
                ctx.assertion(len(values) == 1, 'missing host saver map measurement: '+prefix)
                return values[0]
            return dict(command_id=command, milestones=milestones,
                        before_mapped_bytes=measured('saver_mapped_before.'),
                        after_mapped_bytes=measured('saver_mapped_after.'))
    raise AssertionError('no persisted parked host result with saver measurements')


def enrolled_canary(ctx, name, role):
    current = ctx.sql('''SELECT w.generation,w.incarnation,w.kind,w.mismatch_operation_id
        FROM wake_canaries w JOIN deployments d ON d.id=w.deployment_id JOIN runtime_bindings b
        ON b.deployment_id=w.deployment_id AND b.incarnation=w.incarnation AND b.state='live'
        WHERE d.name=?''', (name,), role=role)
    journal = ctx.sql('''SELECT j.state FROM journal_entries j JOIN operations o ON o.id=j.operation_id
        JOIN deployments d ON d.id=o.deployment_id WHERE d.name=? AND o.kind='restore'
        AND o.rowid=(SELECT MAX(o2.rowid) FROM operations o2 WHERE o2.deployment_id=d.id AND o2.kind='restore')''',
        (name,), role=role)
    ctx.assertion(bool(current) and all(item['kind'] == 'embedding' and item['mismatch_operation_id'] is None
                                      for item in current), 'wake lacks a passing current embedding canary')
    ctx.assertion(any(item['state'] == 'restored' for item in journal), 'wake lacks completed restored journal')
    return dict(reference=current, journal=journal)


def em9(ctx, evidence):
    role = 'em9-server'
    ctx.role(role)
    for engine in ('vllm033', 'sglang0523'):
        name = 'em9-'+engine
        ctx.deploy(name, engine, model='embedding', api='embeddings', residency='deep', role=role)
        baseline = reference(ctx, name, role)['vector']
        cycles = []
        evidence[engine] = cycles
        for index in range(6):
            before = live_identities(ctx, name, role)
            start = time.monotonic()
            park = ctx.lifecycle('park', name, role=role)
            park_s = time.monotonic()-start
            proof = parked_proof(ctx, name, engine, role)
            start = time.monotonic()
            if index < 5:
                ctx.lifecycle('wake', name, role=role)
            observed = probe(ctx, name, role=role)[0]
            wake_s = time.monotonic()-start
            canary = enrolled_canary(ctx, name, role)
            ctx.assertion(before == live_identities(ctx, name, role), 'deep wake replaced the owned launch')
            measured = similarity(baseline, observed)
            ctx.assertion(measured['relative_distance'] <= .02, 'wake vector exceeds canary ceiling')
            cycles.append(dict(cycle=index+1, implicit_wake=index == 5, park=park,
                park_s=park_s, wake_and_request_s=wake_s, proof=proof, canary=canary, **measured))
            ctx.save(name+'-cycles', cycles)
        ctx.stop(name, role=role)
        ctx.assertion(not ctx.owned(name, role), 'embedding stop retained owned processes')


def leases(ctx, name):
    return ctx.sql('''SELECT l.id,l.generation,l.disposition,c.cancelled_at_ms,c.engine_answered
        FROM request_leases l JOIN deployments d ON d.id=l.deployment_id
        LEFT JOIN request_lease_cancellations c ON c.lease_id=l.id WHERE d.name=?''', (name,))


def wait_for(ctx, predicate, reason, seconds=30):
    end = time.monotonic()+min(seconds, ctx.remaining())
    while time.monotonic() < end:
        value = predicate()
        if value:
            return value
        time.sleep(.02)
    raise AssertionError(reason)


def engine_port(ctx, name):
    endpoints = ctx.sql('''SELECT l.host,l.port FROM endpoint_leases l JOIN runtime_bindings b ON b.id=l.binding_id
        JOIN deployments d ON d.id=b.deployment_id WHERE d.name=? AND b.state='live' ''', (name,))
    ctx.assertion(len(endpoints) == 1 and endpoints[0]['host'] in ('127.0.0.1', '::1'),
                  'owned loopback engine endpoint unavailable')
    return int(endpoints[0]['port'])


def work_gauge(ctx, name, engine):
    connection = http.client.HTTPConnection('127.0.0.1', engine_port(ctx, name), timeout=min(2, ctx.remaining()))
    try:
        connection.request('GET', '/metrics')
        response = connection.getresponse()
        body = response.read().decode()
        ctx.assertion(response.status == 200, 'engine metrics require unavailable credential-free observation')
        values = metric_values(body)
    finally:
        connection.close()
    keys = ('vllm:num_requests_running',) if engine.startswith('vllm') else (
        ('sglang:num_running_reqs', 'sglang:num_requests_running') if engine.startswith('sglang') else
        ('llamacpp:requests_processing',))
    for key in keys:
        if key in values:
            raw = values[key]
            return dict(metric=key, running=sum(raw) if isinstance(raw, list) else raw)
    raise AssertionError('embedding engine running gauge missing')


def connection_states(ctx, name):
    """Socket metadata only: never read engine argv credentials or packet bytes."""
    port = engine_port(ctx, name)
    result = []
    for item in live_identities(ctx, name, 'standalone'):
        pid = item['pid']
        inodes = set()
        for fd in (Path('/proc')/str(pid)/'fd').iterdir():
            try:
                target = os.readlink(fd)
            except FileNotFoundError:
                continue
            if target.startswith('socket:['):
                inodes.add(target[8:-1])
        for table in ('tcp', 'tcp6'):
            for line in (Path('/proc')/str(pid)/'net'/table).read_text().splitlines()[1:]:
                fields = line.split()
                if fields[9] in inodes and fields[3] == '01' and int(fields[1].rsplit(':', 1)[1], 16) == port:
                    result.append(dict(pid=pid, inode=fields[9], local=fields[1], remote=fields[2], state='ESTABLISHED'))
    return result


def send_batch(ctx, name, length=512):
    from v020 import credentials
    role = ctx.role()
    conn = http.client.HTTPConnection('127.0.0.1', role['port'], timeout=min(60, ctx.remaining()))
    body = dict(model=name, input=[[100]*length for _ in range(2048)], encoding_format='float')
    conn.request('POST', '/v1/embeddings', json.dumps(body),
                 {'Content-Type': 'application/json', 'Authorization': 'Bearer '+credentials(role['cred'])['api_key']})
    return conn


def signal_owned(ctx, identities, sig):
    from session import matches
    for item in identities:
        if matches(item):
            os.kill(item['pid'], sig)


def cut_batch(ctx, name, engine, mode, index, kill=False):
    before = live_identities(ctx, name, 'standalone')
    event_offset = len(ctx.events())
    conn = send_batch(ctx, name)
    paused = False
    try:
        gauge = wait_for(ctx, lambda: (reading if (reading := work_gauge(ctx, name, engine))['running'] >= 1 else None),
                         '2048-input batch never appeared in engine running gauge')
        active = leases(ctx, name)
        ctx.assertion(len(active) == 1, 'batch lacks a single active lease')
        lease_id = active[0]['id']
        first_bytes = None
        if kill:
            response = conn.getresponse()
            first_bytes = len(response.read1(256))
            ctx.assertion(response.status == 200 and first_bytes > 0, 'kill fixture has not begun an embedding answer')
        sockets = connection_states(ctx, name)
        ctx.assertion(bool(sockets), 'engine connection missing during batch')
        signal_owned(ctx, before, signal.SIGSTOP)
        paused = True
        cut_at = time.monotonic()
        if mode == 'hangup':
            if conn.sock:
                conn.sock.shutdown(socket.SHUT_RDWR)
            conn.close()
        # Deadline mode leaves the client alone: the configured 5-second
        # request deadline must cause the durable cancellation itself.
        cancelling = wait_for(ctx, lambda: [item for item in leases(ctx, name)
                                            if item['id'] == lease_id and item['cancelled_at_ms'] is not None],
                              'cut lease never became cancelling', seconds=8)
        cut_at = cancelling[0]['cancelled_at_ms']/1000
        after_cut = connection_states(ctx, name)
        overlap = {item['inode'] for item in sockets} & {item['inode'] for item in after_cut}
        ctx.assertion(bool(overlap), 'client cut closed the engine connection')
        # A stopped owned engine cannot finish its answer. Its cancelling
        # lease must stay charged until the resumed engine answers whole.
        time.sleep(.2)
        held = leases(ctx, name)
        ctx.assertion(any(item['id'] == lease_id and item['cancelled_at_ms'] is not None for item in held),
                      'lease settled before the engine could complete its answer')
        if kill:
            signal_owned(ctx, before, signal.SIGKILL)
            paused = False
            time.sleep(.2)
            retained = leases(ctx, name)
            # A controller-initiated verified stop may already have completed;
            # the durable stop evidence below must cover either ordering.
            stopped(ctx, name)
            ctx.assertion(not leases(ctx, name), 'killed-answer lease survived verified stop')
            termination = ctx.sql('''SELECT o.kind,o.state,e.evidence_json FROM operations o
                JOIN deployments d ON d.id=o.deployment_id JOIN lifecycle_steps s ON s.operation_id=o.id
                JOIN lifecycle_evidence e ON e.step_id=s.id WHERE d.name=? AND o.kind IN ('stop','restart')
                ORDER BY e.committed_epoch DESC LIMIT 8''', (name,))
            ctx.assertion(bool(termination), 'killed answer lacks durable verified-stop evidence')
            ctx.lifecycle('start', name)
            probe(ctx, name)
            return dict(index=index, mode=mode, lease_id=lease_id, gauge=gauge, first_response_bytes=first_bytes,
                        cancelling=cancelling, connection_before=sockets, connection_after_cut=after_cut,
                        held_before_kill=held, retained_after_kill=retained, verified_stop=termination)
        signal_owned(ctx, before, signal.SIGCONT)
        paused = False
        wait_for(ctx, lambda: not any(item['id'] == lease_id for item in leases(ctx, name)),
                 'cut batch failed to settle on its complete answer', seconds=180)
        events = ctx.events()[event_offset:]
        acknowledgement = [event for event in events if event['kind'] == 'request_cancellation_acknowledged'
                           and lease_id in event['payload_json'] and 'settled on its answer' in event['payload_json']]
        ctx.assertion(bool(acknowledgement), 'cut batch lacks its own complete-answer settlement receipt')
        ctx.assertion(before == live_identities(ctx, name, 'standalone'), 'cut batch restarted the engine')
        return dict(index=index, mode=mode, lease_id=lease_id, gauge=gauge, cancelling=cancelling,
                    connection_before=sockets, connection_after_cut=after_cut,
                    held_while_engine_stopped=held, settlement=acknowledgement,
                    cut_to_settlement_s=max(0, time.time()-cut_at))
    finally:
        if paused:
            signal_owned(ctx, before, signal.SIGCONT)
        conn.close()


def em5(ctx, evidence):
    for engine in EMBEDDING_ENGINES:
        with isolated_role(ctx, 'em5-'+engine):
            name = 'em5-'+engine
            ctx.deploy(name, engine, model='embedding', api='embeddings',
                       extra=dict(request_deadline='5s'))
            trials = []
            evidence[engine] = dict(cuts=trials)
            for index in range(32):
                trials.append(cut_batch(ctx, name, engine, 'hangup' if index % 2 == 0 else 'deadline', index))
                ctx.save(name+'-cuts', trials)
            evidence[engine]['kill_mid_answer'] = cut_batch(ctx, name, engine, 'hangup', 32, kill=True)
            stopped(ctx, name)


def policy_limit(ctx):
    policies = ctx.sql('SELECT policy_json FROM host_resource_policies')
    for item in policies:
        doc = json.loads(item['policy_json'])
        for domain in doc.get('domains', {}).values():
            if domain.get('memory') == 'device':
                value = domain['managed_limit']
                if isinstance(value, int):
                    return value
                if isinstance(value, str) and value.endswith('B') and value[:-1].isdigit():
                    return int(value[:-1])
    raise AssertionError('device managed limit unavailable from published host policy')


def grant_snapshot(ctx, names):
    marks = ','.join('?' for _ in names)
    return ctx.sql(f'''SELECT d.name,r.owner_id,r.footprint_json FROM resource_owners r
        JOIN deployments d ON d.id=r.deployment_id WHERE d.name IN ({marks})''', tuple(names))


def device_charge(records, name):
    matching = [json.loads(item['footprint_json']) for item in records if item['name'] == name]
    if len(matching) != 1 or matching[0]['phase'] != 'ready':
        raise AssertionError('missing ready resource owner for '+name)
    values = {domain: count for domain, count, _ in matching[0]['allocations'] if 'gpu0' in domain}
    if len(values) != 1:
        raise AssertionError('missing single gpu0 ready grant')
    return next(iter(values.items()))


def em7(ctx, evidence):
    for engine in EMBEDDING_ENGINES:
        with isolated_role(ctx, 'em7-'+engine):
            ctx.role()
            limit = policy_limit(ctx)
            # The reserved requests each exceed half the GPU's managed
            # domain. They fit singly, never together, even on a 96GiB card.
            request = (limit*55)//100
            extra = dict(engine_config=dict(context_length=8192,
                         memory=dict(request=str(request)+'B', kv_cache='2GiB')))
            embed, chat = 'em7-embed-'+engine, 'em7-chat-'+engine
            residency = 'restart_only' if engine.startswith('llama') else 'deep'
            ctx.deploy(embed, engine, model='embedding', api='embeddings', residency=residency, extra=extra)
            probe(ctx, embed)
            initial_grant = grant_snapshot(ctx, (embed,))
            before = live_identities(ctx, embed, 'standalone')
            ctx.deploy(chat, engine, model='chat', residency='restart_only', extra=extra)
            answered(ctx, ctx.request(chat))
            first = dict(embedding=ctx.status(embed), chat=ctx.status(chat), grants=grant_snapshot(ctx, (embed, chat)))
            expected = 'stopped' if engine.startswith('llama') else 'parked'
            ctx.assertion(first['embedding']['observed_state'].lower() == expected,
                          'chat activation did not switch embeddings out of the mutually exclusive pool')
            evidence[engine] = dict(managed_limit_bytes=limit, each_request_bytes=request,
                                    combined_request_bytes=request*2, switched_to_chat=first)
            ctx.assertion(request*2 > limit, 'EM7 fixtures fit together')
            embed_domain, embed_charge = device_charge(initial_grant, embed)
            chat_domain, chat_charge = device_charge(first['grants'], chat)
            ctx.assertion(embed_domain == chat_domain and max(embed_charge, chat_charge) <= limit
                          and embed_charge+chat_charge > limit,
                          'actual ready grants are not individually fitting and mutually exclusive')
            evidence[engine].update(initial_embedding_grant=initial_grant,
                                    actual_ready_charges_bytes=[embed_charge, chat_charge], domain=embed_domain)
            release = latest_park(ctx, embed) if expected == 'parked' else None
            if expected == 'parked':
                ctx.assertion(any('memory_released' in item['evidence'].get('milestones', []) for item in release),
                              'switched embedding lacks committed deep release evidence')
            start = time.monotonic()
            probe(ctx, embed)
            second = dict(embedding=ctx.status(embed), chat=ctx.status(chat), grants=grant_snapshot(ctx, (embed, chat)),
                          switch_and_answer_s=time.monotonic()-start)
            ctx.assertion(second['chat']['observed_state'].lower() == 'stopped', 'embedding activation did not switch chat out')
            after = live_identities(ctx, embed, 'standalone')
            if expected == 'parked':
                ctx.assertion(before == after, 'deep pool switch replaced embedding launch')
                second['canary'] = passing_canary(ctx, embed)
            else:
                ctx.assertion(before != after, 'llama.cpp pool switch did not create a fresh launch')
                second['canary'] = passing_canary(ctx, embed, restore=False)
            evidence[engine].update(parked_release=release, switched_to_embedding=second)
            # A second request in each direction rules out one-off start order.
            answered(ctx, ctx.request(chat))
            ctx.assertion(ctx.status(embed)['observed_state'].lower() == expected, 'second chat switch failed')
            probe(ctx, embed)
            if expected == 'parked':
                passing_canary(ctx, embed)
            ctx.save('em7-'+engine+'-switches', evidence[engine])


def em6(ctx, evidence):
    from v020 import credentials
    ctx.installation('EM6_VLLM')
    metadata = ctx.evidence/'EM6_VLLM.observer.json'
    ctx.assertion(metadata.is_file(), 'EM6 needs the setup-created observational environment fingerprint')
    fingerprint = json.loads(metadata.read_text())
    ctx.assertion(fingerprint.get('observational_environment') is True, 'EM6 environment is not explicitly observational')
    evidence['environment'] = fingerprint
    evidence['transport'] = 'observational ASGI middleware fragments unchanged answer bytes into <=64KiB writes'
    log = ctx.evidence/'em6-engine-observation.jsonl'
    role, name = 'em6-server', 'em6-host-embedding'
    info = ctx.role(role)
    ctx.deploy(name, 'em6_vllm', model='embedding', api='embeddings', role=role)
    evidence['roles'] = dict(server_pid=info['process'].pid, host_pid=info['host_process'].pid,
                             host_ingress_port=info['port']+4, status=ctx.status(name, role))
    offset = log.stat().st_size if log.exists() else 0
    rejected = ctx.request(name, path='/v1/embeddings', body=dict(input=[], unknown_field=True), role=role)
    ctx.assertion(rejected['status'] == 400 and 'invalid_request' in json.dumps(rejected['body']),
                  'host-path malformed embedding was not validated')
    request_id = 'em6-public-request-id'
    conn = http.client.HTTPConnection('127.0.0.1', info['port'], timeout=min(180, ctx.remaining()))
    body = dict(model=name, input=['qualification host relay']*2048, encoding_format='float')
    try:
        conn.request('POST', '/v1/embeddings', json.dumps(body),
                     {'Content-Type': 'application/json', 'X-Request-Id': request_id,
                      'Authorization': 'Bearer '+credentials(info['cred'])['api_key']})
        response = conn.getresponse()
        first = response.read1(1)
        first_ns = time.monotonic_ns()
        ctx.assertion(response.status == 200 and bool(first), 'host path never relayed first embedding bytes')
        chunks = [first]
        while True:
            chunk = response.read(65536)
            if not chunk:
                break
            chunks.append(chunk)
        last_ns = time.monotonic_ns()
        headers = {key.lower(): value for key, value in response.getheaders()}
        result = dict(status=response.status, body=json.loads(b''.join(chunks)))
        vectors(ctx, result, name, 2048)
        ctx.assertion(headers.get('x-request-id') == request_id, 'host path did not echo X-Request-Id')
    finally:
        conn.close()
    def observations():
        if not log.exists():
            return []
        with log.open() as stream:
            stream.seek(offset)
            lines = stream.read().splitlines()
        return [json.loads(line) for line in lines if line]
    def complete_observations():
        items = observations()
        return items if any(item['event'] == 'complete' for item in items) else None
    records = wait_for(ctx, complete_observations, 'engine observer never completed host-path request')
    requests = {item['request'] for item in records if item['event'] == 'request_headers'}
    ctx.assertion(len(requests) == 1, 'malformed request reached the engine or host request observer is incomplete')
    seen_headers = [item for item in records if item['event'] == 'request_headers']
    ctx.assertion(all('x-request-id' not in item['header_names'] for item in seen_headers),
                  'public X-Request-Id reached the engine')
    final = [item for item in records if item['event'] == 'response_body' and item['final']]
    ctx.assertion(len(final) == 1 and first_ns < final[0]['monotonic_ns'],
                  'host relay did not deliver bytes before the engine finished its answer')
    evidence.update(validation_status=rejected['status'], engine_observations=records,
                    client_first_byte_ns=first_ns, client_last_byte_ns=last_ns,
                    engine_last_body_ns=final[0]['monotonic_ns'], request_id_echoed=True,
                    engine_saw_request_id=False, client_response_bytes=sum(map(len, chunks)))
    ctx.save('em6-host-path', evidence)
    ctx.stop(name, role=role)
    ctx.assertion(not ctx.owned(name, role), 'host embedding stop retained engine processes')


def run(ctx, row):
    evidence = {}
    try:
        {'EM3': em3, 'EM5': em5, 'EM6': em6, 'EM7': em7, 'EM9': em9}[row['id']](ctx, evidence)
        if row['id'] == 'EM6':
            evidence['scope'] = 'vllm only'
            evidence['unobserved_engines'] = {
                'sglang': 'Protected -BIS launch skips the approved .pth observational hook.',
                'llamacpp': 'llama.cpp is not an ASGI server.',
            }
            return dict(verdict='pass', reason='All ADR 0036 EM6 criteria established for vLLM only (owner decision 2026-11-11)',
                        evidence=evidence)
        return dict(verdict='pass', reason='All ADR 0036 '+row['id']+' criteria established', evidence=evidence)
    except (AssertionError, ValueError, KeyError, TypeError, OSError, TimeoutError, subprocess.SubprocessError) as error:
        ctx.save('embedding-row-failure', dict(reason=str(error), evidence=evidence, logs=ctx.logs()))
        return dict(verdict='fail', reason=str(error), evidence=evidence)
