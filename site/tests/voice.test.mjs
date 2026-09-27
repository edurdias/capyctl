import { test } from 'node:test';
import assert from 'node:assert/strict';
import { visibleText, findViolations, findInternalTerms, findPorts } from '../scripts/lib/voice.mjs';

test('visible text excludes code, scripts and attributes', () => {
  const html = '<p title="powered by x">Hello <code>tested on box</code></p><script>powered by</script><pre>x</pre>';
  assert.equal(visibleText(html).trim(), 'Hello');
});

test('flags forbidden phrases and local denylist entries', () => {
  assert.deepEqual(findViolations('Powered by nothing. Tested on my rig.', []), ['powered by', 'tested on']);
  assert.deepEqual(findViolations('runs on box-one fine', ['box-one']), ['box-one']);
  assert.deepEqual(findViolations('Park a model to free memory.', ['box-one']), []);
});

test('flags marketing words', () => {
  assert.deepEqual(findViolations('A seamless, powerful manager.', []), ['seamless', 'powerful']);
});

test('flags internal terms as whole words only', () => {
  assert.deepEqual(findInternalTerms('See SPEC §6.4 and the ledger.'), ['spec §', 'ledger']);
  assert.deepEqual(findInternalTerms('Each release is released; a readr.'), []);
  assert.deepEqual(findInternalTerms('The ADR 0013 lease.'), ['adr', 'lease']);
});

test('the memory ledger is a defined user term; a bare ledger is not', () => {
  assert.deepEqual(findInternalTerms('The memory ledger counts memory.'), []);
  assert.deepEqual(findInternalTerms('The Memory ledger and the ledger.'), ['ledger']);
});

test('the landing page port check finds ports in text and code, not in styles', () => {
  assert.deepEqual(findPorts('<p>One OpenAI-compatible endpoint</p><style>.a{max-width:720px}</style>'), []);
  assert.deepEqual(findPorts('<div><code>:8443/v1</code></div>'), [':8443']);
  assert.deepEqual(findPorts('<pre>listening on 127.0.0.1:7443</pre>'), [':7443']);
  assert.deepEqual(findPorts('<p>port 8443</p>'), ['8443']);
});
