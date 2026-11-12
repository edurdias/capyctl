"""ADR 0037 OT* harness contracts only; CPU tests are not qualification."""
import json
from pathlib import Path
import socket
import struct
import tempfile
import unittest
from unittest.mock import patch

import v020
from rows import tracing


class Context:
    assertion = staticmethod(v020.Context.assertion)


class TracingTests(unittest.TestCase):
    def test_t48_ot7_propagates_skipped_baseline_but_not_failure(self):
        with tempfile.TemporaryDirectory() as root:
            ctx = Context()
            ctx.evidence = Path(root)
            receipt = ctx.evidence / 'result-OT8.json'
            receipt.write_text(json.dumps(dict(verdict='skip', reason='not installed: SGLANG_0523_TRACING')))
            with self.assertRaises(v020.UnavailableEngine):
                tracing.ot7(ctx, {})
            receipt.write_text(json.dumps(dict(verdict='fail', reason='missing measurements')))
            with self.assertRaisesRegex(AssertionError, 'baseline did not pass'):
                tracing.ot7(ctx, {})

    def test_ot_catalog_is_unique_and_implemented(self):
        rows = v020.select_rows(','.join('OT'+str(n) for n in range(1, 12)))
        self.assertEqual(len(rows), 11)
        self.assertEqual(len({row['id'] for row in rows}), 11)
        self.assertTrue(all(row['budget'] > 1 for row in rows))
        self.assertEqual(next(row for row in rows if row['id'] == 'OT7')['module'], 'tracing')

    def test_status_requires_exact_state_and_reason(self):
        class StatusContext(Context):
            def status(self, *_args):
                return {'instances': [{'engine_tracing': {'state': 'off', 'reason': 'retention_zero'}}]}
        ctx = StatusContext()
        tracing.engine_status(ctx, 'model', 'retention_zero')
        with self.assertRaises(AssertionError):
            tracing.engine_status(ctx, 'model')
        with self.assertRaises(AssertionError):
            tracing.engine_status(ctx, 'model', 'not_installed')

    def test_ot5_requires_engine_parent_and_sglang_id(self):
        entry = {'engine_spans': [{'parent': 'capyctl.forward', 'name': 'llm_request'}]}
        tracing.engine_parent(Context(), entry, 'VLLM_033_TRACING')
        with self.assertRaises(AssertionError):
            tracing.engine_parent(Context(), entry, 'SGLANG_0523_TRACING')
        entry['engine_spans'][0]['gen_ai.request.id_equals_request_id'] = True
        tracing.engine_parent(Context(), entry, 'SGLANG_0523_TRACING')
        with self.assertRaises(AssertionError):
            tracing.engine_parent(Context(), entry, 'SGLANG_0523_TRACING', 'capyctl.ingress')

    def test_ot4_rejects_missing_phase_and_unlinked_activation(self):
        entry = {'activations': [{'steps': ['capyctl.wake', 'capyctl.readiness', 'capyctl.canary'], 'linked_span': 'capyctl.activation'}],
                 'timing_vs_spans': [{'phase': key, 'diff_ms': 1} for key in ('activation_wait_ms', 'queue_wait_ms', 'total_ms')]}
        required = {'capyctl.wake', 'capyctl.readiness', 'capyctl.canary'}
        tracing.check_activation(Context(), entry, required)
        entry['timing_vs_spans'][0]['diff_ms'] = 1.01
        with self.assertRaises(AssertionError):
            tracing.check_activation(Context(), entry, required)
        entry['timing_vs_spans'][0]['diff_ms'] = 0
        entry['activations'][0]['linked_span'] = None
        with self.assertRaises(AssertionError):
            tracing.check_activation(Context(), entry, required)

    def test_each_request_uses_a_distinct_caller_trace(self):
        class RequestContext(Context):
            def request(self, *_args, **kwargs):
                self.headers = kwargs['headers']
                return dict(status=200, body={'choices': [{'message': {'content': '42'}}]},
                            response_headers={'X-CapyCTL-Request-Id': 'rid'})
        ctx, records = RequestContext(), []
        tracing.request(ctx, 'model', records, 'first')
        first = ctx.headers['traceparent']
        tracing.request(ctx, 'model', records, 'second')
        self.assertNotEqual(first, ctx.headers['traceparent'])
        self.assertEqual(records[1]['caller']['trace_id'], ctx.headers['traceparent'].split('-')[1])
        self.assertEqual(records[1]['request_id'], 'rid')

    def test_summary_requires_real_request_span_and_allowlist(self):
        tools = tracing.span_tools()
        span = dict(trace='a'*32, span='b'*16, parent='', name='capyctl.request',
                    start=1, end=1000001, kind='server', attrs={'capyctl.request_id': 'rid'},
                    events=[], links=[], status={}, service='capyctl-standalone',
                    resource={'service.name': 'capyctl-standalone'}, scope='test')
        records = [{'request_id': 'rid', 'label': 'request'}]
        result = tracing.summarize(Context(), [span], records)
        self.assertEqual(result['requests'][0]['request_spans'], 1)
        with self.assertRaises(AssertionError):
            tracing.summarize(Context(), [], records)
        span['attrs']['prompt'] = 'private'
        with self.assertRaises(AssertionError):
            tracing.summarize(Context(), [span], records)
        self.assertTrue(tools.outside_allow_list([span])[0])

    def test_otlp_parser_handles_hex_ids_and_malformed_file(self):
        tools = tracing.span_tools()
        doc = {'resourceSpans': [{'resource': {'attributes': [{'key': 'service.name', 'value': {'stringValue': 'vllm'}}]},
                                'scopeSpans': [{'spans': [{'traceId': 'ab'*16, 'spanId': 'cd'*8, 'parentSpanId': 'ef'*8,
                                                          'name': 'llm_request', 'startTimeUnixNano': '1', 'endTimeUnixNano': '2'}]}]}]}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'otlp.jsonl'
            path.write_text(json.dumps(doc) + '\n')
            spans, problems = tools.load_spans([str(path)])
            self.assertFalse(problems)
            self.assertEqual(spans[0]['parent'], 'ef'*8)
            path.write_text('malformed\n')
            self.assertTrue(tools.load_spans([str(path)])[1])

    def test_capture_retains_header_names_not_values_and_reassembles(self):
        def packet(sequence, payload, destination=19790):
            ethernet = b'\0'*12 + b'\x08\x00'
            ip = bytearray(20)
            ip[0], ip[9] = 0x45, 6
            tcp = bytearray(20)
            struct.pack_into('!HHI', tcp, 0, 20000, destination, sequence)
            tcp[12] = 0x50
            return ethernet + ip + tcp + payload
        capture = tracing.HeaderCapture([19790])
        first = b'POST /v1/chat/completions HTTP/1.1\r\nAuthoriz'
        second = b'ation: Bearer supersecret\r\ntraceparent: private-context\r\n\r\n{"secret":"body"}'
        packets = iter([packet(100, first), packet(100, first), packet(100+len(first), second)])
        class FakeSocket:
            def bind(self, _address):
                pass
            def settimeout(self, _seconds):
                pass
            def recv(self, _size):
                try:
                    return next(packets)
                except StopIteration:
                    capture.done.set()
                    raise socket.timeout()
            def close(self):
                pass
        with patch.object(tracing.socket, 'socket', return_value=FakeSocket()):
            with capture as records:
                capture.thread.join(timeout=1)
        self.assertEqual(len(records), 1)
        self.assertTrue(records[0]['traceparent'])
        self.assertEqual(records[0]['header_names'], ['authorization', 'traceparent'])
        serialized = json.dumps(records)
        self.assertNotIn('supersecret', serialized)
        self.assertNotIn('private-context', serialized)
        self.assertNotIn('body', serialized)


    def test_fresh_tracing_installs_do_not_mutate_on_resume(self):
        import subprocess
        setup = Path(__file__).with_name('setup.sh')
        script = r'''
set -euo pipefail
source "$1"
declare -A V020_OVERRIDES=()
export BIN=$2/bin
key=$3
printf -v "$key" '%s' "$2/$key"
v020_claim() { :; }
v020_venv() { mkdir -p "$1/bin"; }
v020_cmd() { printf '%s\n' "$*"; }
v020_freeze() { printf 'FINGERPRINT\n'; }
if [ "$key" = VLLM_033_TRACING ]; then
  v020_install_vllm "$key" 0.33.0
  v020_install_vllm "$key" 0.33.0
else
  v020_install_sglang "$key" 0.5.23
  v020_install_sglang "$key" 0.5.23
fi
'''
        for key in ('VLLM_033_TRACING', 'SGLANG_0523_TRACING', 'SGLANG_0523_SDK_FREE'):
            with self.subTest(key=key), tempfile.TemporaryDirectory() as directory:
                result = subprocess.run(['bash', '-c', script, 'fixture', str(setup), directory, key],
                                        capture_output=True, text=True, check=True)
                self.assertEqual(result.stdout.count('pip install'), 1)
                self.assertEqual(result.stdout.count('FINGERPRINT'), 2)
                if key.endswith('SDK_FREE'):
                    self.assertIn('sglang==0.5.23', result.stdout)
                    self.assertNotIn('sglang[tracing]', result.stdout)
                    self.assertNotIn('opentelemetry-sdk==', result.stdout)
                else:
                    self.assertIn('opentelemetry-api==1.39.1', result.stdout)
                    self.assertIn('opentelemetry-sdk==1.39.1', result.stdout)
                    self.assertIn('opentelemetry-exporter-otlp==1.39.1', result.stdout)


if __name__ == '__main__':
    unittest.main()
