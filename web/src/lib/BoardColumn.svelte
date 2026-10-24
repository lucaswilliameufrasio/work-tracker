<script lang="ts">
  import type { BoardPage } from './board';
  import type { BoardCard } from './board';
  import type { ProjectStatus } from './project-catalogs';
  type MoveIssue = (issue: BoardCard, targetStatusId: string) => void;
  let {
    status,
    page,
    canMove = false,
    moveStatuses = [],
    moveBusy = false,
    onMove = () => undefined,
  }: {
    status: ProjectStatus;
    page: BoardPage;
    canMove?: boolean;
    moveStatuses?: ProjectStatus[];
    moveBusy?: boolean;
    onMove?: MoveIssue;
  } = $props();
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
        {#if issue.due_date !== undefined || issue.is_overdue}<p class="hint">
            {issue.due_date === undefined
              ? ''
              : `Prazo ${issue.due_date}`}{issue.is_overdue
              ? ' · Atrasada'
              : ''}
          </p>{/if}
        {#if canMove && moveStatuses.length > 0}<details class="move-control">
            <summary>Movimentar</summary>
            <form
              onsubmit={(event) => {
                event.preventDefault();
                const targetSelect = event.currentTarget.querySelector(
                  'select[name="target_status_id"]',
                );
                const targetStatusId =
                  targetSelect !== null &&
                  'value' in targetSelect &&
                  typeof targetSelect.value === 'string'
                    ? targetSelect.value
                    : undefined;
                if (targetStatusId !== undefined) {
                  onMove(issue, targetStatusId);
                }
              }}
            >
              <label for={`move-status-${issue.id}`}>Coluna de destino</label>
              <select
                id={`move-status-${issue.id}`}
                name="target_status_id"
                disabled={moveBusy}
              >
                {#each moveStatuses as destination (destination.id)}<option
                    value={destination.id}
                    selected={destination.id === status.id}
                    >{destination.name}</option
                  >{/each}
              </select>
              <button type="submit" disabled={moveBusy}
                >Mover para o topo</button
              >
            </form>
          </details>{/if}
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
  .hint {
    margin: 0;
    padding: 0 14px 12px;
    color: var(--muted);
    font-size: 14px;
  }
  .move-control {
    padding: 0 14px 14px;
  }
  .move-control summary {
    min-height: 44px;
    display: flex;
    align-items: center;
    color: var(--primary);
    cursor: pointer;
    text-decoration: underline;
  }
  .move-control form {
    display: grid;
    gap: 8px;
  }
  .move-control select,
  .move-control button {
    min-height: 44px;
  }
  .move-control button {
    color: var(--primary);
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 6px;
    cursor: pointer;
    font: inherit;
  }
  .move-control button:focus-visible,
  .move-control select:focus-visible,
  .move-control summary:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 3px;
  }
  .move-control button:disabled,
  .move-control select:disabled {
    cursor: not-allowed;
    opacity: 0.65;
  }
</style>
