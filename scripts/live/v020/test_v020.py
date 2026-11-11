#!/usr/bin/env python3
"""CPU helper tests only; these never qualify a native engine."""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

import v020
from session import begin, identity, matches


class HelperTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.catalog = [dict(id='A', priority=1, budget=10, criteria='criterion A'),
                        dict(id='B', priority=2, budget=20, criteria='criterion B')]

    def test_missing_selected_row_fails(self):
        result = v020.aggregate(self.catalog, [], {'A'})
        self.assertFalse(result['selected_pass'])
        self.assertEqual(result['rows'][0]['verdict'], 'fail')
        self.assertEqual(result['rows'][1]['reason'], 'not selected')

    def test_subset_is_not_full_qualification(self):
        result = v020.aggregate(self.catalog, [dict(id='A', verdict='pass', reason='observed')], {'A'})
        self.assertTrue(result['selected_pass'])
        self.assertFalse(result['qualified'])

    def test_prior_failure_cannot_be_overwritten(self):
        result = v020.aggregate(self.catalog, [dict(id='A', verdict='fail', reason='first'),
                                dict(id='A', verdict='pass', reason='later')], {'A'})
        self.assertEqual(result['rows'][0]['verdict'], 'fail')

    def test_skip_is_not_pass(self):
        result = v020.aggregate(self.catalog, [dict(id='A', verdict='skip', reason='unsupported')], {'A'})
        self.assertFalse(result['selected_pass'])

    def test_invalid_verdict_fails(self):
        result = v020.aggregate(self.catalog, [dict(id='A', verdict='unknown', reason='bad')], {'A'})
        self.assertEqual(result['rows'][0]['verdict'], 'fail')

    def test_selection_is_case_insensitive_and_priority_ordered(self):
        self.assertEqual([row['id'] for row in v020.select_rows('b,a', self.catalog)], ['A', 'B'])
        self.assertEqual(v020.select_rows('ALL', self.catalog), self.catalog)

    def test_unknown_selection_rejected(self):
        with self.assertRaises(ValueError):
            v020.select_rows('A,C', self.catalog)

    def test_deadline_respects_row_and_session(self):
        self.assertEqual(v020.row_deadline(100, 500, 10), 110)
        self.assertEqual(v020.row_deadline(100, 105, 10), 105)
        with self.assertRaises(ValueError):
            v020.row_deadline(100, 105, 0)

    def test_functional_measurement_skip(self):
        self.assertEqual(v020.skip_reason(dict(measurement=True), True), 'measurement not allowed on this host')
        self.assertIsNone(v020.skip_reason({}, True))

    def test_discrete_skip_precedes_measurement(self):
        self.assertEqual(v020.skip_reason(dict(measurement=True, discrete=True), True, True), 'needs a discrete GPU')
        self.assertEqual(v020.skip_reason(dict(unified=True)), 'needs unified memory')

    def test_manifest_hashes_bytes_and_excludes_itself(self):
        (self.root / 'evidence').mkdir()
        (self.root / 'evidence/a.json').write_bytes(b'{"ok": true}\n')
        entries = v020.manifest(self.root)
        expected = hashlib.sha256(b'{"ok": true}\n').hexdigest() + '  evidence/a.json'
        self.assertEqual(entries, [expected])
        self.assertEqual(v020.manifest(self.root), entries)

    def test_manifest_does_not_follow_symlinks(self):
        (self.root / 'link').symlink_to('/etc/passwd')
        self.assertEqual(v020.manifest(self.root), [])

    def test_manifest_changes_when_evidence_changes(self):
        path = self.root / 'evidence.json'
        path.write_text('one')
        first = v020.manifest(self.root)
        path.write_text('two')
        self.assertNotEqual(first, v020.manifest(self.root))

    def test_atomic_json_is_whole(self):
        path = self.root / 'result.json'
        v020.atomic_json(path, {'verdict': 'pass'})
        self.assertEqual(json.loads(path.read_text()), {'verdict': 'pass'})
        self.assertFalse(path.with_suffix('.json.partial').exists())

    def test_standalone_credentials_use_colons(self):
        path = self.root / 'credentials'
        path.write_text('admin_token: private-token\napi_key: private-key\n')
        self.assertEqual(v020.credentials(path)['api_key'], 'private-key')

    def test_server_credentials_use_json(self):
        path = self.root / 'server-credentials.json'
        path.write_text(json.dumps(dict(version=1, api_key='private-key', admin_token='private-token')))
        self.assertEqual(v020.credentials(path), dict(api_key='private-key', admin_token='private-token'))

    def test_missing_credential_rejected(self):
        path = self.root / 'credentials'
        path.write_text('api_key=key\n')
        with self.assertRaises(ValueError):
            v020.credentials(path)

    def test_engine_env_is_parsed_not_executed(self):
        path = self.root / 'engines.env'
        path.write_text('VLLM_033=~/vllm-0.33.0-venv\nTENSORRT_LLM=\nMODEL_CHAT="/home/me/models/chat model"\n')
        data = v020.load_env(path)
        self.assertEqual(data['TENSORRT_LLM'], '')
        self.assertEqual(data['MODEL_CHAT'], '/home/me/models/chat model')
        path.write_text('VLLM_033=$(touch /tmp/unsafe)\n')
        with self.assertRaises(ValueError):
            v020.load_env(path)

    def test_absent_shared_host_engine_skips_without_hiding_bad_paths(self):
        ctx = v020.Context(self.root, {'VLLM_033': ''}, functional_only=True)
        with self.assertRaisesRegex(v020.UnavailableEngine, 'not installed on this host'):
            ctx.installation('VLLM_033')
        ctx.engines['VLLM_033'] = str(self.root / 'missing')
        with self.assertRaisesRegex(AssertionError, 'does not exist'):
            ctx.installation('VLLM_033')
        ctx.functional_only = False
        ctx.engines['VLLM_033'] = ''
        with self.assertRaisesRegex(AssertionError, 'unavailable'):
            ctx.installation('VLLM_033')

    def test_graph_settings_read_native_server_args_dictionary_for_one_launch(self):
        ctx = v020.Context.__new__(v020.Context)
        ctx.owned = lambda name: []
        ctx.secrets = []
        ctx.logs = lambda name: ctx.clean("server_args={'disable_prefill_cuda_graph': True, 'disable_decode_cuda_graph': False, 'cuda_graph_backend_prefill': 'disabled', 'cuda_graph_config': {'prefill': {'backend': 'disabled'}, 'decode': {'backend': 'full'}}, 'api_key': 'must-not-export', 'admin_token': None}")
        settings = ctx.engine_args('phase-test')
        self.assertEqual(settings['resolved']['disable_prefill_cuda_graph'], 'True')
        self.assertEqual(settings['resolved']['disable_decode_cuda_graph'], 'False')
        self.assertEqual(settings['resolved']['cuda_graph_backend_decode'], 'full')
        self.assertNotIn('must-not-export', json.dumps(settings))

    def test_wake_uses_start_deployment_cli(self):
        ctx = v020.Context.__new__(v020.Context)
        calls = []
        ctx.cli = lambda *args, **kwargs: calls.append(args)
        ctx.status = lambda *args: {'observed_state': 'ready'}
        ctx.deadline = v020.time.monotonic() + 10
        ctx.functional_only = True
        ctx.lifecycle('wake', 'parked-model')
        self.assertEqual(calls, [('start', 'deployment', 'parked-model')])

    def test_cache_fixture_explicitly_requests_token_ids(self):
        import contextlib
        import io
        import sys
        import types
        from unittest import mock
        from rows.lmcache import TOKENIZER
        class Tokenizer:
            def encode(self, *args, **kwargs):
                return [1] * 40000
            def decode(self, *args, **kwargs):
                return ''
            def apply_chat_template(self, *args, **kwargs):
                ids = [1] * 32064
                return ids if kwargs.get('return_dict') is False else {'input_ids': ids, 'attention_mask': ids}
        transformers = types.SimpleNamespace(AutoTokenizer=types.SimpleNamespace(from_pretrained=lambda *args, **kwargs: Tokenizer()))
        stream = io.StringIO()
        with mock.patch.dict(sys.modules, transformers=transformers), mock.patch('sys.argv', ['probe', 'model', '[["main",32000]]']), mock.patch('importlib.metadata.version', return_value='test'), contextlib.redirect_stdout(stream):
            exec(TOKENIZER, {})
        self.assertEqual(json.loads(stream.getvalue())['fixtures']['main']['prompt_tokens'], 32064)

    def test_canary_uses_current_launch_reference_and_restored_journal(self):
        from rows.engines import passing_canary
        ctx = v020.Context.__new__(v020.Context)
        reference = [dict(generation=1, incarnation='launch', kind='embedding', mismatch_operation_id=None)]
        journal = [dict(state='restored')]
        ctx.sql = lambda query, args: reference if 'wake_canaries' in query else journal
        self.assertEqual(passing_canary(ctx, 'model')['reference'], reference)
        journal.clear()
        with self.assertRaisesRegex(AssertionError, 'restored'):
            passing_canary(ctx, 'model')
        self.assertEqual(passing_canary(ctx, 'model', restore=False)['reference'], reference)
        reference[0]['mismatch_operation_id'] = 'failed'
        with self.assertRaisesRegex(AssertionError, 'matching canary'):
            passing_canary(ctx, 'model', restore=False)

    def test_restart_row_still_requires_its_explicit_verdict_event(self):
        from rows.functional import restart_canary_verdict
        self.assertFalse(restart_canary_verdict([dict(kind='ready_committed')]))
        self.assertTrue(restart_canary_verdict([dict(kind='embedding_canary', verdict='passed')]))
        self.assertFalse(restart_canary_verdict([dict(kind='embedding_canary', verdict='failed')]))

    def test_process_identity_guards_against_pid_reuse(self):
        record = identity(os.getpid())
        self.assertTrue(matches(record))
        self.assertFalse(matches(dict(record, start_ticks=record['start_ticks'] + 1)))

    def test_catalog_contains_all_required_rows(self):
        catalog = v020.rows()
        ids = [row['id'] for row in catalog]
        self.assertEqual(len(ids), len(set(ids)))
        for expected in ['ISSUE225', 'ISSUE235', 'ISSUE236', 'ISSUE240',
                         *('LC' + str(i) for i in range(12, 19)),
                         *('EM' + str(i) for i in range(1, 10)),
                         *('OT' + str(i) for i in range(1, 12)),
                         *('TF' + str(i) for i in range(1, 15)),
                         *('TR' + str(i) for i in range(1, 12))]:
            self.assertIn(expected, ids)
        for row in catalog:
            self.assertGreater(row['budget'], 0)
            self.assertTrue(row['criteria'])
        self.assertEqual([row['priority'] for row in catalog], sorted(row['priority'] for row in catalog))

    def test_resume_archives_previous_role_state_and_results(self):
        (self.root / 'completion.json').write_text('{}')
        (self.root / 'state').mkdir()
        (self.root / 'evidence').mkdir()
        (self.root / 'evidence/result-EM1.json').write_text('{}')
        (self.root / 'evidence/vllm.freeze.txt').write_text('vllm==0.33.0\n')
        begin(self.root)
        self.assertFalse((self.root / 'state').exists())
        self.assertFalse((self.root / 'evidence/result-EM1.json').exists())
        self.assertTrue((self.root / 'evidence/vllm.freeze.txt').exists())
        archived = list((self.root / 'attempts').iterdir())
        self.assertEqual(len(archived), 1)
        self.assertTrue((archived[0] / 'evidence/result-EM1.json').exists())

    def test_trt_driver_probe_compiles_for_the_detected_gpu(self):
        import subprocess
        (self.root / 'downloads').mkdir()
        setup = Path(__file__).with_name('setup.sh')
        script = '''
source "$1"
V020=$2
CUDACXX=/unused/nvcc
nvidia-smi() { printf '%s\\n' "$3"; }
v020_cmd() { printf '%s\\n' "$*"; }
v020_trt_availability() { :; }
v020_trt_driver_probe TEST
'''
        # The probe must use portable PTX for this GPU, not sm_120 on an A100.
        for capability, architecture in [('8.0', '80'), ('12.0', '120')]:
            with self.subTest(capability=capability):
                output = subprocess.run(
                    ['bash', '-c', script.replace('"$3"', capability),
                     'probe', str(setup), str(self.root)],
                    check=True, capture_output=True, text=True).stdout
                self.assertIn(
                    f'arch=compute_{architecture},code=compute_{architecture}', output)

    def test_cuda_setup_binds_the_available_cpp_host_compiler(self):
        import shutil
        import subprocess
        toolkit = self.root / 'toolkit'
        (toolkit / 'bin').mkdir(parents=True)
        compiler = toolkit / 'bin/nvcc'
        compiler.touch()
        compiler.chmod(0o700)
        script = '''
source "$1"
declare -A V020_OVERRIDES=([CUDA_HOME]=1)
CUDA_HOME=$2
EVIDENCE=$3
v020_cmd() { :; }
v020_setup_cuda
printf '%s\\n' "${NVCC_CCBIN:-unset}"
'''
        result = subprocess.run(
            ['bash', '-c', script, 'cuda', str(Path(__file__).with_name('setup.sh')),
             str(toolkit), str(self.root)],
            check=True, capture_output=True, text=True)
        self.assertEqual(result.stdout.strip(), shutil.which('c++'))

    def test_private_toolkit_uses_the_existing_matched_host_compilers(self):
        import shutil
        import subprocess
        toolkit = self.root / 'toolkit'
        (toolkit / 'bin').mkdir(parents=True)
        subprocess.run(
            ['bash', '-c', 'source "$1"; v020_toolkit_compilers "$2"',
             'compilers', str(Path(__file__).with_name('setup.sh')), str(toolkit)],
            check=True, capture_output=True, text=True)
        self.assertEqual((toolkit / 'bin/g++').resolve(), Path(shutil.which('c++')).resolve())
        cc1plus = subprocess.run(
            [str(toolkit / 'bin/gcc'), '-print-prog-name=cc1plus'],
            check=True, capture_output=True, text=True).stdout.strip()
        self.assertTrue(Path(cc1plus).is_file())

    def test_no_floor_fixture_fits_its_explicit_two_gib_kv_pool(self):
        from rows.regressions import no_floor, editable
        from types import SimpleNamespace
        from unittest import mock
        class Captured(Exception):
            pass
        class Context:
            def deploy(self, name, engine, extra, **kwargs):
                self.extra = extra
                raise Captured
        ctx = Context()
        with self.assertRaises(Captured):
            no_floor(ctx)
        self.assertEqual(ctx.extra['engine_config']['memory']['kv_cache'], '2GiB')
        self.assertEqual(ctx.extra['engine_config']['context_length'], 8192)
        ctx.installation = lambda key: None
        ctx.logs = lambda: ''
        ctx.engines = dict(SGLANG_EDITABLE='fixture')
        ctx.cli = lambda *args, **kwargs: SimpleNamespace(returncode=1, stdout='editable', stderr='')
        ctx.assertion = v020.Context.assertion
        ctx.role = lambda *args, **kwargs: None
        with mock.patch('rows.regressions.install_editable_profile', return_value=('editable', 'fixture')):
            with self.assertRaises(Captured):
                editable(ctx)
        self.assertEqual(ctx.extra['engine_config']['context_length'], 8192)

    def test_editable_fixture_registers_control_before_host_publication(self):
        from types import SimpleNamespace
        from unittest import mock
        from rows.regressions import install_editable_profile
        config = self.root / 'host.yaml'
        config.write_text('runtime_profiles: {}\n')
        published = []
        registered = []
        process = SimpleNamespace(terminate=lambda: None, wait=lambda timeout: None)
        info = dict(host_config=config, host_process=process, host_binary='old')
        ctx = SimpleNamespace(root=self.root, engines={'SGLANG_EDITABLE': '/fixture'},
                              role=lambda role: info, remaining=lambda: 60,
                              assertion=v020.Context.assertion)
        ctx.register = lambda engine, role: registered.append(engine)
        ctx.spawn_host = lambda *args: published.extend(registered)
        with mock.patch('rows.regressions.time.sleep'):
            install_editable_profile(ctx, 'server')
        self.assertEqual(published, ['VLLM_033'])

    def test_trt121_pins_numpy_and_arrow_without_constraining_rc3(self):
        import subprocess
        (self.root / 'downloads').mkdir()
        script = '''
source "$1"
V020=$2 BIN=$2 EVIDENCE=$2
declare -A V020_OVERRIDES=()
TENSORRT_LLM_121=$2/stable TENSORRT_LLM_140RC3=$2/rc3
v020_venv() { :; }
v020_cmd() { :; }
v020_trt_toolkit() { :; }
v020_freeze() { :; }
v020_trt_availability() { :; }
v020_trt_driver_probe() { :; }
v020_setup_tensorrt TENSORRT_LLM_121 1.2.1 2.9.1 cu130 13.1 CUDA_HOME_TRT121
v020_setup_tensorrt TENSORRT_LLM_140RC3 1.4.0rc3 2.14.0 cu132 13.4 CUDA_HOME_TRT140RC3
'''
        subprocess.run(['bash', '-c', script, 'constraints',
                        str(Path(__file__).with_name('setup.sh')), str(self.root)], check=True)
        stable = (self.root / 'downloads/TENSORRT_LLM_121-torch-constraints.txt').read_text()
        rc3 = (self.root / 'downloads/TENSORRT_LLM_140RC3-torch-constraints.txt').read_text()
        self.assertIn('numpy==1.26.4\npyarrow==21.*\n', stable)
        self.assertNotIn('numpy', rc3)
        self.assertNotIn('pyarrow', rc3)

    def test_tracing_refusal_accepts_the_actual_sglang_policy_diagnostic(self):
        from rows.functional import tracing_refusals
        from types import SimpleNamespace
        def cli(*args, **kwargs):
            reason = ('engine_env_reserved' if any('--env=' in arg for arg in args)
                      else 'not accepted here' if '--arg=--enable-trace' in args else 'reserved')
            return SimpleNamespace(returncode=2, stdout='', stderr=reason)
        ctx = SimpleNamespace(root=self.root, binary=lambda: 'capyctl', cli=cli,
                              installation=lambda key: '/fixture',
                              command=lambda *args, **kwargs: SimpleNamespace(
                                  returncode=13, stdout='', stderr='loopback'),
                              assertion=v020.Context.assertion)
        result = tracing_refusals(ctx, {})
        self.assertEqual(result['verdict'], 'pass')
        self.assertEqual(len(result['evidence']['profile_refusals']), 3)

    def test_embedding_load_declares_capacity_for_the_sixteen_client_cells(self):
        from rows.measurements import em8
        from types import SimpleNamespace
        launches = []
        class Captured(Exception):
            pass
        def deploy(*args, **kwargs):
            launches.append(kwargs)
            raise Captured
        ctx = SimpleNamespace(engines={}, deploy=deploy)
        with self.assertRaises(Captured):
            em8(ctx, {})
        self.assertEqual(launches[0]['extra']['engine_config']['max_concurrent_requests'], 16)

    def test_em2_enrolls_vllm_matryoshka_without_changing_restart_fixtures(self):
        from rows.functional import run
        from types import SimpleNamespace
        launches = []
        class Captured(Exception):
            pass
        def deploy(*args, **kwargs):
            launches.append(kwargs)
            raise Captured
        ctx = SimpleNamespace(deploy=deploy)
        for row in ('EM2', 'EM4'):
            with self.assertRaises(Captured):
                run(ctx, dict(id=row))
        config = launches[0]['extra']['engine_config']
        self.assertTrue(config['accept_extra_args'])
        self.assertEqual(config['extra_args'], ['--hf-overrides', '{"is_matryoshka":true}'])
        self.assertEqual(launches[1]['extra'], {})
        ctx = v020.Context(self.root, {})
        ctx.owned = lambda name: []
        ctx.logs = lambda name: "non-default args: {'hf_overrides': {'is_matryoshka': True}}"
        self.assertEqual(ctx.engine_args('model')['resolved']['hf_overrides'], {'is_matryoshka': True})

    def test_llama_embeddings_keep_required_context_and_derive_kv_from_gguf(self):
        ctx = v020.Context(self.root, {})
        ctx.role = lambda role: None
        ctx.register = lambda engine, role: 'profile'
        ctx.cli = lambda *args, **kwargs: None
        ctx.status = lambda *args: {}
        ctx.deploy('embedding', 'llamacpp090', model='embedding', api='embeddings')
        config = json.loads((self.root / 'embedding.json').read_text())
        self.assertEqual(config['engine_config']['context_length'], 32768)
        self.assertEqual(config['engine_config']['max_concurrent_requests'], 8)
        self.assertNotIn('memory', config['engine_config'])
        ctx.deploy('python-embedding', 'vllm033', model='embedding', api='embeddings')
        config = json.loads((self.root / 'python-embedding.json').read_text())
        self.assertEqual(config['engine_config']['memory']['kv_cache'], '5GiB')

    def test_old_server_defaults_to_old_host_but_accepts_explicit_generation_override(self):
        from types import SimpleNamespace
        from unittest import mock
        ctx = v020.Context(self.root, {})
        state = self.root / 'state/old-server'
        (state / 'identity').mkdir(parents=True)
        (state / 'identity/server-credentials.json').write_text(
            '{"api_key":"fixture","admin_token":"fixture-admin"}')
        (self.root / 'old-server.log').write_text('ready')
        def initialize(argv, **kwargs):
            v020.atomic_json(Path(argv[argv.index('--output') + 1]), dict(
                listeners={kind: {} for kind in ('inference', 'management', 'bootstrap', 'control')}))
        ctx.command = initialize
        ctx.spawn = lambda *args: SimpleNamespace(poll=lambda: None)
        with mock.patch.object(ctx, 'enroll') as enroll:
            ctx.role('old-server', old=True)
        enroll.assert_called_once_with('old-server', old=True)
        ctx.roles.clear()
        with mock.patch.object(ctx, 'enroll') as enroll:
            ctx.role('old-server', old=True, host_old=False)
        enroll.assert_called_once_with('old-server', old=False)

    def test_failed_native_start_preserves_sanitized_engine_diagnostics(self):
        ctx = v020.Context(self.root, {})
        ctx.row_id = 'TEST'
        ctx.role = lambda role: None
        ctx.register = lambda engine, role: 'profile'
        ctx.logs = lambda name: 'engine refused start; api_key: private-key'
        ctx.secrets = ['private-key']
        def cli(*args, **kwargs):
            if args[0] == 'start':
                raise AssertionError('operation failed')
        ctx.cli = cli
        with self.assertRaisesRegex(AssertionError, 'operation failed'):
            ctx.deploy('sample', 'vllm033')
        evidence = json.loads(
            (ctx.evidence / 'TEST-sample-engine-failure.json').read_text())
        self.assertIn('engine refused start', evidence['log'])
        self.assertNotIn('private-key', evidence['log'])

    def test_runtime_profiles_live_in_the_archived_attempt_state(self):
        v020.Context(self.root, {})
        self.assertTrue(
            Path(os.environ['XDG_CONFIG_HOME']).is_relative_to(self.root / 'state'))

    def test_embedding_models_metadata_is_nested_and_matches_the_served_name(self):
        from rows.measurements import advertises_embeddings
        response = dict(status=200, body=dict(data=[
            dict(id='embedding-model', capyctl=dict(api='embeddings'))]))
        self.assertTrue(advertises_embeddings(response, 'embedding-model'))
        self.assertFalse(advertises_embeddings(response, 'different-model'))

    def test_tensorfold_native_startup_reports_rounded_gib_sizes(self):
        from rows.measurements import tensorfold_startup_sizes
        line = ("[tensorfold] CUDA sm_80 device 0 (NVIDIA A100-SXM4-80GB): "
                "model 18.61 GiB; 8 streams at once, the engine's own at a "
                "32768-token window (0.28 GiB, in the model's), 7 more taking "
                "455 pages of 512 rows (1.56 GiB) as they need them")
        sizing = tensorfold_startup_sizes(line)
        self.assertEqual(sizing['stream_count'], 8)
        self.assertAlmostEqual(sizing['model_bytes'] / 1024**3, 18.61)
        self.assertAlmostEqual(sizing['stream_bytes'] / 1024**3, .28)
        self.assertEqual(sizing['sizing_provenance'], 'rounded engine startup GiB')

    def test_isolated_measurement_profiles_and_state_are_attempt_local(self):
        import sys
        from rows.measurements import isolated_role
        ctx = v020.Context(self.root, {})
        (self.root / 'bin').mkdir()
        binary = self.root / 'bin/capyctl'
        binary.write_text(
            '#!' + sys.executable + '\n'
            'import pathlib,signal,sys\n'
            'state=pathlib.Path(sys.argv[sys.argv.index("--state-dir")+1])\n'
            '(state/"identity").mkdir(parents=True)\n'
            '(state/"identity/credentials").write_text("fixture")\n'
            'print("ready",flush=True)\n'
            'signal.pause()\n')
        binary.chmod(0o700)
        previous = os.environ['XDG_CONFIG_HOME']
        with isolated_role(ctx, 'test') as info:
            self.assertTrue(info['state'].is_relative_to(self.root / 'state'))
            self.assertEqual(os.environ['XDG_CONFIG_HOME'], str(info['state'] / 'config'))
        self.assertEqual(os.environ['XDG_CONFIG_HOME'], previous)

    def test_lifecycle_fixtures_respect_decisions_residency_and_llama_kv_budget(self):
        from rows.engines import run
        from types import SimpleNamespace
        captured = {}
        def deploy(*args, **kwargs):
            captured.update(kwargs)
            raise AssertionError('fixture captured')
        ctx = SimpleNamespace(register=lambda engine: 'profile', deploy=deploy)
        run(ctx, dict(id='P3-decisions-sglang0523', engine='sglang0523', decisions=True))
        self.assertEqual(captured['residency'], 'restart_only')
        captured.clear()
        run(ctx, dict(id='P3-llamacpp090', engine='llamacpp090'))
        settings = captured['extra']['engine_config']
        self.assertEqual(settings['context_length'], 8192)
        self.assertEqual(settings['max_concurrent_requests'], 2)

    def test_row_summary_does_not_hash_the_private_install_tree(self):
        from unittest import mock
        with mock.patch('v020.manifest', side_effect=AssertionError('not an export tree')):
            report = v020.summary(self.root, 'ISSUE225')
        self.assertFalse(report['selected_pass'])
        self.assertTrue((self.root / 'summary.json').is_file())

    def test_sleep_cache_refusal_proves_only_the_guard_not_qualification(self):
        from unittest import mock
        from rows.lmcache import Protocol, sleep_guard
        ctx = v020.Context(self.root, {})
        ctx.row_id, ctx.index = 'LC13', 1
        ctx.save('operation-1', dict(stderr='sleep_clears_cache', stdout=''))
        ctx.request = lambda *args, **kwargs: dict(status=200)
        p = Protocol(ctx, dict(id='LC13'))
        def deploy(suffix, mode):
            if mode == 'required':
                raise AssertionError('operation 1 failed')
            return 'auto'
        p.deploy = deploy
        p.lifecycle = lambda action, name: dict(status=action)
        p.retire = lambda name: None
        with mock.patch('rows.lmcache.cache_ledger', return_value=dict(
                planned='off', state='off', reason='park_unqualified', planned_reason=None)):
            self.assertTrue(sleep_guard(p))
        self.assertTrue(any('not qualified' in reason for reason in p.failures))
        self.assertFalse(any('auto sleep parking did not disable' in reason for reason in p.failures))

    def test_native_sglang_embedding_setting_does_not_require_cli_argv(self):
        from rows.measurements import argv_settings
        from types import SimpleNamespace
        ctx = SimpleNamespace(owned=lambda name: [], engine_args=lambda name: dict(
            resolved=dict(is_embedding='True')))
        self.assertTrue(argv_settings(ctx, 'embedding-model')['is-embedding'])

    def test_invalid_host_config_is_rejected_before_starting_a_process(self):
        import sys
        ctx = v020.Context(self.root, {})
        binary = self.root / 'host-fixture'
        marker = self.root / 'host-started'
        binary.write_text(
            '#!' + sys.executable + '\nimport pathlib,sys\n'
            'if sys.argv[1:3] == ["validate","config"]: sys.exit(2)\n'
            f'pathlib.Path({str(marker)!r}).touch()\n')
        binary.chmod(0o700)
        with self.assertRaises(AssertionError):
            ctx.spawn_host(binary, self.root / 'invalid.yaml', 'invalid-host')
        self.assertFalse(marker.exists())

    def test_embedding_load_failure_retains_logs_before_cleanup(self):
        from unittest import mock
        from rows import measurements
        ctx = v020.Context(self.root, {})
        ctx.row_id = 'EM8'
        ctx.logs = lambda: 'router backend completion unverified\nengine failed'
        with mock.patch('rows.measurements.em8', side_effect=AssertionError('embedding request failed')):
            result = measurements.run(ctx, dict(id='EM8'))
        self.assertEqual(result['verdict'], 'fail')
        diagnostic = json.loads((ctx.evidence / 'EM8-failure-logs.json').read_text())
        self.assertIn('backend completion unverified', diagnostic['logs'])
        self.assertIn('engine failed', diagnostic['logs'])
        self.assertEqual(result['evidence']['failure_logs'], 'EM8-failure-logs.json')

    def test_router_json_timing_header_and_complete_sse_comment_are_observed(self):
        import http.server
        import threading
        from rows.measurements import timing_phases
        header = json.dumps(dict(selection_ms=1.0, total_ms=None, instance=0))
        self.assertEqual(timing_phases(dict(response_headers={'x-capyctl-timing': header})),
                         dict(selection_ms=1.0))
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers['Content-Length']))
                body = (b'data: {"choices":[{"delta":{"content":"42"}}]}\n\n'
                        b': x-capyctl-timing {"selection_ms":1.0,"total_ms":2.0}\n\n'
                        b'data: [DONE]\n\n')
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('x-capyctl-timing', header)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *args):
                pass
        server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
        worker = threading.Thread(target=server.handle_request)
        worker.start()
        ctx = v020.Context(self.root, {})
        cred = self.root / 'credentials'
        cred.write_text('api_key: fixture\nadmin_token: fixture-admin\n')
        ctx.role = lambda role: dict(port=server.server_port, cred=cred)
        try:
            response = ctx.request('model', stream=True)
            self.assertEqual(timing_phases(response), dict(selection_ms=1.0, total_ms=2.0))
            self.assertTrue(response['done'])
        finally:
            worker.join(timeout=5)
            server.server_close()


if __name__ == '__main__':
    unittest.main()
