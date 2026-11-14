<script lang="ts">
  import { onDestroy } from 'svelte';
  import Refusal from '../../components/Refusal.svelte';
  import { ApiError } from '../../lib/api';
  import { decide, playgroundError, type DecisionResult, type QuestionType } from '../../lib/playground';
  import { snapshot } from '../../lib/state';
  import type { DeploymentSnapshot } from '../../lib/types';
  import { stateWord } from '../../lib/vocabulary';
  import StartModel from './StartModel.svelte';

  // SPEC R31: a typed question to a decision model, answered with the
  // probability of each option.
  let { deployment }: { deployment: DeploymentSnapshot } = $props();

  const live = $derived($snapshot?.deployments.find((d) => d.id === deployment.id) ?? deployment);

  let input = $state('');
  let type = $state<QuestionType>('choice');
  let instructions = $state('');
  let options = $state('');
  let outstanding = $state(false);
  let result = $state<DecisionResult | null>(null);
  let error = $state<ApiError | null>(null);
  let offerStart = $state(false);

  // SPEC R31: leaving the playground cancels its outstanding inference.
  const controller = new AbortController();
  onDestroy(() => controller.abort());

  const stopped = $derived(offerStart || stateWord(live.observed_state, live.suspended) === 'stopped');

  async function ask(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (outstanding) {
      return;
    }
    outstanding = true;
    error = null;
    offerStart = false;
    result = null;
    try {
      result = await decide(deployment.name, input, {
        type,
        instructions,
        options: options
          .split('\n')
          .map((line) => line.trim())
          .filter((line) => line.length > 0),
      }, controller.signal);
    } catch (caught) {
      if (controller.signal.aborted) {
        return;
      }
      error = playgroundError(caught);
      offerStart = error.code === 'deployment_stopped';
    } finally {
      outstanding = false;
    }
  }
</script>

{#if stopped}
  <StartModel deployment={live} onstarted={() => (offerStart = false)} />
{/if}

<form onsubmit={ask}>
  <label>
    Input
    <textarea bind:value={input} rows="4"></textarea>
  </label>
  <label>
    Question type
    <select bind:value={type}>
      <option value="choice">Choice: one of named options</option>
      <option value="noul">Yes or no</option>
      <option value="score">Score: an ordered scale</option>
    </select>
  </label>
  <label>
    Instructions
    <input bind:value={instructions} />
  </label>
  {#if type !== 'noul'}
    <label>
      {type === 'choice' ? 'Options, one per line' : 'Levels from low to high, one per line'}
      <textarea bind:value={options} rows="3"></textarea>
    </label>
  {/if}
  <div class="buttons">
    <button type="submit" class="primary" disabled={outstanding}>{outstanding ? 'Asking…' : 'Ask'}</button>
  </div>
</form>

{#if error}
  <Refusal {error} />
{/if}

{#if result}
  {#each Object.entries(result.answers) as [id, answer] (id)}
    <div class="answer">
      <p>Answer: <strong>{answer.value}</strong></p>
      <table>
        <tbody>
          {#each answer.probabilities as row (row.label)}
            <tr>
              <th scope="row">{row.label}</th>
              <td>{(row.probability * 100).toFixed(1)}%</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {/each}
  <p class="muted">Answered in {(result.total_ms / 1000).toFixed(2)} s</p>
{/if}

<style>
  form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    margin: 1rem 0;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }
  textarea,
  input,
  select {
    font: inherit;
    padding: 0.5rem;
    border-radius: 6px;
    border: 1px solid var(--capyctl-rule);
    background: var(--capyctl-bg);
    color: var(--capyctl-text);
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
  }
  th {
    text-align: left;
    font-weight: normal;
    padding-right: 1rem;
  }
</style>
