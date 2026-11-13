"""ADR 0034 LC12/LC17/LC18; isolated builds, controllers and read-only engines."""
import contextlib
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import threading
import sqlite3
import time

from .lmcache import (Protocol, GIB, TOKENIZER, cache_ledger, cache_http,
                      endurance, is_cache, live_identities, listening_sockets)
from .measurements import isolated_role

ROWS = [
    dict(id='LC12', priority=4, budget=7200, unified=True,
         criteria='ADR0034 LC12: per-engine historical qualified lmcache_driven deep launch, registration fallback or park/hit comparison, read-only GB10 fabric-handle support; qualifies nothing.'),
    dict(id='LC17', priority=4, budget=14400, measurement=True,
         criteria='ADR0034 LC17: auto pool basis, 32k retention with 4k interference, derived/raised caps, second-pool room exhaustion, stated pool and 15GiB server start bound.'),
    dict(id='LC18', priority=4, budget=14400, measurement=True,
         criteria='ADR0034 LC18: both engines, engine-driven restart_only, 15 alternating cycles with off answers, whole-prompt hits, stable covered GPU charge, no held KV or stops.'),
]
HERE = Path(__file__).resolve().parents[1]


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def fixtures(ctx, specs, engine):
    installation = Path(ctx.engines['LMCACHE'])
    python = installation / 'bin/python' if installation.is_dir() else installation.parent / 'python'
    package = 'vllm' if engine == 'vllm' else 'sglang'
    script = TOKENIZER.replace('(engine, "lmcache", "transformers")', repr((package, 'lmcache', 'transformers')))
    data = json.loads(ctx.command([python, '-c', script, ctx.engines['MODEL_CHAT'], json.dumps(specs), package],
        env={**os.environ, 'HF_HUB_OFFLINE':'1', 'TRANSFORMERS_OFFLINE':'1', 'TOKENIZERS_PARALLELISM':'false'}).stdout)
    versions = data['versions']
    approved = {'vllm': {'0.32.0','0.33.0','0.34.0rc1'}, 'sglang': {'0.5.22','0.5.23'}}
    ctx.assertion(versions[package] in approved[package] and versions['lmcache'] in {'0.5.6','0.5.7'},
                  'unapproved engine/LMCache installation')
    ctx.save('fixtures-' + engine, data)
    return data


@contextlib.contextmanager
def qualification_build(ctx, row):
    """The release executable is never overwritten or used for a variant row."""
    key = row.lower()
    binary = Path(ctx.engines.get('CAPYCTL_' + row, ctx.root / 'bin' / ('capyctl-' + key)))
    receipt_path = ctx.root / 'evidence' / ('lmcache-' + key + '-build.json')
    receipt = json.loads(receipt_path.read_text())
    ctx.assertion(receipt.get('qualification_only') is True and receipt.get('release_binary_replaced') is False,
                  'missing isolated qualification-build receipt')
    ctx.assertion(receipt['binary_sha256'] == sha(binary) and
                  receipt['patch_sha256'] == sha(HERE / ('lmcache-' + key + '.patch')),
                  'qualification binary/committed patch fingerprint mismatch')
    original = ctx.binary
    ctx.binary = lambda old=False: original(old) if old else binary
    try:
        yield receipt
    finally:
        ctx.binary = original


@contextlib.contextmanager
def engine_scope(ctx, engine):
    previous = ctx.engines['LMCACHE']
    ctx.engines['LMCACHE'] = ctx.installation('LMCACHE' if engine == 'vllm' else 'LMCACHE_SGLANG')
    try:
        yield
    finally:
        ctx.engines['LMCACHE'] = previous


