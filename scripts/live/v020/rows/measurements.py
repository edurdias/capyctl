"""Bounded live measurements. Missing evidence fails; observations never certify a subset."""
import concurrent.futures
import contextlib
import http.client
import json
import math
import os
from pathlib import Path
import re
import statistics
import threading
import time

from .engines import answered, document, stopped
from .functional import embedding, vectors


ROWS = [dict(id=key, priority=2, budget=budget, measurement=True, criteria=criteria)
        for key, budget, criteria in (
    ("EM1", 900, "Qwen3-Embedding-4B BF16/Q8_0: native 2560 dimensions, ordered batch, API/certification/LMCache evidence and memory/start/charge."),
    ("EM8", 1800, "Embedding latency/load matrix and pinned retrieval; Qwen8B 2048-input float/base64 router memory bound."),
    ("OT8", 3000, "200 requests per tracing arm/engine, phase arrays and throughput; SGLang trace levels 1 and 3."),
    ("TF2", 900, "TensorFold cold/warm readiness, full-context eight-stream device/process/host memory and startup sizing."),
    ("TF3", 1200, "TensorFold request options, drafted/no-draft greedy SHA equivalence and 1/8 stream baseline throughput."),
    ("TR3", 3000, "TensorRT cold/three warm starts, 30-minute mixed traffic and unchanged block-rounded pool beside 20GiB allocation."))]
ROWS += [dict(id="TR3-140RC3",priority=2,budget=3000,measurement=True,
              criteria="TensorRT-LLM 1.4.0rc3: full TR3 protocol after tiny CUDA kernel preflight.")]


@contextlib.contextmanager
def sampling(ctx):
    """Sample during the operation, including startup/response peaks, not just after it."""
    records, errors, done = [], [], threading.Event()
    def sample():
        while not done.is_set():
            try:
                records.append(dict(at_s=time.monotonic(), **ctx.sample(), **process_memory(ctx)))
            except Exception as error:
                errors.append(type(error).__name__)
                break
            done.wait(.1)
    thread = threading.Thread(target=sample, daemon=True)
    thread.start()
    try:
        yield records
    finally:
        done.set()
        thread.join(timeout=min(5, ctx.remaining()))
        ctx.save('memory-' + str(time.monotonic_ns()), dict(samples=records, errors=errors))
        ctx.assertion(not thread.is_alive() and not errors and bool(records), "memory sampler failed: " + ",".join(errors))


def argv_settings(ctx, name):
    allowed = {"runner", "convert", "is-embedding", "embeddings", "batch-size", "ubatch-size",
               "context", "parallel", "name", "host", "port", "backend", "snapshot-dir",
               "no-update-check", "no-drafts", "max_tokens", "max-tokens", "config", "model"}
    result = {}
    for identity in ctx.owned(name):
        path = Path('/proc') / str(identity['pid']) / 'cmdline'
        if not path.exists():
            continue
        args = path.read_bytes().decode(errors='replace').split('\0')
        for index, arg in enumerate(args):
            key, _, value = arg.removeprefix('--').partition('=')
            if arg.startswith('--') and key in allowed:
                result[key] = value or (args[index+1] if index+1 < len(args) and not args[index+1].startswith('--') else True)
    return result


def direct(ctx, name, path):
    settings = argv_settings(ctx, name)
    ctx.assertion(str(settings.get('port', '')).isdigit(), "owned loopback engine port unavailable")
    connection = http.client.HTTPConnection('127.0.0.1', int(settings['port']), timeout=min(15, ctx.remaining()))
    try:
        connection.request('GET', path)
        response = connection.getresponse()
        body = response.read().decode()
        ctx.assertion(response.status == 200, "engine " + path + " unavailable without key access: HTTP " + str(response.status))
        try:
            return json.loads(body)
        except ValueError:
            return body
    finally:
        connection.close()


def metric_values(text):
    if isinstance(text, dict):
        return text
    result = {}
    for line in text.splitlines():
        match = re.match(r'([^#\s{]+)(?:\{[^}]*\})?\s+([-+0-9.eE]+)(?:\s|$)', line)
        if match:
            result.setdefault(match[1], []).append(float(match[2]))
    return result


def number(log, names):
    for name in names:
        matches = re.findall(r'(?i)\b' + re.escape(name) + r'[\s"\x27:=]+([0-9]+(?:\.[0-9]+)?)', log)
        if matches:
            return float(matches[-1])
    raise AssertionError('missing numeric engine evidence: ' + '/'.join(names))


