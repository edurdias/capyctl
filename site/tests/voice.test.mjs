import { test } from 'node:test';
import assert from 'node:assert/strict';
import { visibleText, findViolations } from '../scripts/lib/voice.mjs';

test('visible text excludes code, scripts and attributes', () => {
  const html = '<p title="powered by x">Hello <code>tested on box</code></p><script>powered by</script><pre>x</pre>';
  assert.equal(visibleText(html).trim(), 'Hello');
});

test('flags forbidden phrases and local denylist entries', () => {
  assert.deepEqual(findViolations('Powered by nothing. Tested on my rig.', []), ['powered by', 'tested on']);
  assert.deepEqual(findViolations('runs on box-one fine', ['box-one']), ['box-one']);
  assert.deepEqual(findViolations('Park a model to free memory.', ['box-one']), []);
});