def owned_flags(ctx, name, role):
    servers, connectors, config_files = [], [], []
    identities = live_identities(ctx, name, role)
    for identity in identities:
        args = (Path('/proc') / str(identity['pid']) / 'cmdline').read_bytes().decode().split('\0')
        flags = {}
        for index, arg in enumerate(args):
            key, _, value = arg.removeprefix('--').partition('=')
            if arg.startswith('--') and key in {'supported-transfer-mode','shm-name','kv-transfer-config','lmcache-config-file'}:
                flags[key] = value or (args[index+1] if index+1 < len(args) else '')
        if is_cache(identity):
            servers.append(flags)
        elif 'kv-transfer-config' in flags:
            configured = json.loads(flags['kv-transfer-config'])
            extra = configured.get('kv_connector_extra_config',{})
            connectors.append(dict(mode=extra.get('lmcache.mp.mp_transfer_mode','auto'),
                                   host=extra.get('lmcache.mp.host'),port=extra.get('lmcache.mp.port'),
                                   connector=configured.get('kv_connector')))
        elif 'lmcache-config-file' in flags:
            # Extract exactly three public connector settings, never arbitrary YAML.
            text = Path(flags['lmcache-config-file']).read_text()
            def field(key, default=None):
                match = re.search(r'^\s*' + re.escape(key) + r':\s*([^\n]+)',text,re.M)
                return match[1].strip().strip('\"\'') if match else default
            connectors.append(dict(mode=field('lmcache.mp.mp_transfer_mode','auto'),
                                   host=field('mp_host'),port=int(field('mp_port','0')),
                                   connector='sglang-unified'))
            config_files.append(dict(mode=connectors[-1]['mode']))
    return identities, servers, connectors, config_files


class ExtraProtocol(Protocol):
    def __init__(self, ctx, row, engine, transfer='engine_driven'):
        super().__init__(ctx,row)
        self.engine, self.transfer = engine, transfer
        self.residency = 'restart_only'
        self.prefix += '-' + engine

    def deploy(self, suffix, mode, pool=None):
        name = self.prefix + '-' + suffix
        extra = dict(lmcache=mode)
        if self.engine == 'vllm':
            extra['engine_config'] = dict(accept_extra_args=True,extra_args=['--enable-prompt-tokens-details'])
        if pool:
            extra['lmcache_pool'] = pool
        self.ctx.deploy(name,'lmcache',residency=self.residency,extra=extra)
        self.names.append(name)
        ledger = cache_ledger(self.ctx,name,self.role)
        self.record('deployment',name=name,mode=mode,residency=self.residency,status=self.ctx.status(name),ledger=ledger)
        if mode == 'required':
            self.ctx.assertion(ledger['state']=='on','required connector did not start')
            self.server_identity = self.first_server_gpu = None
            self.journal_start = self.ctx.sql('SELECT COALESCE(MAX(rowid),0) AS n FROM journal_entries')[0]['n']
            self.rendering(name)
        else:
            self.check(ledger['planned']=='off','off launch unexpectedly enabled connector')
        return name

    def rendering(self,name):
        identities, servers, connectors, files = owned_flags(self.ctx,name,self.role)
        ledger = cache_ledger(self.ctx,name,self.role)
        self.check(bool(servers) and bool(connectors),'missing proven server or engine connector rendering')
        for flags in servers:
            self.check(flags.get('supported-transfer-mode','lmcache_driven') == self.transfer,'wrong server transfer rendering')
            self.check('shm-name' not in flags,'unexpected shared-memory transfer')
        for configured in connectors:
            self.check(configured['mode'] == ('auto' if self.transfer=='lmcache_driven' else self.transfer),
                       'wrong engine transfer rendering')
            self.check(configured['host']=='127.0.0.1' and configured['port']==ledger['rpc_port'],
                       'connector does not point to owned loopback server')
        listeners = listening_sockets(self.ctx,[item for item in identities if is_cache(item)])
        self.check({ledger['rpc_port'],ledger['http_port']}.issubset({item['port'] for item in listeners}),
                   'server listeners not proven owned')
        self.record('rendering',servers=servers,connectors=connectors,config=files,listeners=listeners,transfer=self.transfer)
        self.ctx.assertion(not self.failures,'; '.join(self.failures))

    def finish(self):
        for name in self.names[:]:
            self.retire(name)
        self.record('qualification-scope', residency=self.residency,
                    transfer=self.transfer, release_table_mutated=False,
                    qualification='LC12 settles an inference only' if self.row['id']=='LC12' else 'live qualification only; CPU tests do not qualify')
        return dict(verdict='fail' if self.failures else 'skip' if self.ctx.functional_only else 'pass',
                    reason='; '.join(dict.fromkeys(self.failures)) or 'all requested live criteria observed',
                    evidence=self.records)


