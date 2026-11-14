<script lang="ts">
  import Refusal from '../../components/Refusal.svelte';
  import { ApiError } from '../../lib/api';
  import { deploymentAttempt, send } from '../../lib/operate';
  import type { DeploymentSnapshot } from '../../lib/types';

  // SPEC R29: a stopped model offers Start in the playground; it is the same
  // action the model's page sends (plan KTD16).
  let { deployment, onstarted }: { deployment: DeploymentSnapshot; onstarted?: () => void } = $props();

  let busy = $state(false);
  let error = $state<ApiError | null>(null);

  async function start(): Promise<void> {
    busy = true;
    error = null;
    try {
      await send(deploymentAttempt(deployment, 'start'));
      onstarted?.();
    } catch (caught) {
      if (!(caught instanceof ApiError)) {
        throw caught;
      }
      error = caught;
    } finally {
      busy = false;
    }
  }
</script>

<div class="start">
  <p>{deployment.name} is stopped. Start it to try it here.</p>
  <button type="button" class="primary" disabled={busy} onclick={start}>{busy ? 'Starting…' : 'Start'}</button>
  {#if error}
    <Refusal {error} />
  {/if}
</div>

<style>
  .start {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.75rem;
    margin: 1rem 0;
  }
  .start p {
    margin: 0;
  }
</style>