def validate_samples(ctx, samples):
    for key in ('gpu_used_bytes', 'mem_available_bytes', 'router_rss_bytes'):
        ctx.assertion(all(isinstance(sample.get(key), (int, float)) and sample[key] >= 0 for sample in samples), 'missing numeric samples: ' + key)


def advertises_embeddings(response, name):
    return response.get('status') == 200 and any(
        model.get('id') == name and model.get('capyctl', {}).get('api') == 'embeddings'
        for model in response.get('body', {}).get('data', []))


def em1(ctx, evidence):
    for engine in ('vllm033', 'sglang0523', 'llamacpp090'):
        name = 'em1-' + engine
        start = time.monotonic()
        with sampling(ctx) as samples:
            ctx.deploy(name, engine, model='embedding', api='embeddings', residency='restart_only')
            ready_s = time.monotonic()-start
            first = vectors(ctx, embedding(ctx, name, 'qualification probe'), name, 1)
            inputs = ['qualification sentence ' + str(i) for i in range(16)]
            batch = vectors(ctx, embedding(ctx, name, inputs), name, 16)
        validate_samples(ctx, samples)
        ctx.assertion(all(len(v) == 2560 for v in first + batch), 'Qwen4B native dimension mismatch')
        # Compare each batch slot with its own single-input result: indices alone do not prove input order.
        for index, text in enumerate(inputs):
            single = vectors(ctx, embedding(ctx, name, text), name, 1)[0]
            distance = math.sqrt(sum((a-b)**2 for a,b in zip(single, batch[index]))) / max(math.sqrt(sum(a*a for a in single)), 1e-30)
            ctx.assertion(distance <= .02, 'batch vector does not match input at index ' + str(index))
        status = ctx.status(name)
        models = ctx.request(name, path='/v1/models', body={}, method='GET')
        settings = argv_settings(ctx, name)
        evidence[engine] = dict(start_s=ready_s, memory=samples, status=status, models=models, settings=settings)
        rendered = json.dumps(status)
        ctx.assertion('embeddings' in rendered and 'certified' in rendered, 'status lacks embeddings certification field')
        ctx.assertion('embeddings_unsupported' in rendered, 'LMCache off reason missing')
        ctx.assertion(advertises_embeddings(models, name), '/v1/models missing embeddings API metadata')
        ctx.assertion(any(word in rendered for word in ('charge', 'reservation', 'reserved')), 'memory charge evidence missing')
        if engine == 'llamacpp090':
            ctx.assertion(settings.get('embeddings') and settings.get('batch-size') == settings.get('ubatch-size') and str(settings.get('batch-size', '')).isdigit(), 'llama.cpp rendered embedding batch sizes missing/mismatched')
        elif engine.startswith('vllm'):
            ctx.assertion(settings.get('runner') == 'pooling', 'vLLM pooling runner not rendered')
        else:
            ctx.assertion(settings.get('is-embedding'), 'SGLang embedding option not rendered')
        stopped(ctx, name)


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[min(len(ordered)-1, math.ceil(fraction*len(ordered))-1)]


def embedding_load(ctx, name, batch, length, clients):
    # Token-ID input makes requested input lengths exact rather than inferred from text.
    payload = [[100] * length for _ in range(batch)]
    def one(_):
        result = embedding(ctx, name, payload)
        vectors(ctx, result, name, batch)
        return result['seconds']
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=clients) as pool:
        values = list(pool.map(one, range(clients * 3)))
    elapsed = time.monotonic()-start
    ctx.assertion(all(value > 0 and math.isfinite(value) for value in values), 'invalid embedding latency')
    return dict(batch=batch, input_tokens=length, clients=clients, seconds=values,
                p50_s=percentile(values,.5), p95_s=percentile(values,.95), inputs_per_s=batch*len(values)/elapsed)


