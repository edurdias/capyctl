<script lang="ts">
  // ADR 0040, owner decision 2026-11-13: use the existing mark, no tiny tagline.
  let { height = 40 }: { height?: number } = $props();
  const base = import.meta.env.BASE_URL;
</script>
<span class="brand" style:height={`${height}px`}><img src={`${base}favicon.png`} alt="" width={height} height={height} /><span>Capy<span class="ctl">CTL</span></span></span>
<style>
  .brand { display: inline-flex; align-items: center; gap: 0.5rem; color: var(--capyctl-text); font-size: 1.375rem; font-weight: 750; letter-spacing: -0.05em; white-space: nowrap; }
  img { object-fit: contain; }
  .ctl { color: var(--capyctl-accent); }
</style>
