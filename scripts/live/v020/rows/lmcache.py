"""ADR 0034 LC13–LC16: real private-pool parking, not readiness proxies."""
import http.client
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import socket
import statistics
import threading
import time


ROWS = [
    dict(id="LC13", priority=1, budget=2300, discrete=True,
         criteria="ADR0034 §2: 32k park/wake correctness, whole-chunk hits, salts, store race, footprint and wake bounds; uncertified required versus auto."),
    dict(id="LC14", priority=1, budget=2500, discrete=True,
         criteria="ADR0034 §2: 15 alternating-prompt park/wake cycles, off-reference answers, no held KV or server stops, covered charges and <=256MiB server GPU growth."),
    dict(id="LC15", priority=1, budget=1500, discrete=True,
         criteria="ADR0034 §2: enrolled host through the control server, 32k park/wake/hit and passing wake canary."),
    dict(id="LC16", priority=1, budget=2100, discrete=True, measurement=True,
         criteria="ADR0034 §2: five cold/engine-evicted/post-park trials at 8k/16k/32k; wake <=1.1x off, cold TTFT <=1.1x off, 32k post-wake TTFT <=0.25x off.")]
for row in ROWS:
    row["criteria"] += " Full protocol on source-recognized vLLM 0.34.0rc1 and SGLang 0.5.23 with LMCache 0.5.7; vLLM 0.33 retains required-refusal/auto-off guard only (ADR0034 amendment)."
ROWS += [dict(row, id=row["id"] + "U", priority=4, discrete=False, unified=True,
              measurement=row["id"] == "LC16",
              criteria=row["criteria"].replace("host_backed", "deep") + " GB10 deep-parking counterpart.")
         for row in ROWS[:]]
GIB = 1 << 30
MIB = 1 << 20
CHUNK = 256
SIZES = (8000, 16000, 32000)

# This executes only the pinned environment's offline tokenizer, never an engine.
# The nonce precedes the first chunk, so unrelated fixtures cannot share a prefix.
TOKENIZER = r'''
import hashlib, importlib.metadata, json, sys
from transformers import AutoTokenizer
model, specifications, engine = sys.argv[1], json.loads(sys.argv[2]), sys.argv[3]
t = AutoTokenizer.from_pretrained(model, local_files_only=True, trust_remote_code=False)
fixtures = {}
for label, target in specifications:
    code = str(int(hashlib.sha256(label.encode()).hexdigest()[:8], 16) % 900000 + 100000)
    prefix = "Document " + label + ". Remember the access code " + code + ". Begin irrelevant document: "
    suffix = "\nEnd irrelevant document. Return the access code from the beginning, only its six digits. /no_think"
    filler = t.encode(" amber birch cedar delta elm frost grove hazel" * (target // 4 + 100), add_special_tokens=False)
    count = target
    for _ in range(12):
        content = prefix + t.decode(filler[:count], skip_special_tokens=True) + suffix
        messages = [{"role": "user", "content": content}]
        tokens = t.apply_chat_template(messages, tokenize=True, add_generation_prompt=True, return_dict=False)
        difference = target + 64 - len(tokens)
        if 0 <= len(tokens) - target < 256:
            break
        count += difference
    else:
        raise RuntimeError("could not construct chunk-aligned chat fixture")
    fixtures[label] = {"messages": messages, "prompt_tokens": len(tokens),
                       "whole_chunk_tokens": len(tokens) // 256 * 256,
                       "sha256": hashlib.sha256(content.encode()).hexdigest()}
versions = {p: importlib.metadata.version(p) for p in (engine, "lmcache", "transformers")}
print(json.dumps({"fixtures": fixtures, "versions": versions}))
'''


def fixtures(ctx, specs, engine_key="LMCACHE"):
    package = "sglang" if engine_key == "LMCACHE_SGLANG" else "vllm"
    engine = Path(ctx.engines[engine_key])
    python = engine / "bin/python" if engine.is_dir() else engine.parent / "python"
    model = Path(ctx.engines["MODEL_CHAT"])
    result = ctx.command([python, "-c", TOKENIZER, model, json.dumps(specs), package],
                         env={**os.environ, "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
                              "TOKENIZERS_PARALLELISM": "false"})
    data = json.loads(result.stdout)
    # T45 / ADR0034: these pins identify the measured recipe. The launch-time
    # source-shape gate, exercised by sleep_guard, determines sleep safety.
    approved = {
        "LMCACHE": {("0.33.0", "0.5.7")},
        "LMCACHE_VLLM_034": {("0.34.0rc1", "0.5.7")},
        "LMCACHE_SGLANG": {("0.5.23", "0.5.7")},
    }[engine_key]
    if ctx.unified and engine_key == "LMCACHE":
        approved.add(("0.32.0", "0.5.6"))
    actual = (data["versions"][package], data["versions"]["lmcache"])
    ctx.assertion(actual in approved,
                  "qualification engine/LMCache tuple does not match an approved installation")
    return data


def is_cache(identity):
    return identity.get("role", "") == "lmcache" or identity.get("role", "").startswith("lmcache-")


def live_identities(ctx, name, role):
    # Compiler helpers may legitimately exit; they are not engine processes
    # (completion.rs ProcessIdentity::is_engine). Never inspect a reused PID.
    identities = [item for item in ctx.owned(name, role) if not item.get("role", "").startswith("helper-")]
    boot = Path("/proc/sys/kernel/random/boot_id").read_text().strip()
    for identity in identities:
        stat = (Path("/proc") / str(identity["pid"]) / "stat").read_text()
        fields = stat[stat.rfind(")") + 2:].split()
        ctx.assertion(identity["boot_id"] == boot and int(fields[19]) == identity["start_ticks"],
                      "owned identity no longer identifies the same kernel process")
    ctx.assertion(identities, "no retained launch ownership")
    return identities


