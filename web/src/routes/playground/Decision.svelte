<script lang="ts">
  import { onDestroy, type Snippet } from 'svelte';
  import Refusal from '../../components/Refusal.svelte';
  import { ApiError } from '../../lib/api';
  import { decide, playgroundError, type DecisionResult, type QuestionType } from '../../lib/playground';
  import { snapshot } from '../../lib/state';
  import type { DeploymentSnapshot } from '../../lib/types';
  import { stateWord } from '../../lib/vocabulary';
  import StartModel from './StartModel.svelte';
  import { autoGrow, followContent } from './interactions';

  // SPEC R31: a typed question to a decision model, answered with the
  // probability of each option.
  let { deployment, heading }: { deployment: DeploymentSnapshot; heading: Snippet } = $props();

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

<header class="playground-header">{@render heading()}</header>
<div class="playground-scroll" use:followContent={result}>
{#if stopped}
  <StartModel deployment={live} onstarted={() => (offerStart = false)} />
{/if}

<form id="decision-question" class="decision-fields" onsubmit={ask}>
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
  <p class="muted">Ask a typed question to see the probability of each option. CapyCTL stores no prompts or answers.</p>
</form>

{#if error}
  <Refusal {error} />
{/if}

<div class="decision-results" aria-live="polite">
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
</div>
</div>
<div class="playground-composer decision-composer">
  <label class="visually-hidden" for="decision-input">Input</label>
  <textarea id="decision-input" form="decision-question" bind:value={input} use:autoGrow={input} rows="1" placeholder="Ask a decision question"></textarea>
  <button form="decision-question" type="submit" class="primary" disabled={outstanding}>{outstanding ? 'Asking…' : 'Ask'}</button>
</div>

<style>
  th {
    text-align: left;
    font-weight: normal;
    padding-right: 1rem;
  }
</style>