def em8(ctx, evidence):
    fixture = ctx.engines.get('EMBEDDING_RETRIEVAL_SET')
    data = json.loads(Path(fixture).read_text()) if fixture and Path(fixture).is_file() else None
    if data is None:
        evidence['retrieval_skipped'] = 'retrieval set not pinned'
    else:
        ctx.assertion(data.get('revision') and data.get('source') and data.get('queries') and data.get('documents'), 'retrieval fixture needs source, revision, documents and queries with relevant indices')
    for engine in ('vllm033', 'sglang0523', 'llamacpp090'):
        name = 'em8-' + engine
        ctx.deploy(name, engine, model='embedding', api='embeddings')
        matrix = []
        evidence[engine] = dict(matrix=matrix)
        for batch in (1,16,256):
            for length in (32,512,8192):
                for clients in (1,4,16):
                    matrix.append(embedding_load(ctx,name,batch,length,clients))
        if data is not None:
            docs = vectors(ctx, embedding(ctx,name,data['documents']),name,len(data['documents']))
            hits = []
            for query in data['queries']:
                vector = vectors(ctx, embedding(ctx,name,query['text']),name,1)[0]
                def cosine(other):
                    return sum(a*b for a,b in zip(vector,other))/max(math.sqrt(sum(a*a for a in vector)*sum(b*b for b in other)),1e-30)
                ranked = sorted(range(len(docs)),key=lambda i:cosine(docs[i]),reverse=True)
                hits.append(int(ranked[0] in query['relevant']))
            evidence[engine]['retrieval'] = dict(source=data['source'],revision=data['revision'],hits=hits,recall_at_1=statistics.mean(hits))
        stopped(ctx,name)
    name = 'em8-large'
    ctx.deploy(name,'vllm033',model='embedding8b',api='embeddings')
    for encoding in ('float','base64'):
        before = ctx.sample()
        with sampling(ctx) as samples:
            response = embedding(ctx,name,['qualification probe']*2048,encoding_format=encoding)
        values = vectors(ctx,response,name,2048)
        dimension = len(values[0]) if encoding == 'float' else len(__import__('base64').b64decode(values[0]))//4
        ctx.assertion(dimension == 4096,'Qwen8B dimension mismatch')
        validate_samples(ctx,samples)
        peak_delta = max(item['router_rss_bytes'] for item in samples)-before['router_rss_bytes']
        # Retaining even one whole float32 answer is 32 MiB. Both encodings must stay below it.
        bound = 2048*4096*4
        evidence[encoding] = dict(memory=samples,before=before,peak_delta_bytes=peak_delta,bound_bytes=bound)
        ctx.assertion(peak_delta < bound,'router RSS grew by a complete decoded answer or more')
    stopped(ctx,name)


def tf2(ctx,evidence):
    name='tf2-native'
    for kind in ('cold','warm'):
        baseline=ctx.sample()
        start=time.monotonic()
        with sampling(ctx) as samples:
            if kind=='cold':
                ctx.deploy(name,'tensorfold126',model='tensorfold')
            else:
                ctx.lifecycle('start',name)
            ready_s=time.monotonic()-start
            health=direct(ctx,name,'/health')
            ctx.assertion(health.get('status')=='ok' and name in health.get('model_ids',[]) and health.get('max_batch_size')==8,'native health readiness mismatch')
            # Full-context requests must demonstrate their input size via usage, not assumed text length.
            body=dict(messages=[dict(role='user',content=' hello'*32700)],max_tokens=32,temperature=0)
            result=ctx.bench(name,concurrency=8,rounds=1,body=body)
            ctx.assertion(result['ok']==8,'full-context concurrency failed')
            for response in result['results']:
                usage=document(response).get('usage',{})
                ctx.assertion(usage.get('prompt_tokens',0)>=32700,'benchmark did not exercise full context')
            metrics=metric_values(direct(ctx,name,'/metrics'))
        validate_samples(ctx,samples)
        logs=engine_logs(ctx,name)
        model_bytes=number(logs,('model_bytes','model bytes','model_size_bytes'))
        streams=number(logs,('stream_count','num_streams','streams'))
        stream_bytes=number(logs,('stream_bytes','stream bytes'))
        peaks=[value for key,values in metrics.items() if 'device_memory_peak_bytes' in key for value in (values if isinstance(values,list) else [values])]
        ctx.assertion(peaks and max(peaks)>model_bytes and streams==8,'missing device peak/stream startup evidence')
        expected=.29*1024**3
        ctx.assertion(abs(stream_bytes-expected)<=.05*1024**3,'stream bytes outside 0.29 GiB rounding allowance')
        evidence[kind]=dict(ready_s=ready_s,memory=samples,health=health,metrics=metrics,model_bytes=model_bytes,stream_count=streams,stream_bytes=stream_bytes,expected_stream_bytes=expected,host_available_drop_bytes=baseline['mem_available_bytes']-min(s['mem_available_bytes'] for s in samples),settings=argv_settings(ctx,name))
        settings=evidence[kind]['settings']
        ctx.assertion(settings.get('host')=='127.0.0.1' and settings.get('backend')=='cuda' and settings.get('snapshot-dir')=='none' and settings.get('parallel')=='8' and settings.get('no-update-check'),'native rendered command incomplete/mismatched')
        stopped(ctx,name)


