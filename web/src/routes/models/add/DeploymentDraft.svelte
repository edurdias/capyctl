<script lang="ts">
  import { onDestroy } from 'svelte';
  import Refusal from '../../../components/Refusal.svelte';
  import YamlEditor from '../../../components/yaml-editor/YamlEditor.svelte';
  import { ApiError } from '../../../lib/api';
  import { checkDeployment, checkErrors, type CheckResult, type Fit } from '../../../lib/deployment-check';
  import { applyField, hasComments, parseDraft, readForm, RESIDENCIES, type DraftError, type Field } from '../../../lib/deployment-draft';
  import { lostResponse } from '../../../lib/operate';
  import { UNKNOWN_FIT_REASONS } from '../../../lib/vocabulary';

  // SPEC R20–R22 (plan U15): the guided form and the YAML view over one
  // deployment file, and the fit check. `oncheck` hands the parent the
  // check's answer for the current text, or null while there is none.
  let {
    text,
    ontext,
    oncheck,
    engines = [],
  }: {
    text: string;
    ontext: (text: string) => void;
    oncheck: (result: CheckResult | null) => void;
    engines?: string[];
  } = $props();

  let tab = $state<'form' | 'yaml'>('form');
  const draft = $derived(parseDraft(text));
  const invalid = $derived(draft.errors.length > 0);
  const form = $derived(readForm(draft.doc));

  let warnedComments = $state(false);
  let commentNotice = $state(false);

  function edit(field: Field, value: string): void {
    if (invalid) {
      return;
    }
    if (!warnedComments && hasComments(text)) {
      warnedComments = true;
      commentNotice = true;
    }
    // A fresh parse: the derived document belongs to the current text.
    ontext(applyField(parseDraft(text).doc, field, value, form));
  }

  // ADR 0040 §9: the check runs on the text, debounced; a newer text aborts
  // the older check.
  const DEBOUNCE_MS = 500;
  let result = $state<CheckResult | null>(null);
  let checkError = $state<ApiError | null>(null);
  let checking = $state(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let inFlight: AbortController | null = null;

  $effect(() => {
    const current = text;
    clearTimeout(timer);
    inFlight?.abort();
    inFlight = null;
    result = null;
    checkError = null;
    oncheck(null);
    if (current.trim() === '') {
      checking = false;
      return;
    }
    checking = true;
    timer = setTimeout(() => {
      const controller = new AbortController();
      inFlight = controller;
      checkDeployment(current, controller.signal).then(
        (answer) => {
          if (inFlight === controller && !controller.signal.aborted && current === text) {
            inFlight = null;
            checking = false;
            result = answer;
            oncheck(answer);
          }
        },
        (error: unknown) => {
          if (inFlight === controller && !controller.signal.aborted && current === text) {
            inFlight = null;
            checking = false;
            checkError = error instanceof ApiError ? error : lostResponse();
          }
        },
      );
    }, DEBOUNCE_MS);
  });

  onDestroy(() => {
    clearTimeout(timer);
    inFlight?.abort();
    inFlight = null;
  });

  // Errors in the text show at once; the server's own once it answers.
  // C1: HTTP 400 still carries errors[].position beside the error envelope.
  const serverErrors = $derived(checkErrors(checkError));
  const errors = $derived(serverErrors.length > 0 ? serverErrors : (invalid ? draft.errors : (result?.errors ?? [])));

  function where(error: DraftError): string {
    if (!error.position) {
      return error.path ? `${error.path}: ` : '';
    }
    return `Line ${error.position.line}, column ${error.position.column}: `;
  }

  function fitText(fit: Fit): string {
    switch (fit.verdict) {
      case 'fits':
        return 'It fits in memory.';
      case 'needs_room':
        return 'It fits, but starting it will switch other models out to make room.';
      case 'does_not_fit': {
        const limit = typeof fit.limit === 'object' && fit.limit !== null ? JSON.stringify(fit.limit) : fit.limit;
        return `It does not fit in memory: ${limit ?? 'a memory limit is too small'}. You can still add it and change it later.`;
      }
      default:
        return UNKNOWN_FIT_REASONS[fit.reason ?? ''] ?? fit.reason ?? 'CapyCTL cannot tell yet whether the model fits.';
    }
  }

  const ESTIMATED = 'This is an estimate from the size of the model’s files; CapyCTL measures it when the model first starts.';
  const RESIDENCY_LABELS: Record<string, string> = {
    '': 'Automatic: park the way the hardware allows',
    host_backed: 'Park in host memory',
    deep: 'Deep park: drop the weights',
    restart_only: 'Never park: stop instead',
  };
</script>

<div class="tabs" role="tablist">
  <button type="button" role="tab" aria-selected={tab === 'form'} onclick={() => (tab = 'form')}>Form</button>
  <button type="button" role="tab" aria-selected={tab === 'yaml'} onclick={() => (tab = 'yaml')}>YAML</button>
</div>

{#if tab === 'form'}
  {#if invalid}
    <p class="notice" role="status">The YAML has errors, so the form is read-only. Fix them in the YAML view.</p>
  {/if}
  {#if commentNotice}
    <p class="notice" role="status">Comments in the YAML may not be kept when you edit the form.</p>
  {/if}
  <fieldset disabled={invalid}>
    <label>
      Name
      <input name="name" value={form.name} oninput={(e) => edit('name', e.currentTarget.value)} placeholder="my-model" />
    </label>
    <label>
      Model source
      <select name="sourceKind" value={form.sourceKind} onchange={(e) => edit('sourceKind', e.currentTarget.value)}>
        <option value="hf">Hugging Face repository</option>
        <option value="path">Folder on the host</option>
      </select>
    </label>
    <label>
      {form.sourceKind === 'hf' ? 'Repository' : 'Folder'}
      <input
        name="source"
        value={form.source}
        oninput={(e) => edit('source', e.currentTarget.value)}
        placeholder={form.sourceKind === 'hf' ? 'Qwen/Qwen3-4B-Instruct-2507' : 'Qwen3-4B'}
      />
    </label>
    <label>
      Engine
      <input name="engine" list="engine-profiles" value={form.engine} oninput={(e) => edit('engine', e.currentTarget.value)} />
      <datalist id="engine-profiles">
        {#each engines as engine (engine)}
          <option value={engine}></option>
        {/each}
      </datalist>
    </label>
    <label>
      Host
      <input name="host" value={form.host} oninput={(e) => edit('host', e.currentTarget.value)} placeholder="chosen by CapyCTL" />
    </label>
    <label>
      GPUs
      <input name="devices" value={form.devices} oninput={(e) => edit('devices', e.currentTarget.value)} placeholder="chosen by CapyCTL, or gpu0, gpu1" />
    </label>
    <label>
      Instances
      <input name="instances" inputmode="numeric" value={form.instances} oninput={(e) => edit('instances', e.currentTarget.value)} placeholder="1" />
    </label>
    <label>
      Parking
      <select name="residency" value={form.residency} onchange={(e) => edit('residency', e.currentTarget.value)}>
        {#each RESIDENCIES as residency (residency)}
          <option value={residency}>{RESIDENCY_LABELS[residency]}</option>
        {/each}
        {#if !(RESIDENCIES as readonly string[]).includes(form.residency)}
          <option value={form.residency}>{form.residency}</option>
        {/if}
      </select>
    </label>
    <label>
      GPU memory for the model
      <input name="memoryRequest" value={form.memoryRequest} oninput={(e) => edit('memoryRequest', e.currentTarget.value)} placeholder="sized from the model, or 40GiB" />
    </label>
    <label>
      KV cache
      <input name="kvCache" value={form.kvCache} oninput={(e) => edit('kvCache', e.currentTarget.value)} placeholder="sized from the GPU, or 12GiB" />
    </label>
    <label>
      Context length (tokens)
      <input name="contextLength" inputmode="numeric" value={form.contextLength} oninput={(e) => edit('contextLength', e.currentTarget.value)} placeholder="the model’s own" />
    </label>
    <label>
      API
      <select name="api" value={form.api} onchange={(e) => edit('api', e.currentTarget.value)}>
        <option value="">Chat</option>
        <option value="decisions">Decisions</option>
      </select>
    </label>
  </fieldset>
{:else}
  <YamlEditor value={text} onchange={ontext} />
{/if}

<div class="check" aria-live="polite">
  {#if errors.length > 0}
    <ul class="errors">
      {#each errors as error, i (i)}
        <li>{where(error)}{error.message}</li>
      {/each}
    </ul>
  {:else if checkError}
    <Refusal error={checkError} />
  {:else if result}
    <p class="fit" data-verdict={result.verdict}>{fitText(result)}</p>
    {#if result.estimated}
      <p class="muted">{ESTIMATED}</p>
    {/if}
    {#if result.hosts.length > 0}
      <ul aria-label="Fit by host">
        {#each result.hosts as host (host.host_id)}
          <li data-host={host.host_id} data-verdict={host.verdict}>
            <strong>{host.host_id}:</strong> {fitText(host)}
            {#if host.estimated}
              <span class="muted">{ESTIMATED}</span>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  {:else if checking}
    <p class="muted">Checking whether it fits…</p>
  {/if}
</div>

<style>
  .tabs {
    display: flex;
    gap: 0.5rem;
    margin-bottom: 1rem;
  }
  .tabs [aria-selected='true'] {
    font-weight: 600;
    border-bottom: 2px solid var(--capyctl-accent);
  }
  fieldset {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(16rem, 1fr));
    gap: 0.75rem 1rem;
    border: none;
    padding: 0;
    margin: 0;
  }
  label {
    display: grid;
    gap: 0.25rem;
  }
  .notice {
    border-left: 3px solid var(--capyctl-accent);
    padding-left: 0.75rem;
  }
  .check {
    margin-top: 1rem;
  }
  .errors {
    font-family: var(--capyctl-font-mono);
    font-size: 0.875rem;
  }
  .fit {
    font-weight: 600;
  }
</style>