def cache_ledger(ctx, name, role):
    rows = ctx.sql("""SELECT l.binding_id,l.planned,l.planned_reason,l.state,l.reason,l.rpc_port,l.http_port,
               l.pool_bytes,l.overhead_bytes,l.device_bytes,l.held_kv_bytes,l.held_kv_state,
               l.observed_device_bytes,l.measured,l.uncertified,
               (SELECT footprint_json FROM resource_owners WHERE owner_id=l.deployment_id) AS footprint
          FROM launch_lmcache l JOIN runtime_bindings b ON b.id=l.binding_id
          JOIN deployments d ON d.id=l.deployment_id WHERE d.name=? AND b.state='live'""", (name,), role)
    ctx.assertion(len(rows) == 1, "expected one live LMCache launch ledger row")
    return rows[0]


def listening_sockets(ctx, identities):
    """Read only socket inodes belonging to proven owned processes; never argv/env."""
    inodes = set()
    for identity in identities:
        directory = Path("/proc") / str(identity["pid"]) / "fd"
        for fd in directory.iterdir():
            try:
                match = re.fullmatch(r"socket:\[(\d+)\]", os.readlink(fd))
                if match:
                    inodes.add(match[1])
            except FileNotFoundError:
                continue
    listeners = []
    for family, filename in ((socket.AF_INET, "/proc/net/tcp"), (socket.AF_INET6, "/proc/net/tcp6")):
        for line in Path(filename).read_text().splitlines()[1:]:
            fields = line.split()
            if fields[3] != "0A" or fields[9] not in inodes:
                continue
            address, port = fields[1].split(":")
            raw = bytes.fromhex(address)
            raw = raw[::-1] if family == socket.AF_INET else b"".join(raw[i:i+4][::-1] for i in range(0, 16, 4))
            host = socket.inet_ntop(family, raw)
            ctx.assertion(ipaddress.ip_address(host).is_loopback, "LMCache owned listener is not loopback")
            listeners.append(dict(host=host, port=int(port, 16)))
    return listeners


def cache_http(ctx, name, role, path, method="GET", body=None):
    ledger = cache_ledger(ctx, name, role)
    identities = [item for item in live_identities(ctx, name, role) if is_cache(item)]
    ctx.assertion(identities and ledger["state"] == "on", "private LMCache server is not on")
    listeners = listening_sockets(ctx, identities)
    ctx.assertion(any(item["port"] == ledger["http_port"] for item in listeners),
                  "LMCache HTTP port has no owned loopback listener")
    connection = http.client.HTTPConnection("127.0.0.1", ledger["http_port"], timeout=min(10, ctx.remaining()))
    try:
        connection.request(method, path, None if body is None else json.dumps(body),
                           {"Content-Type": "application/json"})
        response = connection.getresponse()
        raw = response.read(4 * 1024 * 1024 + 1)
        ctx.assertion(len(raw) <= 4 * 1024 * 1024, "LMCache HTTP evidence exceeded bound")
        ctx.assertion(response.status == 200, "owned LMCache " + path + " failed: " + str(response.status))
        return raw.decode()
    finally:
        connection.close()


def metrics(ctx, name, role):
    values = {}
    for line in cache_http(ctx, name, role, "/metrics").splitlines():
        match = re.match(r"([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{[^\n]*\})?\s+([-+0-9.eE]+)(?:\s|$)", line)
        if not match:
            continue
        key, value = match.groups()
        # Memory/time series are deliberately not sampled in functional-only mode.
        if re.search(r"(?:lookup_hit_tokens|lookup_requested_tokens|num_chunks_loaded|lookup_hits|hit_tokens|lookups_requests|lookups_total|retrieve_requests|stored_tokens)(?:_total)?$", key):
            number = float(value)
            if math.isfinite(number):
                values[key] = values.get(key, 0) + number
    return values


def counter(values, kind):
    patterns = {
        "hit": (r"lookup_hit_tokens(?:_total)?$", r"lookup_hits(?:_total)?$", r"hit_tokens(?:_total)?$"),
        "requested": (r"lookup_requested_tokens(?:_total)?$", r"lookup_tokens(?:_total)?$"),
        "loaded": (r"num_chunks_loaded(?:_total)?$",),
    }
    for pattern in patterns[kind]:
        matching = [value for key, value in values.items() if re.search(pattern, key)]
        if matching:
            return sum(matching)
    return None


def answer(response):
    body = response.get("body")
    documents = [json.loads(chunk) for chunk in response.get("chunks", []) if chunk != "[DONE]"]
    if not documents and isinstance(body, dict):
        documents = [body]
    content, usage = [], {}
    for doc in documents:
        if doc.get("usage"):
            usage = doc["usage"]
        for choice in doc.get("choices", []):
            message = choice.get("delta", choice.get("message", {}))
            if message.get("content"):
                content.append(message["content"])
    return "".join(content), usage