def tf3(ctx,evidence):
    ctx.assertion(ctx.engines.get('TENSORFOLD_066'),'required TensorFold 0.6.6 comparison environment unavailable: TENSORFOLD_066')
    hashes={}
    arms=(('tensorfold126',True,False),('tensorfold126',True,True),
          ('tensorfold126',False,False),('TENSORFOLD_066',True,False))
    for engine,drafts,thinking in arms:
        baseline=engine=='TENSORFOLD_066'
        label='baseline' if baseline else ('drafted' if drafts else 'no-drafts')
        name='tf3-'+label+('-thinking' if thinking else '')
        config={'tensorfold':{'thinking':thinking}}
        if not drafts:
            config.update(accept_extra_args=True,extra_args=['--no-drafts'])
        ctx.deploy(name,engine,model='tensorfold',extra={'engine_config':config})
        records=[]
        if not baseline:
            for temperature in (0,0.7):
                for stream in (False,True):
                    response=ctx.request(name,stream=stream,body=dict(messages=[dict(role='user',content='What is 17 + 25?')],max_tokens=128,temperature=temperature,stream=stream))
                    answered(ctx,response,stream)
                    if not stream:
                        body=document(response)
                        ctx.assertion(isinstance(body.get('tensorfold'),dict),'TensorFold response metadata missing')
                        if thinking:
                            ctx.assertion(bool(body['choices'][0]['message'].get('reasoning_content')),'thinking response missing reasoning_content')
                        if temperature==0 and not thinking:
                            sha=body['tensorfold'].get('token_sha')
                            ctx.assertion(isinstance(sha,str) and sha,'greedy token_sha unavailable')
                            hashes[label]=sha
                    records.append(dict(thinking=thinking,temperature=temperature,stream=stream,seconds=response['seconds']))
            rejected=ctx.request(name,body=dict(messages=[dict(role='user',content='hello')],cache_salt='qualification'))
            ctx.assertion(rejected.get('status')==400,'TensorFold accepted cache_salt')
        performance=[ctx.bench(name,concurrency=clients,tokens=256,rounds=3) for clients in (1,8)]
        ctx.assertion(all(item['ok']==item['requests'] and item['tokens_per_s']>0 for item in performance),'throughput arm lost requests or output counts')
        evidence[name]=dict(requests=records,throughput=[{key:value for key,value in arm.items() if key!='results'} for arm in performance])
        stopped(ctx,name)
    ctx.assertion(hashes['drafted']==hashes['no-drafts'],'drafted and no-drafts greedy token SHA differs')
    evidence['greedy_sha_equal']=True


def run(ctx,row):
    evidence={}
    try:
        if row['id']=='TR3-140RC3':
            failure=trt_preflight(ctx)
            if failure:
                return failure
            tr3(ctx,evidence,engine='TENSORRT_LLM_140RC3')
        else:
            {'EM1':em1,'EM8':em8,'OT8':ot8,'TF2':tf2,'TF3':tf3,'TR3':tr3}[row['id']](ctx,evidence)
        if evidence.get('retrieval_skipped'):
            return dict(verdict='skip',reason=evidence['retrieval_skipped'],evidence=evidence)
        return dict(verdict='pass',reason='All required measurement arms and evidence bounds passed',evidence=evidence)
    except (AssertionError,ValueError,KeyError,TypeError,OSError,TimeoutError) as error:
        return dict(verdict='fail',reason=str(error),evidence=evidence)


