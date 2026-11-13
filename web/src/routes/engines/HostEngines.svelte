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
  let detecting = $state(false);
  let customDialog: HTMLDialogElement;

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

<article class="host panel">
  <div class="page-title"><h2>{host.name} <span class="chip">{host.online ? 'online' : 'offline'}</span></h2><div class="tools"><button type="button" aria-expanded={detecting} onclick={() => detecting = !detecting}>Detect engines</button><button type="button" onclick={() => customDialog.showModal()}>Add custom engine…</button></div></div>
  {#if engines.length === 0}<p class="muted">No engines are installed on this host.</p>{:else}
    <div class="table-scroll"><table><thead><tr><th>Profile</th><th>Engine</th><th>Version</th><th>Models using it</th><th>Status</th><th><span class="visually-hidden">Actions</span></th></tr></thead>
      <tbody>{#each engines as engine (engine.profile)}
        <tr><td><strong>{engine.profile}</strong></td><td>{engine.engine}{engine.custom ? ' · custom' : ''}</td><td>{engine.version}</td><td><span title={engine.deployments.join(', ')}>{engine.deployments.length} {engine.deployments.length === 1 ? 'model' : 'models'}</span>{#if engine.deployments.length}<small class="used-by">{engine.deployments.join(', ')}</small>{/if}</td><td><span class="chip" class:online={host.online && !engine.retiring}>{engine.retiring ? 'being removed' : host.online ? 'available' : 'offline'}</span></td><td>
          {#if !engine.retiring}<details class="overflow"><summary aria-label={`Actions for ${engine.profile}`}>⋯</summary><div class="menu-items"><button type="button" class="danger" disabled={busy} onclick={() => void remove(engine.profile)}>Remove</button></div></details>{/if}
        </td></tr>
      {/each}</tbody></table></div>
  {/if}

  {#if error}
    <Refusal {error} />
  {/if}

  {#if detecting}<section class="detection"><h3>Detect engines</h3>
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

  </section>{/if}
  <!-- ADR 0040 §11 and owner decision 2026-11-13: keep preview and typed
       confirmation, only move the custom flow behind a modal dialog. -->
  <dialog bind:this={customDialog} aria-labelledby={`custom-title-${host.host_id}`}>
    <div class="page-title"><h2 id={`custom-title-${host.host_id}`}>Custom engine · {host.name}</h2><button type="button" onclick={() => customDialog.close()}>Close</button></div>
    <CustomEngine host={host.host_id} {onchange} />
  </dialog>
</article>

<style>
  .host { margin-bottom: 1.5rem; }
  .host h2 { margin: 0; display: flex; align-items: center; gap: 0.75rem; }
  .tools { display: flex; gap: 0.5rem; flex-wrap: wrap; }
  .online { color: var(--state-ready); }
  .danger { color: var(--capyctl-danger); }
  .used-by { display: block; font-size: 0.75rem; color: var(--capyctl-muted); }
  .detection { margin-top: 1.5rem; border-top: 1px solid var(--capyctl-rule); padding-top: 1rem; }
  td:last-child { width: 4rem; }
  td:has(.overflow[open]) { position: relative; }
  .table-scroll:has(.overflow[open]) { min-height: 12rem; }
  dialog { width: 40rem; }
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
