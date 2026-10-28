<script lang="ts">
  import { href, link } from '../lib/router';
</script>

<section>
  <h1>Page not found</h1>
  <p class="muted">There is nothing at this address. <a href={href('/')} use:link>Go to the Overview</a>.</p>
</section>