@contextlib.contextmanager
def isolated_role(ctx, label, settings=(), environment=None):
    """A private standalone role; never reconfigure an existing owner's role."""
    import subprocess
    original = ctx.roles.get('standalone')
    registered = set(ctx.registered)
    deployments = list(ctx.deployments)
    state = ctx.root / ('state-' + label)
    state.mkdir(mode=0o700, exist_ok=False)
    binary = ctx.binary()
    argv = [str(binary), '--state-dir', str(state), 'start', 'standalone',
            '--listen', '127.0.0.1:19753', '--management-listen', '127.0.0.1:19754',
            '--engine-ports', '19790-19819', '--models-root', str(ctx.model_store),
            '--model-sources-path', str(ctx.root/'sources'), '--json']
    for setting in settings:
        argv.extend(('--set', setting))
    log_path = ctx.root / (label + '.log')
    with log_path.open('ab', buffering=0) as log:
        process = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT,
                                   env={**os.environ, **(environment or {})}, start_new_session=True)
    info = dict(state=state,binary=binary,process=process,port=19753,
                cred=state/'identity/credentials',db=state/'server/srv.sqlite3')
    ctx.roles['standalone'] = info
    ctx.registered = set()
    ctx.deployments = []
    try:
        while not info['cred'].exists() or 'ready' not in log_path.read_text(errors='replace').lower():
            ctx.assertion(process.poll() is None, 'isolated role exited before ready')
            ctx.assertion(ctx.remaining() > 1, 'isolated role readiness deadline')
            time.sleep(.2)
        yield info
    finally:
        try:
            ctx.cleanup()
        finally:
            process.terminate()
            try:
                process.wait(timeout=min(30,ctx.remaining()))
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            ctx.roles.pop('standalone',None)
            if original is not None:
                ctx.roles['standalone']=original
            ctx.registered=registered
            ctx.deployments=deployments


def timing_phases(response):
    headers={key.lower():value for key,value in response.get('response_headers',{}).items()}
    value=headers.get('x-capyctl-timing','')
    phases={}
    for key,number_value in re.findall(r'([A-Za-z_]+)(?:;dur=|=|:)\s*([0-9.]+)',value):
        phases[key]=float(number_value)
    return phases


def load_chat(ctx,name,n=200,concurrency=8,headers=None):
    def one(_):
        result=ctx.request(name,stream=True,headers=headers,
            body=dict(messages=[dict(role='user',content='Write a short story about a lighthouse. /no_think')],
                      max_tokens=256,temperature=0,stream=True,stream_options={'include_usage':True}))
        answered(ctx,result,True)
        count=0
        for chunk in result['chunks']:
            if chunk=='[DONE]':
                continue
            item=json.loads(chunk) if isinstance(chunk,str) else chunk
            count=max(count,item.get('usage',{}).get('completion_tokens',0))
        ctx.assertion(count>0,'SSE usage missing output token count')
        phases=timing_phases(result)
        ctx.assertion(phases,'x-capyctl-timing missing numeric router phases')
        return dict(seconds=result['seconds'],ttft_s=result['ttft_s'],output_tokens=count,phases=phases)
    start=time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        records=list(pool.map(one,range(n)))
    elapsed=time.monotonic()-start
    return dict(n=n,concurrency=concurrency,requests=records,elapsed_s=elapsed,
                tokens_per_s=sum(record['output_tokens'] for record in records)/elapsed)


def ot8(ctx,evidence):
    collector=ctx.engines.get('OTELCOL')
    ctx.assertion(collector and Path(collector).is_file(),'required OpenTelemetry collector unavailable: OTELCOL')
    config=ctx.root/'ot8-collector.yaml'
    config.write_text('receivers:\n  otlp:\n    protocols:\n      http:\n        endpoint: 127.0.0.1:14318\nexporters:\n  debug:\n    verbosity: basic\nservice:\n  telemetry:\n    logs:\n      level: info\n  pipelines:\n    traces:\n      receivers: [otlp]\n      exporters: [debug]\n')
    collector_process=ctx.spawn([collector,'--config',config],'ot8-collector')
    try:
        time.sleep(1)
        ctx.assertion(collector_process.poll() is None,'collector failed to start')
        for engine in ('vllm033','sglang0523','llamacpp090'):
            arms={}
            evidence[engine]=arms
            for arm in ('off','sampled','unsampled','engines'):
                if arm=='engines' and engine=='llamacpp090':
                    continue
                settings=['server.observability.timing_header=true']
                if arm!='off':
                    settings.append('server.observability.tracing.endpoint=http://127.0.0.1:14318/v1/traces')
                if arm=='engines':
                    settings.append('host.observability.tracing.engines=on')
                label='ot8-'+engine+'-'+arm
                with isolated_role(ctx,label,settings):
                    ctx.deploy(label,engine)
                    if arm=='engines':
                        ctx.assertion('not_installed' not in json.dumps(ctx.status(label)),'engine tracing SDK unavailable')
                    parent='00-1234567890abcdef1234567890abcdef-1234567890abcdef-'+('00' if arm=='unsampled' else '01')
                    # Ten identical warm-ups are excluded from the measured arrays.
                    load_chat(ctx,label,n=10,headers={'traceparent':parent})
                    arms[arm]=load_chat(ctx,label,headers={'traceparent':parent})
                    stopped(ctx,label)
                ctx.assertion(collector_process.poll() is None,'collector exited during measurement')
            baseline=arms['off']['tokens_per_s']
            for arm in arms:
                arms[arm]['throughput_ratio_to_off']=arms[arm]['tokens_per_s']/baseline
            if engine=='sglang0523':
                arms['direct']=sglang_levels(ctx)
        # OT8 sets a measured cost, not an invented universal overhead threshold.
        # Require the exporter to have delivered actual spans, not merely accepted its config.
        time.sleep(min(6,ctx.remaining()))
        collector_log=(ctx.root/'ot8-collector.log').read_text(errors='replace')
        ctx.assertion(re.search(r'(?i)(spans|span count)',collector_log),'collector has no exported span evidence')
    finally:
        collector_process.terminate()
        collector_process.wait(timeout=min(15,ctx.remaining()))


