<script lang="ts">
  import Refusal from '../../components/Refusal.svelte';
  import type { ApiError } from '../../lib/api';
  import { addEngine, detectEngines, removeEngine, type Candidate } from '../../lib/host-admin';
  import type { EngineRow, HostRow } from '../../lib/reads';
  import CustomEngine from './CustomEngine.svelte';

  // SPEC R24 (ADR 0040 §10): one host's engines.
  let { host, engines, onchange }: { host: HostRow; engines: EngineRow[]; onchange: () => void } = $props();

  let error = $state<ApiError | null>(null);
  let busy = $state(false);

  async function run(action: () => Promise<unknown>): Promise<boolean> {
    busy = true;
    error = null;
    try {
      await action();
      return true;
    } catch (failure) {
      error = failure as ApiError;
      return false;
    } finally {
      busy = false;
    }
  }

  // `engine detect --path`: extra directories, one per line or comma.
  let searchPaths = $state('');
  let candidates = $state<Candidate[] | null>(null);
  let names = $state<Record<string, string>>({});

  async function detect(): Promise<void> {
    const paths = searchPaths.split(/[\n,]/).map((path) => path.trim()).filter((path) => path !== '');
    await run(async () => {
      candidates = await detectEngines(host.host_id, paths);
    });
  }

  async function add(candidate: Candidate): Promise<void> {
    if (await run(() => addEngine(host.host_id, candidate, (names[candidate.entry] ?? '').trim()))) {
      onchange();
    }
  }

  async function remove(profile: string): Promise<void> {
    // Answers 202; the list shows the profile as being removed until it goes.
    if (await run(() => removeEngine(host.host_id, profile))) {
      onchange();
    }
  }
</script>

<article class="host">
  <h2>{host.name}</h2>
  {#if engines.length === 0}
    <p class="muted">No engines are installed on this host.</p>
  {:else}
    <ul>
      {#each engines as engine (engine.profile)}
        <li>
          <span>{engine.profile}: {engine.engine} {engine.version}{engine.custom ? ' (custom)' : ''}</span>
          {#if engine.retiring}
            <span class="muted">being removed</span>
          {:else}
            <button type="button" disabled={busy} onclick={() => void remove(engine.profile)}>Remove</button>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}

  {#if error}
    <Refusal {error} />
  {/if}

  <h3>Detect engines</h3>
  <p class="muted">Detection runs on the host as the CapyCTL service user, so it finds only what that user can read.</p>
  <label>
    Extra directories to search (optional, one per line)
    <textarea name="search-paths" rows="2" bind:value={searchPaths}></textarea>
  </label>
  <button type="button" disabled={busy} onclick={() => void detect()}>Detect</button>
  {#if candidates !== null}
    {#if candidates.length === 0}
      <p class="muted">No engine installation was found.</p>
    {:else}
      <ul>
        {#each candidates as candidate (candidate.entry)}
          <li>
            <span>{candidate.engine} {candidate.version ?? ''} at <code>{candidate.entry}</code></span>
            {#if candidate.custom}
              <span class="muted">Add it as a custom engine below.</span>
            {:else}
              <input aria-label="Profile name" placeholder="profile name (optional)" bind:value={names[candidate.entry]} />
              <button type="button" disabled={busy} onclick={() => void add(candidate)}>Add</button>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  {/if}

  <h3>Custom engine</h3>
  <CustomEngine host={host.host_id} {onchange} />
</article>

<style>
  .host {
    margin-bottom: 2rem;
  }
  li {
    display: flex;
    gap: 0.75rem;
    align-items: center;
    margin: 0.375rem 0;
  }
  label {
    display: grid;
    gap: 0.25rem;
    margin-bottom: 0.5rem;
  }
</style>
