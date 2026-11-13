"""CPU contract tests for ADR 0039 rows; never native qualification."""
import math
import unittest
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
from unittest.mock import Mock, patch

from rows.measurements import isolated_role

import v020
from rows import tensorfold_native as tf


class NativeContracts(unittest.TestCase):
    def test_t41_isolated_reproof_cleans_replacement_controller(self):
        # T41 / ADR 0039 TF10: replacing a controller cannot leak its listeners
        # into the following qualification rows, even on an assertion failure.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / 'state/isolated-test'
            original, replacement = Mock(), Mock()
            original.poll.return_value = None
            def spawn(*args, **kwargs):
                (state / 'identity').mkdir()
                (state / 'identity/credentials').touch()
                (root / 'test.log').write_text('ready')
                return original
            ctx = SimpleNamespace(root=root, roles={}, registered=set(), deployments=[],
                                  binary=lambda: 'capyctl', model_store=root,
                                  remaining=lambda: 30, cleanup=Mock(),
                                  assertion=lambda condition, message: self.assertTrue(condition, message))
            with patch.dict(os.environ, XDG_CONFIG_HOME=directory), \
                    patch('subprocess.Popen', side_effect=spawn), \
                    patch('rows.measurements.socket.socket') as socket:
                socket.return_value.__enter__.return_value.connect_ex.return_value = 111
                with self.assertRaisesRegex(AssertionError, 'injected'):
                    with isolated_role(ctx, 'test') as info:
                        info['process'] = replacement
                        raise AssertionError('injected')
            replacement.terminate.assert_called_once()
            replacement.wait.assert_called_once()
            original.terminate.assert_not_called()
            self.assertEqual(ctx.roles, {})

    def test_t41_topology_fixture_reaches_native_refusal_gate(self):
        # T41: never mistake an invalid short resource form for native refusal.
        ctx = SimpleNamespace(row_id='TF4', model_store=Path('/models'),
                              register=lambda key: key, root=Path('/tmp'),
                              assertion=lambda condition, message: self.assertTrue(condition, message),
                              owned=lambda name: [])
        result = SimpleNamespace(returncode=2, stdout='', stderr='multi_gpu_unsupported: the TensorFold native line')
        with patch.object(tf, 'install', return_value=result) as install:
            tf.refused(ctx, '1.0.0', 'topology', dict(topology=dict(tensor_parallel=2)), ['multi_gpu_unsupported'])
        value = install.call_args.args[1]
        self.assertNotIn('devices', value)
        self.assertNotIn('gpu', value['resources'])

    def test_tf5_tf6_require_both_whole_gauges(self):
        self.assertEqual(tf.work('tensorfold:requests_running 4\ntensorfold:requests_waiting 0\n'),
                         dict(running=4, waiting=0))
        for text in ('', 'tensorfold:requests_running 0',
                     'tensorfold:requests_running NaN\ntensorfold:requests_waiting 0',
                     'tensorfold:requests_running -1\ntensorfold:requests_waiting 0',
                     'tensorfold:requests_running 0\ntensorfold:requests_running 1\ntensorfold:requests_waiting 0'):
            with self.subTest(text=text), self.assertRaises(AssertionError):
                tf.work(text)

    def test_tf7_keeps_per_pool_labels(self):
        values = tf.samples('tensorfold:kv_cache_usage_ratio{pool="a"} 0.25\n'
                            'tensorfold:kv_cache_usage_ratio{pool="b"} 0.75\n',
                            'tensorfold:kv_cache_usage_ratio')
        self.assertEqual(len(values), 2)
        self.assertNotEqual(values[0]['labels'], values[1]['labels'])
        self.assertEqual([item['value'] for item in values], [.25, .75])

    def report(self, running=0, waiting=0, fresh=True, kv=123):
        return dict(deployments=[dict(instances=[dict(sample=dict(fresh=fresh,
                    engine=dict(running=running, waiting=waiting, kv_usage_ppm=kv)))])])

    def test_tf7_remote_idle_is_fresh_and_known(self):
        self.assertIsNotNone(tf.report_idle(self.report()))
        for report in ({}, self.report(fresh=False), self.report(running=1),
                       self.report(waiting=1), self.report(kv=None), self.report(running=None)):
            self.assertIsNone(tf.report_idle(report))
        self.assertIsNotNone(tf.report_busy(self.report(running=4)))
        self.assertIsNone(tf.report_busy(self.report(fresh=False, running=4)))

    def test_tf7_both_real_engine_histograms_required(self):
        def series(name, source='engine', count=4):
            return dict(name=name, source=source, tier='engine', count=count, buckets=[dict(count=count, le=1)])
        names = ('engine_time_to_first_token', 'engine_e2e_request_latency')
        good = dict(series=[series(name) for name in names])
        self.assertIs(tf.forwarded_histograms(good), good)
        self.assertIsNone(tf.forwarded_histograms(dict(series=[series(names[0])])))
        self.assertIsNone(tf.forwarded_histograms(dict(series=[series(name, source='capyctl') for name in names])))
        self.assertIsNone(tf.forwarded_histograms(dict(series=[series(name, count=0) for name in names])))
        self.assertIsNone(tf.forwarded_histograms({'text': 'engine ttft end_to_end'}))

    def test_tf7_remote_receipt_not_a_local_or_stop_settlement(self):
        receipt = ("the host reported the launch's engine with 0 running and 0 waiting and 0 in flight "
                   "at its ingress after the request was cancelled or answered with an error")
        event = dict(kind='request_cancellation_acknowledged',
                     payload_json=dict(deployment_id='d', count=4, receipt=receipt))
        self.assertIsNotNone(tf.remote_settlement([event], 'd'))
        self.assertIsNone(tf.remote_settlement([event], 'other'))
        for replacement in ('the local engine is idle', 'verified stop', ''):
            body = {**event['payload_json'], 'receipt': replacement}
            self.assertIsNone(tf.remote_settlement([{**event, 'payload_json': body}], 'd'))
        body = {**event['payload_json'], 'count': 3}
        self.assertIsNone(tf.remote_settlement([{**event, 'payload_json': body}], 'd'))

    def test_tf13_signal_timestamps_are_not_stop_invocation(self):
        events = tf.trace_signals('123 1794360000.125000 --- SIGTERM {si_signo=SIGTERM} ---\n'
                                 '123 1794360001.375000 +++ exited with 0 +++\n'
                                 '123 1794360002.0 unrelated text\n')
        self.assertEqual([event['signal'] for event in events], ['SIGTERM', 'exit'])
        self.assertAlmostEqual(events[1]['at'] - events[0]['at'], 1.25)
        self.assertEqual(tf.trace_signals('stop accepted at 1234'), [])

    def test_topology_refusals_require_the_exact_shared_code_for_each_shape(self):
        # TF4 / ADR 0039 owner-decision amendment, 2026-11-12.
        ctx = SimpleNamespace(row_id='TF4', model_store=Path('/models'),
                              register=lambda key: key, root=Path('/tmp'),
                              assertion=lambda condition, message: self.assertTrue(condition, message),
                              owned=lambda name: [])
        for hosts, code in ((None, 'multi_gpu_unsupported'),
                            (['a', 'b'], 'group_shape_unsupported:tensorfold')):
            extra = dict(topology=dict(tensor_parallel=2))
            if hosts:
                extra['placement'] = dict(hosts=hosts)
            for actual in (code, 'group_unsupported', 'multi_gpu_unsupported',
                           'group_shape_unsupported:tensorfold', code + '_wrong'):
                with self.subTest(hosts=hosts, actual=actual):
                    result = SimpleNamespace(returncode=2, stdout='', stderr=actual + ': the TensorFold native line')
                    with patch.object(tf, 'install', return_value=result):
                        if actual == code:
                            tf.refused(ctx, '1.0.0', 'topology', extra, [])
                        else:
                            with self.assertRaises(AssertionError):
                                tf.refused(ctx, '1.0.0', 'topology', extra, [])

    def test_tf_rows_registered_once_and_exact_100_gated(self):
        wanted = {'TF4', 'TF5', 'TF6', 'TF7', 'TF8', 'TF9', 'TF10', 'TF12', 'TF13', 'TF14'}
        catalog = v020.rows()
        for key in wanted:
            matches = [row for row in catalog if row['id'] == key]
            self.assertEqual(len(matches), 1)
            row = matches[0]
            self.assertEqual(row['module'], 'tensorfold_native')
            self.assertNotIn('not implemented', row['criteria'])
            self.assertGreaterEqual(row['budget'], 1800)
            if int(key[2:]) < 11:
                self.assertEqual(v020.skip_reason(row), 'needs unified memory')
                self.assertIsNone(v020.skip_reason(row, unified=True))
            else:
                self.assertIsNone(v020.skip_reason(row))


if __name__ == '__main__':
    unittest.main()