def fabric_readout(ctx):
    # CUDA's read-only CU_DEVICE_ATTRIBUTE_HANDLE_TYPE_FABRIC_SUPPORTED = 128.
    product=ctx.command(['nvidia-smi','--query-gpu=name','--format=csv,noheader']).stdout
    ctx.assertion('GB10' in product,'LC12 requires the GB10, not merely a unified-memory flag')
    # https://meta-pytorch.org/monarch/stable/rust-api/rdmaxcel_sys/constant.CU_DEVICE_ATTRIBUTE_HANDLE_TYPE_FABRIC_SUPPORTED.html
    script = '''import ctypes,json
c=ctypes.CDLL("libcuda.so.1")
s=c.cuInit(0)
if s: raise RuntimeError("cuInit status "+str(s))
n=ctypes.c_int(); s=c.cuDeviceGetCount(ctypes.byref(n))
if s: raise RuntimeError("cuDeviceGetCount status "+str(s))
rows=[]
for i in range(n.value):
 d=ctypes.c_int(); s=c.cuDeviceGet(ctypes.byref(d),i)
 if s: raise RuntimeError("cuDeviceGet status "+str(s))
 value=ctypes.c_int(); s=c.cuDeviceGetAttribute(ctypes.byref(value),128,d)
 rows.append(dict(device=i,attribute=128,cuda_status=s,supported=bool(value.value) if s==0 else None))
print(json.dumps(rows))'''
    result = json.loads(ctx.command(['python3','-c',script]).stdout)
    ctx.assertion(result and all(item['cuda_status']==0 for item in result),'GB10 fabric attribute could not be read')
    return result


def historical(ctx,row,engine,receipt):
    p = ExtraProtocol(ctx,row,engine,'lmcache_driven')
    p.residency = 'deep'
    p.record('qualification-build',**receipt)
    p.record('fabric-handles',devices=fabric_readout(ctx),qualification='LC12 qualifies nothing')
    data = fixtures(ctx,[('main',32000)],engine)['fixtures']['main']
    off = p.deploy('off','off')
    reference = p.request(off,data,'off')['answer']
    p.retire(off)
    name=p.prefix+'-historical'
    ctx.deploy(name,'lmcache',residency='deep',extra=dict(lmcache='auto'))
    p.names.append(name)
    ledger=cache_ledger(ctx,name,p.role)
    p.record('historical-launch',status=ctx.status(name),ledger=ledger)
    p.check(ledger['planned']=='on' and not ledger['uncertified'],'historical build did not list the tested combination')
    if ledger['state']=='off':
        p.check(ledger['reason']=='engine_start_failed','historical fallback reason is not engine_start_failed')
        bindings=ctx.sql('SELECT incarnation FROM runtime_bindings WHERE deployment_id=(SELECT id FROM deployments WHERE name=?)',(name,))
        lines=[]
        for binding in bindings:
            for path in ctx.roles[p.role]['state'].rglob(str(binding['incarnation'])+'.log'):
                lines.extend(line for line in ctx.clean(path.read_text(errors='replace')).splitlines()
                             if re.search(r'register|sleep|memory.saver|CUDA|IPC|handle|LMCache|error|failed',line,re.I))
        p.record('registration-fallback',log_lines=lines)
        p.check(bool(lines) and any(re.search(r'register|cuMemGetHandle|cudaIpcGetMemHandle|export.*handle|handle.*(?:unsupported|not supported)',line,re.I) for line in lines),
                'missing original KV-registration failure log line')
        p.request(name,data,'fallback-off',expected=reference)
    else:
        p.check(ledger['state']=='on','unexpected historical launch state')
        p.rendering(name)
        p.request(name,data,'historical-seed',cache=True,expected=reference)
        p.lifecycle('park',name)
        p.memory(name,'historical-park',cache=False)
        p.lifecycle('wake',name)
        p.request(name,data,'historical-first-hit',cache=True,expected=reference,hit=True)
    return p.finish()


