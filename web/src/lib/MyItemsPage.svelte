<script lang="ts">
  import { onMount } from 'svelte';
  import { listAssignedIssues } from './my-items.ts';
  import type { IssueSummary } from './issues.ts';

  type View =
    | { type: 'loading' }
    | { type: 'ready'; items: IssueSummary[] }
    | { type: 'unavailable' };

  let view = $state<View>({ type: 'loading' });
  const controller = new AbortController();

  onMount(() => {
    void load();
    return () => controller.abort();
  });

  async function load(): Promise<void> {
    view = { type: 'loading' };
    try {
      const page = await listAssignedIssues(controller.signal);
      if (!controller.signal.aborted) {
        view = { type: 'ready', items: page.data };
      }
    } catch {
      if (!controller.signal.aborted) {
        view = { type: 'unavailable' };
      }
    }
  }
</script>

<main>
  <h1>Meus itens</h1>
  <p class="intro">
    Issues em que você é o responsável agora, nos projetos que ainda pode ler.
  </p>
  {#if view.type === 'loading'}
    <p role="status">Carregando itens…</p>
  {:else if view.type === 'unavailable'}
    <p role="status">Não foi possível carregar os seus itens.</p>
    <button type="button" onclick={() => void load()}>Tentar de novo</button>
  {:else if view.items.length === 0}
    <p role="status">Nenhum item está atribuído a você.</p>
  {:else}
    <ul>
      {#each view.items as issue (issue.id)}
        <li>
          <a
            href={`/projects/${issue.project_id}/issues/${issue.issue_number}`}
          >
            {issue.issue_key}
          </a>
          <span>{issue.title}</span>
          {#if issue.due_date !== undefined}
            <span>Prazo {issue.due_date}</span>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  ul {
    list-style: none;
    padding: 0;
    display: grid;
    gap: 12px;
  }
  li {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    align-items: center;
    min-height: 44px;
  }
  a {
    color: var(--primary);
  }
</style>
