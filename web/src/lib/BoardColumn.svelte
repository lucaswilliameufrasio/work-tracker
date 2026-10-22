<script lang="ts">
  import type { BoardPage } from './board';
  import type { ProjectStatus } from './project-catalogs';
  let { status, page }: { status: ProjectStatus; page: BoardPage } = $props();
</script>

<section class="column" data-board-column={status.id}>
  <h2>{status.name}</h2>
  <p class="count">
    {page.total_count}
    {page.total_count === '1'
      ? 'issue fora da lixeira'
      : 'issues fora da lixeira'}
  </p>
  {#if page.data.length === 0}<p>Nenhum card nesta página.</p>{/if}
  <ul aria-label={`Cards de ${status.name}`}>
    {#each page.data as issue (issue.id)}
      <li data-board-card={issue.id}>
        <a href={`/projects/${issue.project_id}/issues/${issue.issue_number}`}
          ><span>{issue.issue_key}</span><strong>{issue.title}</strong></a
        >
      </li>
    {/each}
  </ul>
  {#if page.has_more}<a
      class="continue"
      href={`/projects/${status.project_id}/board/${status.id}?cursor=${page.next_cursor}`}
      >Continuar nesta coluna</a
    >{/if}
</section>

<style>
  .column {
    min-width: 0;
    border-top: 3px solid var(--primary);
    padding: 20px 0;
  }
  h2 {
    font-size: 18px;
    margin: 0 0 8px;
    overflow-wrap: anywhere;
  }
  .count {
    color: var(--muted);
    font-size: 14px;
    margin: 0 0 16px;
  }
  ul {
    list-style: none;
    padding: 0;
    margin: 0;
  }
  li {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    margin: 10px 0;
  }
  a {
    color: var(--primary);
    display: grid;
    min-height: 44px;
    gap: 8px;
    padding: 14px;
    text-decoration: none;
    overflow-wrap: anywhere;
  }
  a:hover strong {
    text-decoration: underline;
  }
  a:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 4px;
  }
  a span {
    font-size: 13px;
    color: var(--muted);
  }
  strong {
    font-size: 15px;
    font-weight: 500;
    line-height: 1.5;
  }
  .continue {
    padding: 12px 0;
    text-decoration: underline;
    font-size: 14px;
  }
</style>
