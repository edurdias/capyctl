"""T47/T17: CPU protocol tests only, never native-engine qualification."""
import asyncio
import json
from pathlib import Path
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

import em6_observer
import v020
from rows import embeddings_extra as em


def require(condition, reason):
    if not condition:
        raise AssertionError(reason)


def varint(value):
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def field(number, value):
    return varint(number*8+2)+varint(len(value))+value


class EmbeddingProtocolTests(unittest.TestCase):
    def test_catalog_replaces_skips_without_duplicates(self):
        catalog = v020.rows()
        for key in ('EM3', 'EM5', 'EM6', 'EM7', 'EM9'):
            matching = [row for row in catalog if row['id'] == key]
            self.assertEqual(len(matching), 1)
            self.assertEqual(matching[0]['module'], 'embeddings_extra')
            self.assertTrue(matching[0]['measurement'])
            self.assertGreater(matching[0]['budget'], 1)
            self.assertNotIn('not implemented', matching[0]['criteria'])

    def test_em3_pins_a_hundred_distinct_sentences(self):
        sentences = json.loads(em.FIXTURE.read_text())
        self.assertEqual(len(sentences), 100)
        self.assertEqual(len(set(sentences)), 100)
        self.assertTrue(all(isinstance(item, str) and item.strip() for item in sentences))

    def test_relative_tolerance_has_margin_and_qualification_ceiling(self):
        same = em.similarity([.6, .8], [.6, .8])
        self.assertAlmostEqual(same['relative_distance'], 0)
        self.assertAlmostEqual(same['cosine'], 1)
        shifted = em.similarity([.6, .8], [.619, .8])
        self.assertAlmostEqual(shifted['relative_distance'], .019)
        result = em.tolerance([same, shifted])
        self.assertAlmostEqual(result['margin'], .0019)
        self.assertGreater(result['measured_tolerance'], result['qualification_ceiling'])
        self.assertAlmostEqual(em.tolerance([same])['measured_tolerance'], .001)

    def test_similarity_rejects_dimension_and_nonfinite_changes(self):
        for observed in ([], [1, 2, 3], [float('nan'), 1], [0, 0]):
            with self.assertRaises(AssertionError):
                em.similarity([.6, .8], observed)

    def test_em3_exercises_exact_lifecycle_and_concurrent_batch_protocol(self):
        counts = {}
        launches = []
        def deploy(name, engine, **kwargs):
            launches.append((name, engine, kwargs))
        def request(name, **kwargs):
            inputs = kwargs['body']['input']
            count = len(inputs) if isinstance(inputs, list) else 1
            counts.setdefault(name, []).append(count)
            return dict(status=200, body=dict(model=name, data=[dict(index=i, embedding=[.6, .8]) for i in range(count)]))
        ctx = SimpleNamespace(deploy=deploy, request=request, remaining=lambda: 60,
                              assertion=require, save=lambda *args: None, lifecycle=lambda *args: None)
        with mock.patch.object(em, 'reference', return_value=dict(vector=[.6, .8], tolerance=.02)), \
             mock.patch.object(em, 'passing_canary', return_value={'passed': True}), \
             mock.patch.object(em, 'stopped'):
            evidence = {}
            em.em3(ctx, evidence)
        self.assertEqual(len(launches), 3)
        for name, engine, _ in launches:
            self.assertEqual(counts[name].count(1), 40 if engine.startswith('llama') else 60)
            self.assertEqual(counts[name].count(64), 20)
            self.assertEqual(counts[name].count(100), 1)
        self.assertEqual(len(evidence['cross_engine']), 3)
        self.assertTrue(all(len(pair['observations']) == 100 for pair in evidence['cross_engine'].values()))

    def test_protobuf_decodes_only_residency_fields_and_refuses_truncation(self):
        message = field(10, field(1, b'parked')+field(4, b'saver_mapped_after.0'))
        outer = list(em.protobuf_fields(message))
        self.assertEqual(outer[0][0], 10)
        self.assertEqual(list(em.protobuf_fields(outer[0][1])), [(1, b'parked'), (4, b'saver_mapped_after.0')])
        for data in (b'\x80', b'\x52\x08abc', b'\x00', b'\x0b'):
            with self.assertRaises(ValueError):
                list(em.protobuf_fields(data))

    def test_em9_reads_actual_host_journal_saver_counts(self):
        with tempfile.TemporaryDirectory() as root:
            state = Path(root)
            journal = state/'host/state/journal'
            journal.mkdir(parents=True)
            data = field(10, field(1, b'parked')+field(4, b'memory_released')+
                         field(4, b'saver_mapped_before.8000')+field(4, b'saver_mapped_after.0'))
            with sqlite3.connect(journal/'commands.sqlite') as db:
                db.executescript('CREATE TABLE commands(sequence INTEGER,command_id TEXT,deployment TEXT);'
                                 'CREATE TABLE native_results(command_id TEXT,result BLOB);')
                db.execute('INSERT INTO commands VALUES(1,?,?)', ('park-command', 'deployment'))
                db.execute('INSERT INTO native_results VALUES(?,?)', ('park-command', data))
            ctx = SimpleNamespace(status=lambda *args: dict(id='deployment'),
                                  role=lambda *args: dict(state=state), assertion=require)
            result = em.host_saver_evidence(ctx, 'model', 'enrolled')
            self.assertEqual(result['before_mapped_bytes'], 8000)
            self.assertEqual(result['after_mapped_bytes'], 0)
            self.assertEqual(result['command_id'], 'park-command')

    def test_em7_uses_actual_ready_resource_grants(self):
        records = [dict(name='embed', footprint_json=json.dumps(dict(phase='ready',
                       allocations=[['host/gpu0', 55, 0], ['host/system', 2, 0]])))]
        self.assertEqual(em.device_charge(records, 'embed'), ('host/gpu0', 55))
        records[0]['footprint_json'] = json.dumps(dict(phase='parked', allocations=[['host/gpu0', 1, 0]]))
        with self.assertRaises(AssertionError):
            em.device_charge(records, 'embed')

    def test_em6_does_not_qualify_other_engines_from_vllm_observation(self):
        with mock.patch.object(em, 'em6', side_effect=lambda ctx, evidence: evidence.update(vllm='observed')):
            result = em.run(SimpleNamespace(), dict(id='EM6'))
        self.assertEqual(result['verdict'], 'pass')
        self.assertEqual(result['evidence']['vllm'], 'observed')
        self.assertEqual(result['evidence']['scope'], 'vllm only')
        self.assertIn('-BIS', result['evidence']['unobserved_engines']['sglang'])
        self.assertIn('not an ASGI', result['evidence']['unobserved_engines']['llamacpp'])

    def test_em5_requires_its_own_answer_receipt_and_preserves_connection(self):
        for has_receipt in (True, False):
            with self.subTest(has_receipt=has_receipt):
                connection = mock.Mock()
                active = [dict(id='lease', cancelled_at_ms=None)]
                cancelling = [dict(id='lease', cancelled_at_ms=1000, engine_answered=0)]
                receipt = [dict(kind='request_cancellation_acknowledged',
                                payload_json='lease: settled on its answer')] if has_receipt else []
                ctx = SimpleNamespace(assertion=require, events=mock.Mock(side_effect=[[], receipt]))
                identities = [dict(pid=123)]
                with mock.patch.object(em, 'send_batch', return_value=connection), \
                     mock.patch.object(em, 'live_identities', return_value=identities), \
                     mock.patch.object(em, 'work_gauge', return_value=dict(running=1)), \
                     mock.patch.object(em, 'connection_states', return_value=[dict(inode='socket')]), \
                     mock.patch.object(em, 'leases', side_effect=[active, cancelling, cancelling, []]), \
                     mock.patch.object(em, 'signal_owned') as signals, \
                     mock.patch.object(em, 'wait_for', side_effect=lambda ctx, predicate, *args, **kwargs: predicate()), \
                     mock.patch.object(em.time, 'sleep'):
                    if has_receipt:
                        result = em.cut_batch(ctx, 'embed', 'vllm033', 'hangup', 0)
                        self.assertEqual(result['lease_id'], 'lease')
                        self.assertEqual(result['held_while_engine_stopped'], cancelling)
                        self.assertEqual(result['settlement'], receipt)
                    else:
                        with self.assertRaisesRegex(AssertionError, 'complete-answer settlement receipt'):
                            em.cut_batch(ctx, 'embed', 'vllm033', 'hangup', 0)
                    self.assertEqual([call.args[2] for call in signals.call_args_list],
                                     [em.signal.SIGSTOP, em.signal.SIGCONT])

    def test_em6_observer_preserves_response_and_never_reads_header_values(self):
        class Secret:
            def __str__(self):
                raise AssertionError('header value read')
            def decode(self, *args):
                raise AssertionError('header value decoded')
        sent = []
        records = []
        payload = b'x'*140000
        async def app(scope, receive, send):
            await receive()
            await send(dict(type='http.response.start', status=200, headers=[]))
            await send(dict(type='http.response.body', body=payload, more_body=False))
        async def receive():
            return dict(type='http.request', body=b'fixture', more_body=False)
        async def send(message):
            sent.append(message)
        scope = dict(type='http', path='/v1/embeddings',
                     headers=[(b'authorization', Secret()), (b'content-type', Secret())])
        with mock.patch.object(em6_observer, 'record', side_effect=lambda item: records.append(dict(item))):
            asyncio.run(em6_observer.Observe(app)(scope, receive, send))
        bodies = [item for item in sent if item['type'] == 'http.response.body']
        self.assertEqual(b''.join(item['body'] for item in bodies), payload)
        self.assertTrue(all(len(item['body']) <= 65536 for item in bodies))
        self.assertEqual([item['more_body'] for item in bodies], [True, True, False])
        self.assertEqual(records[0]['header_names'], ['authorization', 'content-type'])
        self.assertEqual(records[-1]['request_bytes'], 7)
        self.assertEqual(records[-1]['response_bytes'], len(payload))
        self.assertNotIn('header_values', json.dumps(records))

    def test_em8_checkpoints_the_largest_cell_before_execution(self):
        from rows import measurements
        snapshots = []
        evidence = {}
        class ReachedCell(Exception):
            pass
        with tempfile.TemporaryDirectory() as root:
            binary = Path(root)/'capyctl'
            binary.write_bytes(b'fixed binary')
            ctx = SimpleNamespace(engines={}, deploy=lambda *args, **kwargs: None, binary=lambda: binary,
                                  save=lambda name, data: snapshots.append(json.loads(json.dumps(data))))
            def load(ctx, name, batch, length, clients):
                if (batch, length, clients) == (256, 8192, 16):
                    raise ReachedCell
                return dict(batch=batch, input_tokens=length, clients=clients)
            with mock.patch.object(measurements, 'embedding_load', side_effect=load):
                with self.assertRaises(ReachedCell):
                    measurements.em8(ctx, evidence)
        self.assertEqual(snapshots[-1]['current_cell'], dict(batch=256, input_tokens=8192, clients=16))
        self.assertEqual(snapshots[-1]['last_completed_cell'], dict(batch=256, input_tokens=8192, clients=4))
        self.assertEqual(len(evidence['binary_sha256']), 64)


if __name__ == '__main__':
    unittest.main()