class Protocol:
    def __init__(self, ctx, row, role="standalone", engine="LMCACHE"):
        self.ctx, self.row, self.role, self.engine = ctx, row, role, engine
        self.sglang = engine == "LMCACHE_SGLANG"
        # SPEC §17: CapyCTL always renders SGLang's reserved cache-report flag.
        self.engine_config = ({} if self.sglang else
                              dict(accept_extra_args=True, extra_args=["--enable-prompt-tokens-details"]))
        self.residency = "deep" if ctx.unified else "host_backed"
        self.records, self.failures, self.names = [], [], []
        self.prefix = row["id"].lower() + "-" + engine.lower().replace("_", "-")
        self.first_server_gpu = None
        self.server_identity = None
        self.journal_start = 0
        self.dependency_override = None

    def record(self, kind, **fields):
        value = dict(kind=kind, engine=self.engine, **fields)
        if self.dependency_override is not None:
            value["dependency_override"] = self.dependency_override
        self.records.append(value)
        self.ctx.save(self.engine.lower() + "-record-" + str(len(self.records)).zfill(5), value)
        return value

    def check(self, condition, reason):
        if not condition:
            self.failures.append(reason)
        return bool(condition)

    def deploy(self, suffix, mode, pool=None):
        name = self.prefix + "-" + suffix
        extra = dict(lmcache=mode, engine_config=self.engine_config)
        if pool:
            extra["lmcache_pool"] = pool
        try:
            self.ctx.deploy(name, self.engine, residency=self.residency, extra=extra, role=self.role)
        except Exception:
            if (self.role, name) in self.ctx.deployments:
                self.names.append(name)
            raise
        self.names.append(name)
        status = self.ctx.status(name, self.role)
        ledger = cache_ledger(self.ctx, name, self.role)
        self.record("deployment", name=name, mode=mode, residency=self.residency,
                    status=status, ledger={key: ledger[key] for key in ("planned", "planned_reason", "state", "reason", "uncertified")})
        if mode == "required":
            self.ctx.assertion(ledger["planned"] == "on" and ledger["state"] == "on", "required connector did not start")
            self.server_identity = None
            self.first_server_gpu = None
            self.journal_start = self.ctx.sql("SELECT COALESCE(MAX(rowid),0) AS n FROM journal_entries", role=self.role)[0]["n"]
            self.rendering(name)
        else:
            self.ctx.assertion(ledger["planned"] == "off", "off/uncertified auto launch unexpectedly enabled LMCache")
        return name

    def retire(self, name):
        self.ctx.cli("delete", "deployment", name, "--stop", role=self.role)
        if (self.role, name) in self.ctx.deployments:
            self.ctx.deployments.remove((self.role, name))
        self.names.remove(name)
        retained = self.ctx.sql("SELECT b.id FROM runtime_bindings b JOIN deployments d ON d.id=b.deployment_id WHERE d.name=? AND b.state!='released'", (name,), self.role)
        self.ctx.assertion(not retained, "deleted qualification deployment retains a binding")

    def rendering(self, name):
        identities = live_identities(self.ctx, name, self.role)
        server = [item for item in identities if is_cache(item)]
        self.ctx.assertion(server, "no owned cache server")
        allowed = {"supported-transfer-mode", "shm-name", "eviction-trigger-watermark", "eviction-ratio"}
        flags = {}
        # Export only the four qualification settings, never raw command lines.
        for identity in server:
            args = (Path("/proc") / str(identity["pid"]) / "cmdline").read_bytes().decode().split("\0")
            for index, arg in enumerate(args):
                key, _, value = arg.removeprefix("--").partition("=")
                if arg.startswith("--") and key in allowed:
                    flags[key] = value or (args[index + 1] if index + 1 < len(args) else None)
        listeners = listening_sockets(self.ctx, server)
        ledger = cache_ledger(self.ctx, name, self.role)
        self.check(flags.get("supported-transfer-mode") == "engine_driven", "server transfer mode is not engine_driven")
        self.check("shm-name" not in flags, "default transfer unexpectedly uses shared memory")
        self.check(flags.get("eviction-trigger-watermark") == "0.8" and flags.get("eviction-ratio") == "0.2", "eviction policy differs from pool-sizing policy")
        self.check({ledger["rpc_port"], ledger["http_port"]}.issubset({item["port"] for item in listeners}), "both server listeners were not proven owned")
        connector = None
        for identity in identities:
            if is_cache(identity):
                continue
            args = (Path("/proc") / str(identity["pid"]) / "cmdline").read_bytes().decode().split("\0")
            for index, arg in enumerate(args):
                key, _, value = arg.removeprefix("--").partition("=")
                if arg.startswith("--") and key == "kv-transfer-config":
                    configured = json.loads(value or args[index + 1])
                    extra = configured.get("kv_connector_extra_config", {})
                    connector = {key: configured.get(key) for key in ("kv_connector", "kv_connector_module_path", "kv_role")}
                    connector["extra"] = {key: extra.get(key) for key in
                                          ("lmcache.mp.host", "lmcache.mp.port", "lmcache.mp.autostart", "lmcache.mp.mp_transfer_mode")}
                elif self.sglang and arg == "--public-settings-json":
                    settings = json.loads(args[index + 1]).get("settings", {})
                    config_path = settings.get("lmcache_config_file")
                    if config_path:
                        # ADR0033 §4: inspect only the generated, secret-free
                        # connector file; never export the public argv wholesale.
                        text = Path(config_path).read_text()
                        expected = ("mp_host: 127.0.0.1\nmp_port: " + str(ledger["rpc_port"]) +
                                    "\nextra_config:\n  lmcache.mp.mp_transfer_mode: engine_driven\n")
                        connector = dict(kind="sglang", config_matches=text == expected)
        if self.sglang:
            self.check(connector is not None and connector.get("config_matches"),
                       "SGLang connector does not match owned engine-driven server")
        else:
            self.check(connector is not None and connector["kv_connector"] == "LMCacheMPConnector" and
                       connector["kv_connector_module_path"] == "lmcache.integration.vllm.lmcache_mp_connector" and
                       connector["kv_role"] == "kv_both" and
                       connector["extra"]["lmcache.mp.mp_transfer_mode"] == "engine_driven" and
                       connector["extra"]["lmcache.mp.host"] == "127.0.0.1" and
                       connector["extra"]["lmcache.mp.port"] == ledger["rpc_port"] and
                       connector["extra"]["lmcache.mp.autostart"] is False,
                       "engine connector does not match owned engine-driven server")
        # /status includes memory internals; /healthcheck proves readiness
        # without accidentally sampling memory in functional-only mode.
        health = cache_http(self.ctx, name, self.role, "/healthcheck")
        self.record("rendering", flags=flags, connector=connector, listeners=listeners, health=json.loads(health),
                    certified=not bool(ledger["uncertified"]), qualification_mode="required; no qualification-table mutation")

    def lifecycle(self, action, name):
        result = self.ctx.lifecycle(action, name, self.role)
        entry = dict(action=action, name=name, status=result["status"])
        if not self.ctx.functional_only:
            entry["seconds"] = result["seconds"]
        self.record("lifecycle", **entry)
        return entry

    def memory(self, name, label, cache=False):
        if self.ctx.functional_only:
            return None
        identities = live_identities(self.ctx, name, self.role)
        raw = self.ctx.command(["nvidia-smi", "--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"]).stdout
        gpu = {}
        for line in raw.splitlines():
            fields = [field.strip() for field in line.split(",")]
            if len(fields) == 2 and fields[0].isdigit() and fields[1].isdigit():
                gpu[int(fields[0])] = gpu.get(int(fields[0]), 0) + int(fields[1]) * MIB
        rss, device = {"engine": 0, "server": 0}, {"engine": 0, "server": 0}
        for identity in identities:
            group = "server" if is_cache(identity) else "engine"
            text = (Path("/proc") / str(identity["pid"]) / "status").read_text()
            match = re.search(r"^VmRSS:\s+(\d+)", text, re.M)
            rss[group] += int(match[1]) * 1024 if match else 0
            device[group] += gpu.get(identity["pid"], 0)
        result = dict(label=label, **self.ctx.sample(), engine_rss_bytes=rss["engine"],
                      server_rss_bytes=rss["server"], engine_gpu_bytes=device["engine"], server_gpu_bytes=device["server"])
        if cache:
            ledger = cache_ledger(self.ctx, name, self.role)
            footprint = json.loads(ledger.pop("footprint"))
            result.update(charge=ledger, footprint=footprint)
            self.check(ledger["held_kv_bytes"] == 0 and ledger["held_kv_state"] is None, "held engine KV was charged")
            self.check(device["server"] <= max(ledger["device_bytes"], ledger["observed_device_bytes"] or 0), "server GPU reading exceeds its charge")
            self.check(rss["server"] <= ledger["pool_bytes"] + ledger["overhead_bytes"], "server RSS reading exceeds pool plus overhead charge")
            allocations = footprint.get("allocations", [])
            self.check(sum(item[2] for item in allocations) >= ledger["pool_bytes"], "resource ledger does not hold the pool's host-KV charge")
            self.check(sum(item[1] for item in allocations) >= ledger["pool_bytes"] + ledger["overhead_bytes"] + device["server"], "resource ledger does not cover server memory")
            if self.first_server_gpu is None:
                self.first_server_gpu = device["server"]
            self.check(device["server"] <= self.first_server_gpu + 256 * MIB, "server GPU grew more than 256 MiB above first-start reading")
        return self.record("memory", **result)

    def request(self, name, fixture, label, salt=None, cache=False, expected=None, hit=None, transfer_memory=False):
        before = metrics(self.ctx, name, self.role) if cache else {}
        samples = []
        stop = threading.Event()
        errors = []
        def sample_transfer():
            while not stop.is_set():
                try:
                    samples.append(self.memory(name, label + "-transfer", cache=cache))
                except Exception as error:
                    errors.append(type(error).__name__ + ": " + str(error))
                    return
                stop.wait(0.25)
        sampler = None
        if transfer_memory and not self.ctx.functional_only:
            sampler = threading.Thread(target=sample_transfer, daemon=True)
            sampler.start()
        payload = dict(messages=fixture["messages"], temperature=0, max_tokens=32,
                       stream_options=dict(include_usage=True))
        if salt is not None:
            payload["cache_salt"] = salt
        try:
            response = self.ctx.request(name, body=payload, stream=True, role=self.role)
        finally:
            stop.set()
            if sampler:
                sampler.join(timeout=min(20, self.ctx.remaining()))
                self.ctx.assertion(not sampler.is_alive(), "transfer sampler failed to stop")
        self.ctx.assertion(response["status"] == 200 and response.get("done"), "32k qualification request failed or stream did not finish")
        content, usage = answer(response)
        self.ctx.assertion(content, "qualification request produced no answer")
        actual_prompt = usage.get("prompt_tokens")
        self.check(isinstance(actual_prompt, int) and
                   fixture["whole_chunk_tokens"] <= actual_prompt < fixture["whole_chunk_tokens"] + CHUNK,
                   label + ": engine token count does not match the offline fixture")
        if expected is not None:
            self.check(content == expected, label + ": deterministic answer differs from LMCache-off")
        after = before
        if cache:
            # Wait only for asynchronous metric publication, not for stores: the
            # store-race protocol below intentionally parks before any scrape.
            until = min(self.ctx.deadline, time.monotonic() + 8)
            while True:
                after = metrics(self.ctx, name, self.role)
                requested = (counter(after, "requested") or 0) - (counter(before, "requested") or 0)
                retrieved = (counter(after, "loaded") or 0) - (counter(before, "loaded") or 0)
                complete = requested > 0 and (not hit or retrieved * CHUNK >= fixture["whole_chunk_tokens"])
                if complete or time.monotonic() >= until:
                    break
                time.sleep(0.1)
            self.check(counter(after, "hit") is not None, "LMCache emitted no token hit counter")
            self.check(counter(after, "requested") is not None and requested > 0,
                       "LMCache emitted no requested-token increment")
        delta = {key: value - before.get(key, 0) for key, value in after.items()}
        hits = (counter(after, "hit") or 0) - (counter(before, "hit") or 0)
        loaded = (counter(after, "loaded") or 0) - (counter(before, "loaded") or 0)
        cached = usage.get("prompt_tokens_details", {}).get("cached_tokens")
        if cache and hit is not None:
            self.check(hits >= fixture["whole_chunk_tokens"] if hit else hits == 0,
                       label + (": missing whole-chunk external hit" if hit else ": crossed a salt/cold isolation boundary"))
            if hit:
                self.check(counter(after, "loaded") is not None and
                           loaded * CHUNK >= fixture["whole_chunk_tokens"],
                           label + ": lookup hit was not retrieved")
        entry = dict(label=label, prompt_sha256=fixture["sha256"], expected_chunk_tokens=fixture["whole_chunk_tokens"],
                     answer=content, off_answer=expected, usage=usage, salt=salt,
                     external_hit_tokens=hits, loaded_chunks=loaded, counters_before=before,
                     counters_after=after, counter_delta=delta, cached_tokens=cached,
                     transfer_samples=len(samples), sampler_errors=errors)
        if not self.ctx.functional_only:
            entry.update(ttft_s=response.get("ttft_s"), seconds=response.get("seconds"))
        self.check(not errors, "transfer memory sampling failed")
        self.record("request", **entry)
        return entry

    def footprint(self, off, on):
        if self.ctx.functional_only:
            return
        charge = on["charge"]
        extra_ram = off["mem_available_bytes"] - on["mem_available_bytes"]
        ram_bound = charge["pool_bytes"] + charge["overhead_bytes"] + (on["server_gpu_bytes"] if self.ctx.unified else 0) + GIB
        gpu_bound = off["gpu_used_bytes"] + on["server_gpu_bytes"] + GIB
        self.record("park-footprint-comparison", extra_system_bytes=extra_ram,
                    allowed_extra_system_bytes=ram_bound, observed_gpu_bytes=on["gpu_used_bytes"],
                    allowed_gpu_bytes=gpu_bound)
        self.check(extra_ram <= ram_bound, "parked system footprint exceeds off plus pool/server and 1 GiB")
        if not self.ctx.unified:
            self.check(on["gpu_used_bytes"] <= gpu_bound, "parked device footprint retains engine KV beyond 1 GiB allowance")

    def server_check(self, name):
        server = sorted((item["pid"], item["boot_id"], item["start_ticks"]) for item in live_identities(self.ctx, name, self.role) if is_cache(item))
        if self.server_identity is None:
            self.server_identity = server
        self.check(bool(server) and server == self.server_identity, "LMCache server restarted or lost ownership across parking")
        health = json.loads(cache_http(self.ctx, name, self.role, "/healthcheck"))
        entries = self.ctx.sql("""SELECT j.state,j.evidence FROM journal_entries j JOIN operations o ON o.id=j.operation_id
                     JOIN deployments d ON d.id=o.deployment_id WHERE d.name=? AND j.rowid>?
                     AND j.state IN ('wake_mismatch','wake_uncertain','lmcache_server_stopped','lmcache_charge_raised')""",
                     (name, self.journal_start), self.role)
        events = self.ctx.sql("""SELECT kind,payload_json FROM management_events WHERE deployment_id=(SELECT id FROM deployments WHERE name=?)
                      AND kind IN ('wake_mismatch','wake_uncertain','lmcache_server_stopped','lmcache_server_exited')""", (name,), self.role)
        failures = [entry for entry in entries if entry["state"] != "lmcache_charge_raised"]
        raises = [entry for entry in entries if entry["state"] == "lmcache_charge_raised"]
        measurements = []
        if raises and not self.ctx.functional_only:
            measurements = self.ctx.sql("""SELECT m.device_bytes,m.overhead_bytes FROM lmcache_measurements m
                         JOIN launch_lmcache l ON l.binding_id=m.binding_id
                         JOIN deployments d ON d.id=l.deployment_id WHERE d.name=?""", (name,), self.role)
            ledger = cache_ledger(self.ctx, name, self.role)
            self.check(bool(measurements) and max(item["device_bytes"] for item in measurements) >=
                       (ledger["observed_device_bytes"] or 0), "LMCache charge raised beyond a recorded measurement")
        self.check(not failures and not events, "wake/server failure recorded during cache protocol")
        self.record("server-continuity", health=health, failures=failures, events=events,
                    charge_raise_events=raises, supporting_measurements=measurements)

    def finish(self, guard=False):
        for name in self.names[:]:
            self.retire(name)
        measurement = "not qualified: timing and memory subparts intentionally not sampled" if self.ctx.functional_only else "numeric samples and comparisons recorded"
        if guard:
            measurement = "clearing-source required-refusal/auto-off guard only; not sleep qualification"
        self.record("qualification-scope", measurements=measurement, model="chat", residency=self.residency,
                    external_counter_source="owned LMCache /metrics; engine keys never read",
                    table="required uncertified rendering; no claim that auto is certified")
        reason = ("clearing-source guard passed; " if guard else "correctness/hits passed; ") + measurement
        return dict(verdict="fail" if self.failures else "skip" if self.ctx.functional_only else "pass",
                    reason="; ".join(dict.fromkeys(self.failures)) if self.failures else reason,
                    evidence=self.records)


