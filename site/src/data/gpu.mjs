// Website spec, Landing page 4: illustrative, computed from weight sizes.
// One model runs on one GPU. Fit rule: weights x 1.2 (KV cache and engine
// overhead) + 2 GB must fit in 90% of the budget.
export const MODELS = [8, 14, 32, 70]; // billions of parameters
export const BF16 = 2;                 // bytes per parameter
export const Q4 = 0.5625;              // 4-bit weights with scales

export const weightsGb = (paramsB, bytes) => Math.round(paramsB * bytes * 10) / 10;
export const fits = (w, budget) => w * 1.2 + 2 <= budget * 0.9;

const largest = (budget, bytes) => {
  const ok = MODELS.filter((p) => fits(weightsGb(p, bytes), budget));
  return ok.length ? `${ok.at(-1)}B` : 'none';
};

export function gpuRow(budget) {
  return {
    largestBf16: largest(budget, BF16),
    largestQ4: largest(budget, Q4),
    count8bBf16: Math.floor((budget * 0.9) / (weightsGb(8, BF16) * 1.2 + 2)),
  };
}

export const GPU_CLASSES = [
  { label: 'Gaming card', budget: 24 },
  { label: 'High-end card', budget: 32 },
  { label: 'Workstation card', budget: 48 },
  { label: 'Workstation card', budget: 96 },
  { label: 'Unified-memory box', budget: 128 },
];
