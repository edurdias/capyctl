<script lang="ts">
  // SPEC R34: the horizontal lockups from docs/brand: the dark-background
  // image by default, the light one when the system is light.
  let { height = 40 }: { height?: number } = $props();
  const base = import.meta.env.BASE_URL;
  const width = $derived(height * 3);
</script>

<span class="brand">
  <img class="dark" src={`${base}horizontal-dark.png`} alt="CapyCTL" {width} {height} />
  <img class="light" src={`${base}horizontal-light.png`} alt="CapyCTL" {width} {height} />
</span>

<style>
  .brand {
    display: inline-flex;
    align-items: center;
  }
  .dark {
    display: block;
  }
  .light {
    display: none;
  }
  @media (prefers-color-scheme: light) {
    .dark {
      display: none;
    }
    .light {
      display: block;
    }
  }
</style>