def correctness(protocol, fixture_data, repeats=5, enrolled=False):
    p, ctx = protocol, protocol.ctx
    data = fixture_data["fixtures"]
    off = p.deploy("off", "off")
    references, off_parks, off_wakes, off_post = {}, [], [], []
    for label in ("main", "salt", "race"):
        references[label] = p.request(off, data[label], "off-" + label)["answer"]
    for index in range(repeats):
        p.lifecycle("park", off)
        off_parks.append(p.memory(off, "off-park-" + str(index)))
        off_wakes.append(p.lifecycle("wake", off))
        off_post.append(p.request(off, data["main"], "off-post-wake-" + str(index), expected=references["main"]))
    p.retire(off)
    if not enrolled:
        auto = p.deploy("auto", "auto")
        ledger = cache_ledger(ctx, auto, p.role)
        p.check((ledger["planned_reason"] or ledger["reason"]) == "park_unqualified", "unlisted auto deployment does not name park_unqualified")
        p.lifecycle("park", auto)
        p.retire(auto)
    on = p.deploy("on", "required")
    ledger = cache_ledger(ctx, on, p.role)
    p.check(bool(ledger["uncertified"]), "unlisted required deployment is not shown uncertified")
    p.memory(on, "first-start", cache=True)
    p.server_check(on)
    p.request(on, data["main"], "on-seed", cache=True, expected=references["main"], hit=False)
    p.memory(on, "seed-stored", cache=True)
    wakes, post = [], []
    for index in range(repeats):
        p.lifecycle("park", on)
        parked = p.memory(on, "on-park-" + str(index), cache=True)
        p.footprint(off_parks[index], parked)
        wakes.append(p.lifecycle("wake", on))
        post.append(p.request(on, data["main"], "on-post-wake-" + str(index), cache=True, expected=references["main"], hit=True))
        p.memory(on, "on-awake-" + str(index), cache=True)
        p.server_check(on)
    for index, salt in enumerate(("qualification-A", "qualification-A", "qualification-B", None)):
        if index:
            p.lifecycle("park", on)
            p.memory(on, "salt-park-" + str(index), cache=True)
            p.lifecycle("wake", on)
        p.request(on, data["salt"], "salt-" + str(index), salt=salt, cache=True,
                  expected=references["salt"], hit=index == 1)
        p.memory(on, "salt-awake-" + str(index), cache=True)
    # Deliberately no scrape, filesystem evidence write, sleep or memory reading
    # between the long response and arming park: its asynchronous store may race.
    response = ctx.request(on, body=dict(messages=data["race"]["messages"], temperature=0, max_tokens=32), stream=True, role=p.role)
    parked = ctx.lifecycle("park", on, p.role)
    content, usage = answer(response)
    p.check(response["status"] == 200 and response.get("done") and content == references["race"], "store-race seed differs from off")
    p.record("store-race-armed", answer=content, off_answer=references["race"], usage=usage,
             status=parked["status"], prompt_sha256=data["race"]["sha256"])
    p.memory(on, "store-race-park", cache=True)
    p.lifecycle("wake", on)
    p.request(on, data["race"], "store-race-post-wake", cache=True, expected=references["race"], hit=True)
    p.memory(on, "store-race-awake", cache=True)
    p.server_check(on)
    if not ctx.functional_only:
        off_wake = statistics.median(item["seconds"] for item in off_wakes)
        on_wake = statistics.median(item["seconds"] for item in wakes)
        off_ttft = statistics.median(item["ttft_s"] for item in off_post)
        on_ttft = statistics.median(item["ttft_s"] for item in post)
        p.record("wake-comparison", off_wake_s=off_wake, on_wake_s=on_wake,
                 wake_ratio=on_wake / off_wake, off_postwake_ttft_s=off_ttft,
                 on_postwake_ttft_s=on_ttft, postwake_ttft_ratio=on_ttft / off_ttft)
        p.check(on_wake <= 1.1 * off_wake, "wake exceeds 1.1x LMCache-off")
        p.check(on_ttft <= 0.25 * off_ttft, "32k post-wake TTFT exceeds 0.25x LMCache-off")
    if enrolled:
        # Restore commits only after the controller's reference comparison
        # (coordinator/worker.rs wake_canary_check); it emits 'restored', not
        # a fictional 'canary_passed' journal state.
        canaries = ctx.sql("""SELECT j.state,j.evidence FROM journal_entries j JOIN operations o ON o.id=j.operation_id
                 JOIN deployments d ON d.id=o.deployment_id WHERE d.name=? AND o.kind='restore'""", (on,), p.role)
        reference = ctx.sql("""SELECT w.generation,w.incarnation,w.kind,w.mismatch_operation_id
                 FROM wake_canaries w JOIN runtime_bindings b ON b.deployment_id=w.deployment_id
                   AND b.incarnation=w.incarnation AND b.state='live'
                 JOIN deployments d ON d.id=w.deployment_id WHERE d.name=?""", (on,), p.role)
        p.record("enrolled-wake-canary", journal=canaries, reference=reference)
        p.check(any(item["state"] == "restored" for item in canaries) and bool(reference) and
                all(item["mismatch_operation_id"] is None for item in reference),
                "enrolled wake lacks restored evidence and a matching launch reference")
    p.retire(on)