def pool_status(status):
    """Use structured status field names; unrelated prose is not pool evidence."""
    found={}
    def walk(node):
        if isinstance(node,dict):
            for key,value in node.items():
                if key in {'pool_basis','pool_warning','pool_tokens','pool_bytes','host_kv_limit','host_kv_bytes'}:
                    found.setdefault(key,[]).append(value)
                walk(value)
        elif isinstance(node,list):
            for item in node: walk(item)
    walk(status)
    return found


def pool_case(ctx,row,label,limit,context,expected,warning,pool=None,retention=False,occupied=False):
    settings=[] if limit is None else [f'host.resource_policy.memory.system.host_kv_limit={limit}']
    with isolated_role(ctx,'lc17-'+label,settings=settings):
        p=ExtraProtocol(ctx,row,'vllm','lmcache_driven')
        if occupied:
            # Keep the first private pool charged while its engine is restart-parked.
            first=p.deploy('occupant','required',pool='1GiB')
            p.lifecycle('park',first)
        name=p.prefix+'-'+label
        extra=dict(lmcache='auto',engine_config=dict(context_length=context,accept_extra_args=True,
                   memory=dict(kv_cache='10GiB' if context>32768 else '5GiB'),
                   extra_args=['--enable-prompt-tokens-details']))
        if pool: extra['lmcache_pool']=pool
        # Observe the server from owned /proc start ticks to its first /status.
        # Unlike the deployment's total start time this excludes model loading.
        observed=[]; errors=[]; stop=threading.Event()
        def observe_start():
            while not stop.is_set():
                try:
                    deployments=ctx.sql('SELECT id FROM deployments WHERE name=?',(name,))
                    if not deployments:
                        stop.wait(.1); continue
                    for database in ctx.roles[p.role]['state'].rglob('commands.sqlite'):
                        with sqlite3.connect(database.as_uri()+'?mode=ro',uri=True,timeout=.2) as connection:
                            rows=connection.execute(
                                "SELECT p.pid,p.boot,p.ticks FROM processes p JOIN commands c ON c.command_id=p.command_id "
                                "WHERE c.deployment=? AND c.claim=1 AND p.role='lmcache'",
                                (deployments[0]['id'],)).fetchall()
                        for pid,boot,ticks in rows:
                            stat=(Path('/proc')/str(pid)/'stat').read_text()
                            actual=int(stat[stat.rfind(')')+2:].split()[19])
                            ctx.assertion(actual==ticks and boot==Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                                          'large-pool ownership identity changed')
                            args=(Path('/proc')/str(pid)/'cmdline').read_bytes().decode().split('\0')
                            port=int(args[args.index('--http-port')+1])
                            identity=dict(pid=pid,boot_id=boot,start_ticks=ticks,role='lmcache')
                            listeners=listening_sockets(ctx,[identity])
                            if port not in {item['port'] for item in listeners}:
                                continue
                            connection=http.client.HTTPConnection('127.0.0.1',port,timeout=1)
                            try:
                                connection.request('GET','/status')
                                response=connection.getresponse()
                                body=json.loads(response.read())
                                if response.status!=200: continue
                            finally:
                                connection.close()
                            uptime=float(Path('/proc/uptime').read_text().split()[0])
                            observed.append(dict(server_age_s=uptime-ticks/os.sysconf('SC_CLK_TCK'),
                                                 status=body,identity=identity,listeners=listeners))
                            return
                except Exception as error:
                    errors.append(type(error).__name__)
                stop.wait(.1)
        sampler=threading.Thread(target=observe_start,daemon=True)
        if pool=='15GiB': sampler.start()
        try:
            ctx.deploy(name,'lmcache',residency='restart_only',extra=extra)
            p.names.append(name)
        finally:
            stop.set()
            if sampler.is_alive(): sampler.join(timeout=10)
            ctx.assertion(not sampler.is_alive(),'large-pool observer failed to stop')
        ledger=cache_ledger(ctx,name,p.role)
        status=ctx.status(name)
        fields=pool_status(status)
        p.record('pool-sizing',case=label,context_length=context,host_kv_limit=limit,
                 stated_pool=pool,status=status,fields=fields,ledger=ledger)
        p.check((ledger['pool_bytes'] or 0)==expected*GIB,label+': incorrect pool size')
        if expected:
            p.check(ledger['state']=='on',label+': cache did not start')
            # The exact operator warning and automatic model/context basis must
            # be exposed, not inferred from the chosen numeric pool.
            text=json.dumps(status).lower()
            p.check(('raise host_kv_limit or set lmcache_pool' in text)==warning,label+': incorrect warning')
            if not pool:
                p.check('qwen3' in text and ('per token' in text or 'bytes_per_token' in text),label+': missing pool basis')
                p.check(str(context) in text.replace(',',''),label+': pool basis omits the configured context')
            if pool=='15GiB':
                p.record('large-pool-start',observations=observed,transient_errors=errors[-10:],bound_s=30)
                p.check(bool(observed) and observed[0]['server_age_s']<=30,'15GiB pool /status exceeded start bound or was unobserved')
            if retention:
                data=fixtures(ctx,[('main',32000),('noise',4000)],'vllm')['fixtures']
                seed=p.request(name,data['main'],'seed',cache=True)
                p.request(name,data['noise'],'interference',cache=True)
                p.lifecycle('park',name); p.lifecycle('wake',name)
                p.request(name,data['main'],'retained',cache=True,hit=True,expected=seed['answer'])
            charge=json.loads(ledger['footprint'])
            p.check(sum(item[2] for item in charge.get('allocations',[]))>=expected*GIB,'pool charge missing from host-KV ledger')
        else:
            p.check(ledger['planned_reason']=='host_kv_room' or ledger['reason']=='host_kv_room','no-room launch not off for host_kv_room')
        return p.finish()


