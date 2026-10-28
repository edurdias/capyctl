<script lang="ts">
  // SPEC R10: memory in use against what the host has, in GiB.
  let { label, used, total }: { label: string; used: number; total: number } = $props();

  const GIB = 1024 ** 3;
  const fraction = $derived(total > 0 ? Math.min(1, Math.max(0, used / total)) : 0);
  const text = $derived(`${(used / GIB).toFixed(1)} of ${(total / GIB).toFixed(1)} GiB`);
</script>

<div class="memory">
  <div class="labels">
    <span>{label}</span>
    <span class="muted">{text}</span>
  </div>
  <div
    class="bar"
    role="meter"
    aria-label={label}
    aria-valuemin={0}
    aria-valuemax={total}
    aria-valuenow={used}
    aria-valuetext={text}
  >
    <div class="fill" style:width={`${fraction * 100}%`}></div>
  </div>
</div>

<style>
  .labels {
    display: flex;
    justify-content: space-between;
    gap: 1rem;
    font-size: 0.875rem;
  }
  .bar {
    height: 0.5rem;
    border-radius: 999px;
    background: var(--capyctl-rule);
    overflow: hidden;
  }
  .fill {
    height: 100%;
    background: var(--capyctl-accent);
  }
</style>
