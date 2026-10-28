<script lang="ts">
  // A screen another unit of the web UI plan fills in. It shows no data until
  // then.
  let { title, unit }: { title: string; unit: string } = $props();
</script>

<section>
  <h1>{title}</h1>
  <p class="muted">This screen is not built yet; it arrives with {unit} of the web UI plan.</p>
</section>
