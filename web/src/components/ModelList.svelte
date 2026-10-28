<script lang="ts">
  import { href, link } from '../lib/router';
  import type { LoadReport } from '../lib/reads';
  import type { DeploymentSnapshot } from '../lib/types';
  import ActionBar from './ActionBar.svelte';
  import StateBadge from './StateBadge.svelte';

  // SPEC R10, R36: one row per model with its state, instances, requests in
  // flight and actions. Below 768 px each row is a card and its actions are
  // full-width buttons.
  let { deployments, load }: { deployments: DeploymentSnapshot[]; load: LoadReport | null } = $props();

  function inFlight(id: string): string {
    const row = load?.deployments.find((d) => d.deployment_id === id);
    return row === undefined ? '–' : String(row.router.in_flight);
  }
</script>

<div class="models" role="table" aria-label="Models">
  <div class="row head" role="row">
    <span role="columnheader">Model</span>
    <span role="columnheader">State</span>
    <span role="columnheader">Instances ready</span>
    <span role="columnheader">In flight</span>
    <span role="columnheader">Actions</span>
  </div>
  {#each deployments as deployment (deployment.id)}
    <div class="row model" role="row" data-model={deployment.name}>
      <span role="cell" class="name">
        <a href={href(`/models/${deployment.id}`)} use:link>{deployment.name}</a>
      </span>
      <span role="cell"><StateBadge state={deployment.observed_state} suspended={deployment.suspended} /></span>
      <span role="cell"><span class="label">Instances ready </span>{deployment.ready_instances} of {deployment.desired_instances}</span>
      <span role="cell"><span class="label">In flight </span>{inFlight(deployment.id)}</span>
      <span role="cell" class="cell-actions"><ActionBar {deployment} /></span>
    </div>
  {/each}
</div>

<style>
  .row {
    display: grid;
    grid-template-columns: minmax(8rem, 1.5fr) 8rem 8rem 6rem minmax(12rem, 3fr);
    gap: 0.75rem;
    align-items: start;
    padding: 0.75rem 0;
    border-bottom: 1px solid var(--capyctl-rule);
  }
  .head {
    font-size: 0.875rem;
    color: var(--capyctl-muted);
  }
  .name {
    font-weight: 600;
    overflow-wrap: anywhere;
  }
  .label {
    display: none;
  }
  @media (max-width: 767.98px) {
    .head {
      display: none;
    }
    .row {
      grid-template-columns: 1fr;
      gap: 0.5rem;
      padding: 1rem;
      margin-bottom: 0.75rem;
      border: 1px solid var(--capyctl-rule);
      border-radius: 8px;
      background: var(--capyctl-surface);
    }
    .label {
      display: inline;
      color: var(--capyctl-muted);
    }
  }
</style>