def sglang_levels(ctx):
    """Direct comparison is required: SGLANG_TRACE_LEVEL is reserved under CapyCTL."""
    import subprocess
    engine=Path(ctx.engines['SGLANG_0523'])
    python=engine/'bin/python' if engine.is_dir() else engine.parent/'python'
    model=ctx.engines['MODEL_CHAT']
    result={}
    for level in ('off','1','3'):
        label='ot8-direct-'+level
        argv=[str(python),'-m','sglang.launch_server','--model-path',model,
              '--served-model-name','ot8-direct','--host','127.0.0.1','--port','19829',
              '--context-length','8192','--mem-fraction-static','0.3']
        environment=dict(os.environ)
        if level!='off':
            environment.update(SGLANG_TRACE_LEVEL=level,OTEL_EXPORTER_OTLP_TRACES_PROTOCOL='http/protobuf')
            argv += ['--enable-trace','--otlp-traces-endpoint','http://127.0.0.1:14318/v1/traces','--otlp-service-name','sglang']
        with (ctx.root/(label+'.log')).open('ab',buffering=0) as log:
            process=subprocess.Popen(argv,stdout=log,stderr=subprocess.STDOUT,env=environment,start_new_session=True)
        def direct_request(_):
            connection=http.client.HTTPConnection('127.0.0.1',19829,timeout=ctx.remaining())
            try:
                start=time.monotonic()
                connection.request('POST','/v1/chat/completions',json.dumps(dict(model='ot8-direct',messages=[dict(role='user',content='Write a short story about a lighthouse. /no_think')],max_tokens=256,temperature=0)),{'Content-Type':'application/json'})
                response=connection.getresponse()
                body=json.loads(response.read())
                ctx.assertion(response.status==200,'direct SGLang chat failed')
                count=body.get('usage',{}).get('completion_tokens',0)
                ctx.assertion(count>0,'direct SGLang usage missing')
                return dict(seconds=time.monotonic()-start,output_tokens=count)
            finally:
                connection.close()
        try:
            while True:
                ctx.assertion(process.poll() is None,'direct SGLang exited before ready')
                ctx.assertion(ctx.remaining()>1,'direct SGLang readiness deadline')
                connection=http.client.HTTPConnection('127.0.0.1',19829,timeout=min(2,ctx.remaining()))
                try:
                    connection.request('GET','/health')
                    response=connection.getresponse()
                    response.read()
                    if response.status==200:
                        break
                except OSError:
                    pass
                finally:
                    connection.close()
                time.sleep(1)
            for index in range(10):
                direct_request(index)
            start=time.monotonic()
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                records=list(pool.map(direct_request,range(200)))
            elapsed=time.monotonic()-start
            result[level]=dict(requests=records,elapsed_s=elapsed,tokens_per_s=sum(r['output_tokens'] for r in records)/elapsed)
        finally:
            # All members belong to this explicitly created direct-launch session.
            import signal
            os.killpg(process.pid,signal.SIGTERM)
            try:
                process.wait(timeout=min(30,ctx.remaining()))
            except subprocess.TimeoutExpired:
                os.killpg(process.pid,signal.SIGKILL)
                process.wait(timeout=5)
    for arm in result.values():
        arm['throughput_ratio_to_off']=arm['tokens_per_s']/result['off']['tokens_per_s']
    return result


