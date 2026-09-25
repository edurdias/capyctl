import { test } from 'node:test';
import assert from 'node:assert/strict';
import { weightsGb, fits, gpuRow, GPU_CLASSES } from '../src/data/gpu.mjs';

test('weights are parameters times bytes per parameter', () => {
  assert.equal(weightsGb(8, 2), 16);
  assert.equal(weightsGb(70, 0.5625), 39.4);
});

test('fit rule: weights x 1.2 + 2 GB within 90% of the budget', () => {
  assert.equal(fits(16, 24), true);   // 21.2 <= 21.6
  assert.equal(fits(18, 24), false);  // 32B 4-bit: 23.6 > 21.6
});

// Recomputed from weight sizes (website spec, section 4). Changing the fit
// rule or the model list must change these on purpose.
test('rows', () => {
  assert.deepEqual(gpuRow(24), { largestBf16: '8B', largestQ4: '14B', count8bBf16: 1 });
  assert.deepEqual(gpuRow(32), { largestBf16: '8B', largestQ4: '32B', count8bBf16: 1 });
  assert.deepEqual(gpuRow(48), { largestBf16: '14B', largestQ4: '32B', count8bBf16: 2 });
  assert.deepEqual(gpuRow(96), { largestBf16: '32B', largestQ4: '70B', count8bBf16: 4 });
  assert.deepEqual(gpuRow(128), { largestBf16: '32B', largestQ4: '70B', count8bBf16: 5 });
});

test('classes are memory budgets, not products', () => {
  assert.deepEqual(GPU_CLASSES.map((c) => c.label), ['Gaming card', 'High-end card', 'Workstation card', 'Workstation card', 'Unified-memory box']);
  for (const c of GPU_CLASSES) assert.doesNotMatch(c.label, /\d{4}|RTX|Radeon|Apple/i);
});