def endurance(p, fixture_data):
    data = fixture_data["fixtures"]
    labels = ["same" if index % 2 == 0 else "new-" + str(index) for index in range(15)]
    off = p.deploy("off", "off")
    references, baseline = {}, []
    for index, label in enumerate(labels):
        p.request(off, data[label], "off-seed-" + str(index))
        # T45 / ADR0034 LC18: restart-only without LMCache cannot park
        # (ADR0033 §7). Its answers are the reference, not a deep-park footprint.
        if p.residency != "restart_only":
            p.lifecycle("park", off)
            baseline.append(p.memory(off, "off-cycle-" + str(index)))
            p.lifecycle("wake", off)
        references[label] = p.request(off, data[label], "off-cycle-" + str(index), expected=references.get(label))["answer"]
    p.retire(off)
    on = p.deploy("on", "required")
    p.memory(on, "first-start", cache=True)
    p.server_check(on)
    for index, label in enumerate(labels):
        p.request(on, data[label], "on-seed-" + str(index), cache=True, expected=references[label])
        p.lifecycle("park", on)
        parked = p.memory(on, "on-park-" + str(index), cache=True)
        if p.residency != "restart_only":
            p.footprint(baseline[index], parked)
        p.server_check(on)
        p.lifecycle("wake", on)
        p.request(on, data[label], "on-cycle-" + str(index), cache=True, expected=references[label], hit=True)
        p.memory(on, "on-awake-" + str(index), cache=True)
        p.server_check(on)
    p.record("endurance", completed_cycles=15, alternating_prompt_labels=labels)
    p.retire(on)