def sizing(ctx,row):
    results=[]
    for args in (
        # T45 / ADR0034 LC17: 12.17GiB is a displayed approximation.
        # Byte quantities must be integral (effective::parse_bytes); round down.
        ('auto32','13067437998B',32768,8,False,None,True,False),
        ('auto8','13067437998B',8192,4,False,None,False,False),
        ('derived-cap','13067437998B',65536,12,True,None,False,False),
        ('raised-cap','24GiB',65536,15,False,None,False,False),
        ('room3','4GiB',32768,3,True,None,False,True),
        ('room0','1GiB',32768,0,False,None,False,True),
        ('stated','24GiB',32768,15,False,'15GiB',False,False)):
        results.append(pool_case(ctx,row,*args))
    failures=[r['reason'] for r in results if r['verdict']!='pass']
    return dict(verdict='fail' if failures else 'pass',reason='; '.join(failures) or 'all LC17 pool cases passed',evidence=results)


def run(ctx,row):
    results=[]
    try:
        if row['id']=='LC17': return sizing(ctx,row)
        with qualification_build(ctx,row['id']) as receipt:
            for engine in ('vllm','sglang'):
                with engine_scope(ctx,engine), isolated_role(ctx,row['id'].lower()+'-'+engine):
                    if row['id']=='LC12':
                        results.append(historical(ctx,row,engine,receipt))
                    else:
                        p=ExtraProtocol(ctx,row,engine)
                        p.record('qualification-build',**receipt)
                        labels=['same']+['new-'+str(i) for i in range(1,15,2)]
                        data=fixtures(ctx,[(label,32000) for label in labels],engine)
                        endurance(p,data)
                        results.append(p.finish())
        failures=[r['reason'] for r in results if r['verdict']!='pass']
        return dict(verdict='fail' if failures else 'pass',reason='; '.join(failures) or row['id']+' criteria passed on both engines',evidence=results)
    except Exception as error:
        # T45: failed starts can leave no runtime binding for logs(name).
        # Retain sanitized controller/server logs too, before the session is gone.
        ctx.save('failure-logs',dict(logs=ctx.logs()))
        return dict(verdict='fail',reason=type(error).__name__+': '+str(error),evidence=results)