def tr3(ctx,evidence,engine='TENSORRT_LLM_121'):
    label='tr3-'+engine.lower().replace('_','-')
    cache=ctx.root/(label+'-kernel-cache')
    cache.mkdir(mode=0o700,exist_ok=False)
    environment={key:str(cache/key.lower()) for key in
                 ('XDG_CACHE_HOME','TORCHINDUCTOR_CACHE_DIR','TRITON_CACHE_DIR','FLASHINFER_WORKSPACE_BASE')}
    for path in environment.values():
        Path(path).mkdir(mode=0o700)
    with isolated_role(ctx,label+'-isolated',environment=environment):
        name=label+'-engine'
        starts=[]
        evidence['starts']=starts
        for index in range(4):
            before=ctx.sample()
            start=time.monotonic()
            with sampling(ctx) as samples:
                if index==0:
                    ctx.deploy(name,engine,extra={'engine_config':{'memory':{'kv_cache':'4GiB'}}})
                else:
                    ctx.lifecycle('start',name)
            validate_samples(ctx,samples)
            logs=engine_logs(ctx,name)
            weights=number(logs,('weights_bytes','model_weights_bytes','loaded_weights_bytes'))
            pool=number(logs,('max_tokens','max_num_tokens'))
            block=number(logs,('tokens_per_block','block_size'))
            kv=number(logs,('kv_cache_bytes','kv_cache_size_bytes'))
            requested=number(logs,('requested_max_tokens','rendered_max_tokens'))
            ctx.assertion(pool==math.ceil(requested/block)*block,'built TensorRT pool differs from block-rounded rendered max_tokens')
            # Prove the cold launch uses only the newly-created session cache.
            if index==0:
                proved=False
                for identity in ctx.owned(name):
                    path=Path('/proc')/str(identity['pid'])/'environ'
                    if path.exists():
                        selected={item.split('=',1)[0]:item.split('=',1)[1] for item in path.read_bytes().decode(errors='replace').split('\0') if '=' in item and item.split('=',1)[0] in environment}
                        proved=proved or all(selected.get(key)==value for key,value in environment.items())
                ctx.assertion(proved,'cold kernel-cache isolation not preserved in owned engine environment')
            peak=max(sum(sample['owned_gpu_bytes_by_pid'].values()) for sample in samples)
            ctx.assertion(peak>0,'per-process startup GPU peak unavailable')
            starts.append(dict(kind='cold' if index==0 else 'warm',ready_s=time.monotonic()-start,memory=samples,weights_bytes=weights,kv_bytes=kv,built_pool_tokens=pool,rendered_pool_tokens=requested,block_tokens=block,peak_increment_bytes=peak,startup_margin_bytes=peak-weights-kv))
            if index<3:
                stopped(ctx,name)
        traffic=[]
        evidence['mixed_traffic']=traffic
        until=time.monotonic()+1800
        ctx.assertion(ctx.remaining()>1800+180,'insufficient row budget for required 30-minute mixed traffic and allocation repeat')
        while time.monotonic()<until:
            for clients in (1,4,8):
                batch=ctx.bench(name,concurrency=clients,tokens=(32 if len(traffic)%2 else 256),rounds=1)
                ctx.assertion(batch['ok']==batch['requests'],'mixed traffic request failed')
                traffic.append({key:value for key,value in batch.items() if key!='results'})
                if time.monotonic()>=until:
                    break
        evidence['steady']=dict(**ctx.sample(),**process_memory(ctx))
        stopped(ctx,name)
        installation=Path(ctx.engines[engine])
        python=installation/'bin/python' if installation.is_dir() else installation.parent/'python'
        marker=ctx.root/(label+'-allocation-ready')
        code='import pathlib,time,torch; x=torch.empty(20*1024**3,dtype=torch.uint8,device=\"cuda\"); x.fill_(1); torch.cuda.synchronize(); pathlib.Path('+repr(str(marker))+').write_text(str(x.numel())); time.sleep(3600)'
        allocation=ctx.spawn([python,'-c',code],'tr3-allocation')
        try:
            while not marker.exists():
                ctx.assertion(allocation.poll() is None,'bounded 20GiB allocation failed')
                ctx.assertion(ctx.remaining()>1,'allocation readiness deadline')
                time.sleep(.2)
            ctx.assertion(int(marker.read_text())==20*1024**3,'allocation did not hold exactly 20GiB')
            with sampling(ctx) as samples:
                start=time.monotonic()
                ctx.lifecycle('start',name)
            logs=engine_logs(ctx,name)
            pool=number(logs,('max_tokens','max_num_tokens'))
            ctx.assertion(pool==starts[-1]['built_pool_tokens'],'resident 20GiB allocation changed TensorRT KV pool')
            evidence['beside_20gib']=dict(ready_s=time.monotonic()-start,built_pool_tokens=pool,memory=samples)
            answered(ctx,ctx.request(name))
            stopped(ctx,name)
        finally:
            allocation.terminate()
            allocation.wait(timeout=min(15,ctx.remaining()))