def costs(p, fixture_data):
    data = fixture_data["fixtures"]
    results = {"off": {}, "on": {}}
    references = {}
    # 12 GiB keeps the 32k target plus 2*18,560 noise tokens below the
    # eviction trigger. The noise alone exceeds the engine's 5 GiB KV grant.
    # Thus 'evicted' means engine-evicted, not an accidental LMCache miss.
    p.record("cost-method", repeats=5, sizes=list(SIZES), pool="12GiB", noise_tokens=37120,
             historical_lmcache_driven=dict(source="ADR0034 Context, GB10 vLLM0.32.0, not a same-machine bound",
                 evicted_16k_ttft_s=0.18, evicted_32k_ttft_s=0.35, postwake_32k_ttft_s=0.37))
    for mode in ("off", "on"):
        name = p.deploy(mode, "off" if mode == "off" else "required", pool="12GiB")
        cache = mode == "on"
        p.memory(name, mode + "-first-start", cache=cache)
        if cache:
            p.server_check(name)
        for size in SIZES:
            trials = []
            for repeat in range(5):
                label = str(size) + "-" + str(repeat)
                target = data[label]
                if cache:
                    cache_http(p.ctx, name, p.role, "/cache/clear", "POST", dict(tier="l1", force=False))
                cold = p.request(name, target, mode + "-cold-" + label, cache=cache,
                                 expected=references.get(label), hit=False if cache else None, transfer_memory=True)
                references[label] = cold["answer"] if not cache else references[label]
                for noise in ("noise-a-", "noise-b-"):
                    p.request(name, data[noise + label], mode + "-" + noise + label, cache=cache)
                evicted = p.request(name, target, mode + "-evicted-" + label, cache=cache,
                                    expected=references[label], hit=True if cache else None, transfer_memory=True)
                # Off's request must have recomputed, and on's whole prefix must
                # have come from the private pool, not the engine prefix cache.
                if evicted["cached_tokens"] is not None and not cache:
                    p.check(evicted["cached_tokens"] < CHUNK, "noise did not evict target from engine cache")
                p.lifecycle("park", name)
                parked = p.memory(name, mode + "-park-" + label, cache=cache)
                wake = p.lifecycle("wake", name)
                post = p.request(name, target, mode + "-postwake-" + label, cache=cache,
                                 expected=references[label], hit=True if cache else None, transfer_memory=True)
                if cache:
                    p.server_check(name)
                trials.append(dict(cold=cold, evicted=evicted, wake=wake, postwake=post, parked=parked))
            results[mode][size] = trials
        p.retire(name)
    for size in SIZES:
        off, on = results["off"][size], results["on"][size]
        if p.ctx.functional_only:
            continue
        comparisons = {}
        for phase in ("cold", "evicted", "postwake"):
            baseline = statistics.median(trial[phase]["ttft_s"] for trial in off)
            measured = statistics.median(trial[phase]["ttft_s"] for trial in on)
            comparisons[phase] = dict(off_median_ttft_s=baseline, on_median_ttft_s=measured, ratio=measured / baseline,
                                      off_samples_s=[trial[phase]["ttft_s"] for trial in off],
                                      on_samples_s=[trial[phase]["ttft_s"] for trial in on])
            historical = {(16000, "evicted"): 0.18, (32000, "evicted"): 0.35,
                          (32000, "postwake"): 0.37}.get((size, phase))
            comparisons[phase]["historical_lmcache_driven_ttft_s"] = historical
            if historical is not None:
                comparisons[phase]["ratio_to_historical_lmcache_driven"] = measured / historical
        off_wake = statistics.median(trial["wake"]["seconds"] for trial in off)
        on_wake = statistics.median(trial["wake"]["seconds"] for trial in on)
        p.record("cost-comparison", prompt_tokens=size, phases=comparisons,
                 off_wake_samples_s=[trial["wake"]["seconds"] for trial in off],
                 on_wake_samples_s=[trial["wake"]["seconds"] for trial in on],
                 off_wake_s=off_wake, on_wake_s=on_wake, wake_ratio=on_wake / off_wake)
        p.check(comparisons["cold"]["ratio"] <= 1.1, str(size) + ": cold TTFT exceeds 1.1x off")
        p.check(on_wake <= off_wake * 1.1, str(size) + ": wake exceeds 1.1x off")
        if size == 32000:
            p.check(comparisons["postwake"]["ratio"] <= 0.25, "32k post-wake TTFT exceeds 0.25x off")


