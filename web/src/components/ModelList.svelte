<script lang="ts">
  import { onMount } from 'svelte';
  import { readEngines, type EngineRow, type LoadReport } from '../lib/reads';
  import { livePoll } from '../lib/poll';
  import type { DeploymentSnapshot } from '../lib/types';
  import ModelCard from './ModelCard.svelte';
  let { deployments, load }: { deployments: DeploymentSnapshot[]; load: LoadReport | null } = $props();
  let engines = $state<EngineRow[]>([]);
  onMount(() => livePoll(async () => { engines = await readEngines(); }, 5000));
</script>
<div class="models">
  {#each deployments as deployment (deployment.id)}<ModelCard {deployment} {engines} load={load?.deployments.find((d) => d.deployment_id === deployment.id) ?? null} />{/each}
</div>
<style>.models { display: grid; gap: 0.75rem; }</style>