def engine_logs(ctx,name):
    texts=[ctx.logs(name)]
    info=ctx.roles.get('standalone',{})
    state=info.get('state')
    if state:
        texts += [path.read_text(errors='replace') for path in Path(state).rglob('*.log')]
    return ctx.clean('\n'.join(texts))


def process_memory(ctx):
    """Only owned PIDs enter evidence; never persist the system-wide process list."""
    import subprocess
    pids=set()
    for role,name in list(ctx.deployments):
        pids.update(int(identity['pid']) for identity in ctx.owned(name,role=role))
    rss={}
    for pid in pids:
        path=Path('/proc')/str(pid)/'status'
        try:
            match=re.search(r'VmRSS:\s+(\d+)',path.read_text())
            if match:
                rss[str(pid)]=int(match[1])*1024
        except FileNotFoundError:
            pass
    gpu={}
    if pids:
        result=subprocess.run(['nvidia-smi','--query-compute-apps=pid,used_gpu_memory','--format=csv,noheader,nounits'],
                              capture_output=True,text=True,timeout=min(5,ctx.remaining()),check=True)
        for line in result.stdout.splitlines():
            fields=line.split(',')
            if len(fields)==2 and fields[0].strip().isdigit() and int(fields[0]) in pids:
                ctx.assertion(fields[1].strip().isdigit(),'per-process GPU memory is unavailable')
                gpu[fields[0].strip()]=int(fields[1])*1024**2
    return dict(owned_gpu_bytes_by_pid=gpu,owned_rss_bytes_by_pid=rss)


def trt_preflight(ctx):
    """A real tiny CUDA kernel distinguishes driver incompatibility from recipe errors."""
    import subprocess
    availability=ctx.evidence/'TENSORRT_LLM_140RC3.availability.json'
    if availability.is_file():
        record=json.loads(availability.read_text())
        if record.get('available') is False:
            ctx.assertion(str(record.get('reason','')).startswith('driver_unsupported:'),'rc3 unavailable for a non-driver reason')
            return dict(verdict='skip',reason='driver does not support cu132',evidence={'diagnostic':ctx.clean(record['reason'])})
        ctx.assertion(record.get('available') is True,'invalid rc3 preflight availability evidence')
        ctx.assertion(ctx.engines.get('TENSORRT_LLM_140RC3'),'successful rc3 preflight has no installation mapping')
        return None
    value=ctx.engines.get('TENSORRT_LLM_140RC3')
    ctx.assertion(value,'missing TensorRT-LLM rc3 installation mapping')
    installation=Path(value)
    python=installation/'bin/python' if installation.is_dir() else installation.parent/'python'
    result=subprocess.run([str(python),'-c','import torch; x=torch.ones(1,device=\"cuda\"); y=x+1; torch.cuda.synchronize(); assert y.item()==2; print(\"cuda_kernel_ok\")'],
                          capture_output=True,text=True,timeout=min(60,ctx.remaining()))
    if result.returncode==0:
        ctx.assertion('cuda_kernel_ok' in result.stdout,'CUDA preflight produced no successful kernel marker')
        return None
    diagnostic=ctx.clean((result.stdout+'\n'+result.stderr)[-3000:])
    if re.search(r'(?i)(driver.{0,80}(too old|insufficient|incompatible|not support|mismatch)|unsupported display driver|forward compatibility|error 80[34])',diagnostic):
        return dict(verdict='skip',reason='driver does not support cu132',evidence={'diagnostic':diagnostic})
    raise AssertionError('TensorRT rc3 CUDA kernel preflight failed: '+diagnostic)
