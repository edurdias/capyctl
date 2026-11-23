<script lang="ts">
  // SPEC R9, F1: a browser without a valid session is told which command to
  // run on the box.
  let { linkRefused = false }: { linkRefused?: boolean } = $props();
</script>

<h1>Sign in to CapyCTL</h1>
{#if linkRefused}
  <p class="refused" role="alert">This sign-in link was already used or has expired.</p>
{/if}
<p>This browser is not signed in. On the machine running CapyCTL, run:</p>
<pre class="command">capyctl open ui</pre>
<p>
  It prints a sign-in link and a QR code. Open the link in this browser, or scan the code with this phone. Each link
  works once, for 10 minutes.
</p>

<style>
  .refused {
    font-weight: 600;
    color: var(--capyctl-accent-text);
  }
</style>