def sleep_guard(p, qualification=True):
    """ADR 0034 amendment: a compatible refusal never certifies cache retention."""
    ctx = p.ctx
    try:
        probe = p.deploy("required-compatibility", "required")
    except AssertionError:
        operation = ctx.evidence / (ctx.row_id + "-operation-" + str(ctx.index) + ".json")
        receipt = json.loads(operation.read_text()) if operation.is_file() else {}
        diagnostic = receipt.get("stderr", "") + receipt.get("stdout", "")
        if "sleep_clears_cache" not in diagnostic:
            raise
        p.record("required-sleep-refusal", reason="sleep_clears_cache", diagnostic=diagnostic)
        refused = p.prefix + "-required-compatibility"
        if (p.role, refused) in ctx.deployments:
            p.retire(refused)
    else:
        p.retire(probe)
        return False
    auto = p.deploy("auto-compatibility", "auto")
    ledger = cache_ledger(ctx, auto, p.role)
    p.check(ledger["planned"] == "off" and ledger["state"] == "off",
            "auto sleep parking did not disable LMCache")
    # ADR0034: unchanged qualification planning may disable auto before the
    # launch-time source gate sees it. Neither off reason qualifies sleep hits.
    p.check((ledger["planned_reason"] or ledger["reason"]) in ("sleep_clears_cache", "park_unqualified"),
            "auto clearing-source launch does not name a sleep/qualification off reason")
    park = p.lifecycle("park", auto)
    wake = p.lifecycle("wake", auto)
    response = ctx.request(auto, role=p.role)
    p.check(response.get("status") == 200, "auto LMCache-off deployment did not answer after wake")
    p.record("auto-sleep-guard", ledger=ledger, park=park, wake=wake,
             response_status=response.get("status"), external_hits_expected=False)
    p.retire(auto)
    if qualification:
        p.check(False, "sleep_clears_cache: compatibility guard observed; sleep hit/memory/endurance/cost not qualified")
    return True


