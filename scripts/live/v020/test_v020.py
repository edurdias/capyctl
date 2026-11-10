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
        ctx.logs = lambda name: "server_args={'disable_prefill_cuda_graph': True, 'disable_decode_cuda_graph': False, 'cuda_graph_backend_prefill': 'disabled', 'cuda_graph_config': {'prefill': {'backend': 'disabled'}, 'decode': {'backend': 'full'}}, 'api_key': 'must-not-export'}"
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


if __name__ == '__main__':
    unittest.main()
