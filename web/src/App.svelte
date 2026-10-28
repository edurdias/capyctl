<script lang="ts">
  import Shell from './components/Shell.svelte';
  import { route } from './lib/router';
  import SignIn from './routes/sign-in/SignIn.svelte';
</script>

<!-- The sign-in page stands alone: without a session there is nothing for
     the shell to load. -->
{#if $route === '/sign-in'}
  <SignIn />
{:else}
  <Shell />
{/if}