def restart_cache_check(p):
    """Keep restart-only retention distinct from unsupported sleep retention."""
    ctx = p.ctx
    fixture = fixtures(ctx, [("restart", 32000)], p.engine)["fixtures"]["restart"]
    name = p.prefix + "-restart-cache"
    ctx.deploy(name, p.engine, residency="restart_only", role=p.role,
               extra=dict(lmcache="required", engine_config=p.engine_config))
    p.names.append(name)
    seed = p.request(name, fixture, "restart-seed", cache=True, hit=False)
    p.lifecycle("park", name)
    p.lifecycle("wake", name)
    p.request(name, fixture, "restart-post-wake", cache=True, hit=True, expected=seed["answer"])
    p.retire(name)


def run(ctx, row):
    row_id = row["id"].removesuffix("U")
    role = "server" if row_id == "LC15" else "standalone"
    results = []
    # T45 / ADR0034 amendment: run both preservation recipes in full, and retain
    # the old clearing-source guard separately. No version bypasses the source gate.
    for engine in ("LMCACHE", "LMCACHE_VLLM_034", "LMCACHE_SGLANG"):
        p = Protocol(ctx, row, role=role, engine=engine)
        guard_only = engine == "LMCACHE"
        try:
            if engine == "LMCACHE_VLLM_034":
                receipt = ctx.root / "evidence" / "LMCACHE_VLLM_034.dependency-override.json"
                p.dependency_override = json.loads(receipt.read_text())
                ctx.assertion(bool(p.dependency_override.get("reason")),
                              "missing test-only dependency override provenance")
                p.record("test-environment-override")
            guarded = sleep_guard(p, qualification=not guard_only)
            if guard_only:
                p.check(guarded, "vLLM 0.33 clearing-source guard unexpectedly enabled sleep LMCache")
                if guarded and row_id == "LC13":
                    restart_cache_check(p)
            elif guarded:
                p.check(False, "sleep_clears_cache: full preservation protocol not qualified")
            else:
                if row_id in ("LC13", "LC15"):
                    specs = [(label, 32000) for label in ("main", "salt", "race")]
                elif row_id == "LC14":
                    specs = [(label, 32000) for label in ["same"] + ["new-" + str(index) for index in range(1, 15, 2)]]
                elif row_id == "LC16":
                    specs = []
                    for size in SIZES:
                        for repeat in range(5):
                            label = str(size) + "-" + str(repeat)
                            specs.extend(((label, size), ("noise-a-" + label, 18560), ("noise-b-" + label, 18560)))
                else:
                    raise ValueError("unknown cache row")
                data = fixtures(ctx, specs, engine)
                artifact = engine.lower() + "-fixtures"
                ctx.save(artifact, data)
                p.record("tokenizer-fixtures", versions=data["versions"],
                         artifact=ctx.row_id + "-" + artifact + ".json",
                         fixtures={label: {key: value for key, value in fixture.items() if key != "messages"}
                                   for label, fixture in data["fixtures"].items()})
                if row_id == "LC13":
                    correctness(p, data)
                elif row_id == "LC14":
                    endurance(p, data)
                elif row_id == "LC15":
                    correctness(p, data, repeats=1, enrolled=True)
                else:
                    costs(p, data)
            results.append(p.finish(guard=guard_only))
        except Exception as error:
            p.record("protocol-error", error=type(error).__name__, reason=str(error),
                     measurements="not sampled" if ctx.functional_only else "partial numeric evidence retained")
            results.append(dict(verdict="fail", reason=engine + ": " + str(error), evidence=p.records))
            # Failed live work must release its deployments before the next engine.
            for name in p.names[:]:
                p.retire(name)
    verdict = "fail" if any(result["verdict"] == "fail" for result in results) else (
        "skip" if any(result["verdict"] == "skip" for result in results) else "pass")
    return dict(verdict=verdict,
                reason="; ".join(result["reason"] for result in results),
                evidence=[record for result in results for record in result["evidence"]])
