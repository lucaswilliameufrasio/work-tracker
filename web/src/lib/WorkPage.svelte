<script lang="ts">
  import { flushSync, onMount, tick } from 'svelte';
  import { SvelteURLSearchParams } from 'svelte/reactivity';
  import BoardColumn from './BoardColumn.svelte';
  import { IssueRequestError } from './issue-transport.ts';
  import {
    WorkScreen,
    workRoute,
    type WorkData,
    type WorkState,
  } from './issue-navigation';
  import {
    IssueMoveIdentityChanged,
    moveIssue,
    type IssueMoveInput,
    type IssueMoveTarget,
  } from './issue-move.ts';
  import type { BoardCard } from './board.ts';
  import IssueAssigneeEditor from './IssueAssigneeEditor.svelte';
  import { onSessionEnded } from './session-lifecycle.ts';
  import {
    IssuePriorityIdentityChanged,
    setIssuePriority,
    type IssuePriorityInput,
    type IssuePriorityTarget,
  } from './issue-priority.ts';
  import {
    IssueDueDateIdentityChanged,
    setIssueDueDate,
    type IssueDueDateInput,
    type IssueDueDateTarget,
  } from './issue-due-date.ts';
  import { validateIssueSearchText, type IssuePriority } from './issues.ts';

  const route = workRoute(
    globalThis.location.pathname,
    globalThis.location.search,
  );
  type BoardWorkData = Extract<WorkData, { type: 'board' }>;
  type MoveCommand = {
    projectId: string;
    actorId: string;
    issue: IssueMoveTarget & { issue_key: string };
    idempotencyKey: string;
    input: IssueMoveInput;
  };
  type MoveState =
    | { type: 'idle' }
    | { type: 'pending'; command: MoveCommand }
    | { type: 'uncertain'; command: MoveCommand }
    | { type: 'confirmed'; issueKey: string }
    | { type: 'conflict' }
    | { type: 'error' };
  type PriorityCommand = {
    projectId: string;
    actorId: string;
    issue: IssuePriorityTarget;
    idempotencyKey: string;
    input: IssuePriorityInput;
  };
  type PriorityState =
    | { type: 'idle' }
    | { type: 'pending'; command: PriorityCommand }
    | { type: 'uncertain'; command: PriorityCommand }
    | { type: 'confirmed'; priority: IssuePriority }
    | { type: 'conflict' }
    | { type: 'error' };
  type DueDateCommand = {
    projectId: string;
    actorId: string;
    issue: IssueDueDateTarget;
    idempotencyKey: string;
    input: IssueDueDateInput;
  };
  type DueDateState =
    | { type: 'idle' }
    | { type: 'pending'; command: DueDateCommand }
    | { type: 'uncertain'; command: DueDateCommand }
    | { type: 'confirmed'; dueDate: string | null }
    | { type: 'conflict' }
    | { type: 'error' };
  type AssigneeNotice =
    | {
        type: 'confirmed';
        issueId: string;
        assigneeUserId: string | null;
      }
    | { type: 'conflict'; issueId: string };
  let view = $state<WorkState>({ type: 'idle' });
  let moveState = $state<MoveState>({ type: 'idle' });
  let priorityState = $state<PriorityState>({ type: 'idle' });
  let dueDateState = $state<DueDateState>({ type: 'idle' });
  let assigneeNotice = $state<AssigneeNotice | undefined>(undefined);
  let searchDraft = $state(route.type === 'search' ? (route.query ?? '') : '');
  let selectedSearchPriority = $state<IssuePriority | ''>(
    route.type === 'search' ? (route.priority ?? '') : '',
  );
  let selectedPriority = $state<IssuePriority>('none');
  let selectedDueDate = $state('');
  let selectedPriorityIssueId = '';
  let selectedDueDateIssueId = '';
  let moveOperation = new AbortController();
  let moveGeneration = 0;
  let priorityOperation = new AbortController();
  let priorityGeneration = 0;
  let dueDateOperation = new AbortController();
  let dueDateGeneration = 0;
  let heading: HTMLElement | undefined;
  const screen = new WorkScreen(route, (state) => {
    view = state;
  });
  const priorities = {
    none: 'Sem prioridade',
    low: 'Baixa',
    medium: 'Média',
    high: 'Alta',
    urgent: 'Urgente',
  };
  const roles = { manager: 'Gerente', member: 'Membro', viewer: 'Leitor' };
  const eventLabels = {
    'issue.created': 'Issue criada',
    'issue.title_updated': 'Título alterado',
    'issue.description_updated': 'Descrição alterada',
    'issue.text_updated': 'Título e descrição alterados',
    'issue.moved': 'Issue movida',
    'issue.field_updated': 'Campo estruturado alterado',
  };
  const date = (value: string) =>
    `${new Intl.DateTimeFormat('pt-BR', { dateStyle: 'medium', timeStyle: 'short', timeZone: 'UTC' }).format(new Date(value))} UTC`;

  $effect(() => {
    if (view.type === 'ready' && view.data.type === 'detail') {
      if (selectedPriorityIssueId !== view.data.issue.id) {
        selectedPriorityIssueId = view.data.issue.id;
        selectedPriority = view.data.issue.priority;
      }
      if (selectedDueDateIssueId !== view.data.issue.id) {
        selectedDueDateIssueId = view.data.issue.id;
        selectedDueDate = view.data.issue.due_date ?? '';
        dueDateState = { type: 'idle' };
      }
    }
  });

  function projectLink(cursor?: string): string {
    const query = new SvelteURLSearchParams({
      include_archived: String(
        route.type === 'projects' && route.includeArchived,
      ),
    });
    if (cursor !== undefined) {
      query.set('cursor', cursor);
    }
    return `/projects?${query}`;
  }

  function listLink(project: string, cursor?: string): string {
    return `/projects/${project}/issues${cursor === undefined ? '' : `?cursor=${cursor}`}`;
  }

  function searchLink(
    project: string,
    query: string | undefined,
    priority: IssuePriority | undefined,
    cursor?: string,
  ): string {
    const params = new SvelteURLSearchParams();
    if (query !== undefined) {
      validateIssueSearchText(query);
      params.set('q', query);
    }
    if (priority !== undefined) {
      params.set('priority', priority);
    }
    if (cursor !== undefined) {
      params.set('cursor', cursor);
    }
    return `/projects/${project}/issues?${params}`;
  }

  function issueCollectionPath(): string {
    if (route.type === 'issues' || route.type === 'search') {
      return `/projects/${route.projectId}/issues`;
    }
    return '/projects';
  }

  function detailLink(
    project: string,
    number: string,
    history?: string,
  ): string {
    const query = new SvelteURLSearchParams();
    const fromSearch =
      route.type === 'search'
        ? {
            query: route.query,
            priority: route.priority,
            cursor: route.cursor,
          }
        : route.type === 'detail' &&
            (route.fromSearchQuery !== undefined ||
              route.fromSearchPriority !== undefined)
          ? {
              query: route.fromSearchQuery,
              priority: route.fromSearchPriority,
              cursor: route.fromSearchCursor,
            }
          : undefined;
    const from =
      route.type === 'issues'
        ? route.cursor
        : route.type === 'detail'
          ? route.from
          : undefined;
    if (fromSearch !== undefined) {
      if (fromSearch.query !== undefined) {
        query.set('from_q', fromSearch.query);
      }
      if (fromSearch.priority !== undefined) {
        query.set('from_priority', fromSearch.priority);
      }
      if (fromSearch.cursor !== undefined) {
        query.set('from_cursor', fromSearch.cursor);
      }
    } else if (from !== undefined) {
      query.set('from', from);
    }
    if (history !== undefined) {
      query.set('history_cursor', history);
    }
    return `/projects/${project}/issues/${number}${query.size === 0 ? '' : `?${query}`}`;
  }

  async function refresh(preserveAssigneeNotice = false): Promise<void> {
    if (!preserveAssigneeNotice) {
      assigneeNotice = undefined;
    }
    await screen.refresh();
    await tick();
    heading?.focus();
  }

  async function sendMove(command: MoveCommand): Promise<void> {
    if (moveState.type === 'pending') {
      return;
    }
    const generation = ++moveGeneration;
    moveState = { type: 'pending', command };
    try {
      const result = await moveIssue(
        command.projectId,
        command.issue,
        command.actorId,
        command.idempotencyKey,
        command.input,
        moveOperation.signal,
      );
      moveState = { type: 'confirmed', issueKey: result.receipt.issue_key };
      await screen.refresh();
    } catch (error) {
      if (generation !== moveGeneration) {
        return;
      }
      if (error instanceof IssueMoveIdentityChanged) {
        moveState = { type: 'idle' };
        screen.suspend();
        if (document.visibilityState === 'visible') {
          void screen.start();
        }
        return;
      }
      if (error instanceof IssueRequestError) {
        if (error.status === 401) {
          moveState = { type: 'idle' };
          return;
        }
        if (error.status === 409) {
          moveState = { type: 'conflict' };
          await screen.refresh();
          return;
        }
        if (error.status >= 500) {
          moveState = { type: 'uncertain', command };
          return;
        }
        moveState = { type: 'error' };
        if (error.status === 403 || error.status === 404) {
          await screen.refresh();
        }
        return;
      }
      moveState = { type: 'uncertain', command };
    }
  }

  function moveToColumn(
    board: BoardWorkData,
    issue: BoardCard,
    targetStatusId: string,
  ): void {
    if (
      !board.canMoveIssues ||
      moveState.type === 'pending' ||
      moveState.type === 'uncertain'
    ) {
      return;
    }
    const targetColumn = board.columns.find(
      (column) => column.status.id === targetStatusId,
    );
    if (targetColumn === undefined) {
      moveState = { type: 'error' };
      return;
    }
    const firstRemaining = targetColumn.page.data.find(
      (candidate) => candidate.id !== issue.id,
    );
    if (
      targetColumn.page.total_count !== '0' &&
      firstRemaining === undefined &&
      !(
        targetColumn.page.total_count === '1' &&
        targetColumn.page.data[0]?.id === issue.id
      )
    ) {
      moveState = { type: 'conflict' };
      void screen.refresh();
      return;
    }
    const command: MoveCommand = {
      projectId: board.projectId,
      actorId: board.actorId,
      issue: {
        id: issue.id,
        issue_number: issue.issue_number,
        issue_key: issue.issue_key,
      },
      idempotencyKey: globalThis.crypto.randomUUID(),
      input: {
        expected_version: issue.version,
        target_status_id: targetStatusId,
        ...(firstRemaining === undefined
          ? {}
          : { next_issue_id: firstRemaining.id }),
      },
    };
    void sendMove(command);
  }

  function replayMove(): void {
    if (moveState.type === 'uncertain') {
      void sendMove(moveState.command);
    }
  }

  async function sendPriority(command: PriorityCommand): Promise<void> {
    if (priorityState.type === 'pending') {
      return;
    }
    const generation = ++priorityGeneration;
    priorityState = { type: 'pending', command };
    try {
      const result = await setIssuePriority(
        command.projectId,
        command.issue,
        command.actorId,
        command.idempotencyKey,
        command.input,
        priorityOperation.signal,
      );
      if (generation !== priorityGeneration) {
        return;
      }
      selectedPriority = result.receipt.priority;
      priorityState = {
        type: 'confirmed',
        priority: result.receipt.priority,
      };
      await screen.refresh();
    } catch (error) {
      if (generation !== priorityGeneration) {
        return;
      }
      if (error instanceof IssuePriorityIdentityChanged) {
        priorityState = { type: 'idle' };
        screen.suspend();
        if (document.visibilityState === 'visible') {
          void screen.start();
        }
        return;
      }
      if (error instanceof IssueRequestError) {
        if (error.status === 401) {
          priorityState = { type: 'idle' };
          return;
        }
        if (error.status === 409) {
          priorityState = { type: 'conflict' };
          await screen.refresh();
          return;
        }
        if (error.status >= 500) {
          priorityState = { type: 'uncertain', command };
          return;
        }
        priorityState = { type: 'error' };
        if (error.status === 403 || error.status === 404) {
          await screen.refresh();
        }
        return;
      }
      priorityState = { type: 'uncertain', command };
    }
  }

  function savePriority(): void {
    if (
      view.type !== 'ready' ||
      view.data.type !== 'detail' ||
      !view.data.canUpdateIssueFields ||
      view.data.issue.trashed_at !== undefined ||
      priorityState.type === 'pending' ||
      priorityState.type === 'uncertain'
    ) {
      return;
    }
    void sendPriority({
      projectId: view.data.issue.project_id,
      actorId: view.data.actorId,
      issue: {
        id: view.data.issue.id,
        issue_number: view.data.issue.issue_number,
        issue_key: view.data.issue.issue_key,
      },
      idempotencyKey: globalThis.crypto.randomUUID(),
      input: {
        expected_version: view.data.issue.version,
        priority: selectedPriority,
      },
    });
  }

  function replayPriority(): void {
    if (priorityState.type === 'uncertain') {
      void sendPriority(priorityState.command);
    }
  }

  async function sendDueDate(command: DueDateCommand): Promise<void> {
    if (dueDateState.type === 'pending') {
      return;
    }
    const generation = ++dueDateGeneration;
    dueDateState = { type: 'pending', command };
    try {
      const result = await setIssueDueDate(
        command.projectId,
        command.issue,
        command.actorId,
        command.idempotencyKey,
        command.input,
        dueDateOperation.signal,
      );
      if (generation !== dueDateGeneration) {
        return;
      }
      selectedDueDate = result.receipt.due_date ?? '';
      dueDateState = {
        type: 'confirmed',
        dueDate: result.receipt.due_date,
      };
      await screen.refresh();
    } catch (error) {
      if (generation !== dueDateGeneration) {
        return;
      }
      if (error instanceof IssueDueDateIdentityChanged) {
        selectedDueDate = '';
        selectedDueDateIssueId = '';
        dueDateState = { type: 'idle' };
        screen.suspend();
        if (document.visibilityState === 'visible') {
          void screen.start();
        }
        return;
      }
      if (error instanceof IssueRequestError) {
        if (error.status === 401) {
          selectedDueDate = '';
          selectedDueDateIssueId = '';
          dueDateState = { type: 'idle' };
          return;
        }
        if (error.status === 409) {
          dueDateState = { type: 'conflict' };
          await screen.refresh();
          return;
        }
        if (error.status >= 500) {
          dueDateState = { type: 'uncertain', command };
          return;
        }
        dueDateState = { type: 'error' };
        if (error.status === 403 || error.status === 404) {
          await screen.refresh();
          if (view.type === 'ready' && view.data.type === 'detail') {
            selectedDueDate = view.data.issue.due_date ?? '';
          } else {
            selectedDueDate = '';
          }
        }
        return;
      }
      dueDateState = { type: 'uncertain', command };
    }
  }

  function saveDueDate(value: string | null = selectedDueDate || null): void {
    if (
      view.type !== 'ready' ||
      view.data.type !== 'detail' ||
      !view.data.canUpdateIssueFields ||
      view.data.issue.trashed_at !== undefined ||
      dueDateState.type === 'pending' ||
      dueDateState.type === 'uncertain'
    ) {
      return;
    }
    void sendDueDate({
      projectId: view.data.issue.project_id,
      actorId: view.data.actorId,
      issue: {
        id: view.data.issue.id,
        issue_number: view.data.issue.issue_number,
        issue_key: view.data.issue.issue_key,
      },
      idempotencyKey: globalThis.crypto.randomUUID(),
      input: {
        expected_version: view.data.issue.version,
        due_date: value,
      },
    });
  }

  function replayDueDate(): void {
    if (dueDateState.type === 'uncertain') {
      void sendDueDate(dueDateState.command);
    }
  }

  onMount(() => {
    const unsubscribeSession = onSessionEnded(() => {
      assigneeNotice = undefined;
      moveGeneration += 1;
      moveOperation.abort();
      moveOperation = new AbortController();
      moveState = { type: 'idle' };
      priorityGeneration += 1;
      priorityOperation.abort();
      priorityOperation = new AbortController();
      priorityState = { type: 'idle' };
      dueDateGeneration += 1;
      dueDateOperation.abort();
      dueDateOperation = new AbortController();
      dueDateState = { type: 'idle' };
      selectedDueDate = '';
      selectedDueDateIssueId = '';
      flushSync(() => screen.sessionEnded());
    });
    const conceal = () => {
      assigneeNotice = undefined;
      moveOperation.abort();
      moveOperation = new AbortController();
      priorityOperation.abort();
      priorityOperation = new AbortController();
      dueDateOperation.abort();
      dueDateOperation = new AbortController();
      flushSync(() => screen.suspend());
    };
    const restore = () => {
      if (document.visibilityState === 'visible') {
        void screen.start();
      }
    };
    const visibility = () => {
      if (document.visibilityState === 'hidden') {
        conceal();
      } else {
        restore();
      }
    };
    const pageShow = (event: globalThis.PageTransitionEvent) => {
      if (event.persisted) {
        restore();
      }
    };
    globalThis.addEventListener('pagehide', conceal);
    globalThis.addEventListener('blur', conceal);
    globalThis.addEventListener('focus', restore);
    globalThis.addEventListener('pageshow', pageShow);
    document.addEventListener('visibilitychange', visibility);
    if (document.visibilityState === 'visible') {
      void screen.start();
    } else {
      screen.suspend();
    }
    return () => {
      globalThis.removeEventListener('pagehide', conceal);
      globalThis.removeEventListener('blur', conceal);
      globalThis.removeEventListener('focus', restore);
      globalThis.removeEventListener('pageshow', pageShow);
      document.removeEventListener('visibilitychange', visibility);
      unsubscribeSession();
      moveGeneration += 1;
      moveOperation.abort();
      priorityGeneration += 1;
      priorityOperation.abort();
      screen.dispose();
    };
  });
</script>

<main class="work">
  <h1 tabindex="-1" bind:this={heading}>
    {route.type === 'board' || route.type === 'column'
      ? 'Board do projeto'
      : route.type === 'projects'
        ? 'Meus projetos'
        : route.type === 'detail'
          ? 'Leitura da issue'
          : route.type === 'search'
            ? 'Busca de issues'
            : 'Issues do projeto'}
  </h1>
  <p class="intro">
    Leitura em desenvolvimento. Busque por termos no título/descrição ou por
    chave exata; a prioridade também pode ser filtrada. Outros filtros
    estruturados e atualização ao vivo ainda não estão disponíveis.
  </p>
  <nav class="work-links" aria-label="Navegação do trabalho">
    <a href="/projects">Meus projetos</a>
    {#if 'projectId' in route}
      <a href={`/projects/${route.projectId}/board`}>Ver board</a>
      <a href={`/projects/${route.projectId}/issues/new`}>Criar issue</a>
      {#if route.type === 'board' || route.type === 'column'}<a
          href={listLink(route.projectId)}>Lista de issues</a
        >{/if}
    {/if}
    {#if route.type === 'detail'}<a
        href={route.fromSearchQuery === undefined &&
        route.fromSearchPriority === undefined
          ? listLink(route.projectId, route.from)
          : searchLink(
              route.projectId,
              route.fromSearchQuery,
              route.fromSearchPriority,
              route.fromSearchCursor,
            )}
        >{route.fromSearchQuery === undefined &&
        route.fromSearchPriority === undefined
          ? 'Voltar à lista de issues'
          : 'Voltar aos resultados da busca'}</a
      >{/if}
  </nav>
  {#if view.type === 'idle' || view.type === 'loading'}
    <p role="status">Consultando acesso e dados…</p>
  {:else if view.type === 'suspended'}
    <p role="status">Dados ocultos até nova verificação.</p>
    <button onclick={() => void refresh()}>Consultar novamente</button>
  {:else if view.type === 'signed-out'}
    <h2>Sessão encerrada ou expirada</h2>
    <p role="status">
      Nenhum dado privado é exibido. Uma nova sessão é necessária; login ainda
      não está disponível nesta versão.
    </p>
  {:else if view.type === 'unavailable'}
    <h2>Conteúdo indisponível</h2>
    <p role="status">O endereço não está disponível para este acesso.</p>
    <button onclick={() => void refresh()}>Verificar acesso novamente</button>
  {:else if view.type === 'read-error'}
    <h2>Não foi possível consultar</h2>
    <p role="status">
      Nenhum dado anterior foi mantido. Confira a conexão e tente novamente.
    </p>
    <button onclick={() => void refresh()}>Tentar novamente</button>
  {:else if view.type === 'ready'}
    {@const data = view.data}
    <div data-private-work>
      {#if data.type === 'projects'}
        <p class="hint">
          Somente projetos com participação explícita. Seu papel de organização
          não amplia esta lista.
        </p>
        <a
          class="archive-toggle"
          href={route.type === 'projects' && route.includeArchived
            ? '/projects'
            : '/projects?include_archived=true'}
          >{route.type === 'projects' && route.includeArchived
            ? 'Ocultar projetos arquivados'
            : 'Incluir projetos arquivados'}</a
        >
        {#if data.page.data.length === 0}<p role="status">
            Nenhum projeto acessível nesta página.
          </p>{/if}
        <ul class="work-index" aria-label="Projetos acessíveis">
          {#each data.page.data as project (project.id)}
            <li data-project-id={project.id}>
              <a href={listLink(project.id)}
                ><span class="key">{project.project_key}</span><strong
                  >{project.name}</strong
                ></a
              >
              <p class="hint">
                {roles[project.role]}{project.archived_at === undefined
                  ? ''
                  : ' · Arquivado, somente leitura'}
              </p>
              {#if project.role !== 'viewer' && project.archived_at === undefined}<a
                  href={`/projects/${project.id}/issues/new`}>Criar issue</a
                >{/if}
            </li>
          {/each}
        </ul>
        {#if data.page.has_more}<a
            class="next-page"
            href={projectLink(data.page.next_cursor)}
            >Próxima página de projetos</a
          >{/if}
      {:else if data.type === 'issues' || data.type === 'search'}
        <form class="search-form" method="get" action={issueCollectionPath()}>
          <label for="issue-search"
            >Buscar no título, descrição ou chave exata (opcional)</label
          >
          <div>
            <input
              id="issue-search"
              type="search"
              name={searchDraft === '' ? undefined : 'q'}
              maxlength="256"
              bind:value={searchDraft}
              placeholder="Ex.: prazo revisão ou DEV-123"
            />
            <button type="submit">Buscar</button>
          </div>
          <label for="issue-search-priority">Filtrar por prioridade</label>
          <select
            id="issue-search-priority"
            name={selectedSearchPriority === '' ? undefined : 'priority'}
            bind:value={selectedSearchPriority}
          >
            <option value="">Todas as prioridades</option>
            <option value="none">Sem prioridade</option>
            <option value="low">Baixa</option>
            <option value="medium">Média</option>
            <option value="high">Alta</option>
            <option value="urgent">Urgente</option>
          </select>
          {#if data.type === 'search'}<a href={listLink(data.projectId)}
              >Limpar busca e filtro</a
            >{/if}
        </form>
        {#if data.page.data.length === 0}
          <p role="status">
            {data.type === 'search'
              ? 'Nenhuma issue corresponde aos critérios. Ajuste o texto ou a prioridade.'
              : 'Nenhuma issue fora da lixeira nesta página.'}
          </p>
        {/if}
        <ul class="work-index" aria-label="Issues do projeto">
          {#each data.page.data as issue (issue.id)}
            <li data-issue-id={issue.id}>
              <a href={detailLink(issue.project_id, issue.issue_number)}
                ><span class="key">{issue.issue_key}</span><strong
                  >{issue.title}</strong
                ></a
              >
              <p class="hint">
                {priorities[issue.priority]}{issue.due_date === undefined
                  ? ''
                  : ` · Prazo ${issue.due_date}`}{issue.is_overdue
                  ? ' · Atrasada'
                  : ''}
              </p>
            </li>
          {/each}
        </ul>
        {#if data.page.has_more}
          {#if data.type === 'search'}<a
              class="next-page"
              href={searchLink(
                data.projectId,
                data.query,
                data.priority,
                data.page.next_cursor,
              )}>Próxima página de resultados</a
            >{:else if route.type === 'issues'}<a
              class="next-page"
              href={listLink(route.projectId, data.page.next_cursor)}
              >Próxima página de issues</a
            >{/if}
        {/if}
      {:else if data.type === 'board'}
        {@const statuses = data.columns.map((column) => column.status)}
        <p class="hint">
          Colunas desta página do board, ordenadas por posição. Mover um card
          exige permissão e coloca-o no topo da coluna escolhida.
        </p>
        {#if moveState.type === 'pending'}<p role="status">
            Salvando movimento…
          </p>{/if}
        {#if moveState.type === 'uncertain'}<div role="alert">
            <p>
              Não foi possível confirmar o movimento. Não crie outro comando.
            </p>
            <button onclick={replayMove}
              >Repetir o mesmo comando com a mesma chave</button
            >
          </div>{/if}
        {#if moveState.type === 'conflict'}<p role="alert">
            O board mudou. Os dados foram atualizados; escolha o destino
            novamente.
          </p>{/if}
        {#if moveState.type === 'error'}<p role="alert">
            O movimento não foi aplicado. Verifique o acesso e tente novamente.
          </p>{/if}
        {#if moveState.type === 'confirmed'}<p role="status">
            Movimento de {moveState.issueKey} confirmado.
          </p>{/if}
        {#if data.columns.length === 0}<p role="status">
            Nenhuma coluna visível nesta página do board.
          </p>{/if}
        <div class="board-grid">
          {#each data.columns as column (column.status.id)}<BoardColumn
              status={column.status}
              page={column.page}
              canMove={data.canMoveIssues}
              moveStatuses={statuses}
              moveBusy={moveState.type === 'pending' ||
                moveState.type === 'uncertain'}
              onMove={(issue, statusId) => moveToColumn(data, issue, statusId)}
            />{/each}
        </div>
        {#if data.nextStatusCursor !== undefined && route.type === 'board'}<a
            class="next-page"
            href={`/projects/${route.projectId}/board?status_cursor=${data.nextStatusCursor}`}
            >Próxima página de colunas</a
          >{/if}
      {:else if data.type === 'column'}
        <p class="hint">
          Página de uma coluna. A contagem inclui cards ainda não carregados.
        </p>
        <BoardColumn status={data.status} page={data.page} />
      {:else}
        {@const issue = data.issue}
        <article data-issue-detail={issue.id}>
          {#if issue.trashed_at === undefined}<a
              href={`/projects/${issue.project_id}/issues/${issue.issue_number}/edit`}
              >Editar texto</a
            >{/if}
          <p class="key">{issue.issue_key}</p>
          <h2 class="issue-title">{issue.title}</h2>
          {#if issue.trashed_at !== undefined}<p role="status">
              Na lixeira. A leitura permanece até a remoção definitiva.
            </p>{/if}
          <dl>
            <div>
              <dt>Prioridade</dt>
              <dd>{priorities[issue.priority]}</dd>
            </div>
            <div>
              <dt>Status</dt>
              <dd>
                {data.status.name}
              </dd>
            </div>
            <div>
              <dt>Tipo</dt>
              <dd>
                {data.issueType.name}
              </dd>
            </div>
            {#if issue.due_date !== undefined}<div>
                <dt>Prazo</dt>
                <dd>{issue.due_date}{issue.is_overdue ? ' · Atrasada' : ''}</dd>
              </div>{/if}
            <div>
              <dt>Versão</dt>
              <dd>{issue.version}</dd>
            </div>
            <div>
              <dt>Atualizada</dt>
              <dd>
                <time datetime={issue.updated_at}>{date(issue.updated_at)}</time
                >
              </dd>
            </div>
          </dl>
          <IssueAssigneeEditor
            projectId={issue.project_id}
            actorId={data.actorId}
            {issue}
            canEdit={data.canUpdateIssueFields}
            onRefresh={async () => {
              await refresh(true);
            }}
            onNotice={(notice) => {
              assigneeNotice = notice;
            }}
          />
          {#if assigneeNotice?.issueId === issue.id}
            {#if assigneeNotice.type === 'confirmed'}<p role="status">
                {assigneeNotice.assigneeUserId === null
                  ? 'Responsável removido.'
                  : 'Responsável atualizado.'}
              </p>
            {:else}<p role="alert">
                A issue mudou. Os dados foram atualizados; revise a pessoa
                escolhida antes de salvar novamente.
              </p>{/if}
          {/if}
          {#if data.canUpdateIssueFields && issue.trashed_at === undefined}<section
              aria-labelledby="priority-update-title"
            >
              <h3 id="priority-update-title">Alterar prioridade</h3>
              <form
                onsubmit={(event) => {
                  event.preventDefault();
                  savePriority();
                }}
              >
                <label for="issue-priority">Prioridade</label>
                <select
                  id="issue-priority"
                  bind:value={selectedPriority}
                  disabled={priorityState.type === 'pending' ||
                    priorityState.type === 'uncertain'}
                >
                  <option value="none">Sem prioridade</option>
                  <option value="low">Baixa</option>
                  <option value="medium">Média</option>
                  <option value="high">Alta</option>
                  <option value="urgent">Urgente</option>
                </select>
                <button
                  type="submit"
                  disabled={priorityState.type === 'pending' ||
                    priorityState.type === 'uncertain'}
                  >Salvar prioridade</button
                >
              </form>
              {#if priorityState.type === 'pending'}<p role="status">
                  Salvando prioridade…
                </p>{/if}
              {#if priorityState.type === 'uncertain'}<div role="alert">
                  <p>
                    Não foi possível confirmar a alteração. Não crie outro
                    comando.
                  </p>
                  <button onclick={replayPriority}
                    >Repetir o mesmo comando com a mesma chave</button
                  >
                </div>{/if}
              {#if priorityState.type === 'conflict'}<p role="alert">
                  A issue mudou. Os dados foram atualizados; revise a prioridade
                  escolhida antes de salvar novamente.
                </p>{/if}
              {#if priorityState.type === 'error'}<p role="alert">
                  A prioridade não foi alterada. Verifique o acesso e tente
                  novamente.
                </p>{/if}
              {#if priorityState.type === 'confirmed'}<p role="status">
                  Prioridade alterada para
                  {priorities[priorityState.priority]}.
                </p>{/if}
            </section>{/if}
          {#if data.canUpdateIssueFields && issue.trashed_at === undefined}<section
              aria-labelledby="due-date-update-title"
            >
              <h3 id="due-date-update-title">Alterar prazo</h3>
              <form
                class="due-date-form"
                onsubmit={(event) => {
                  event.preventDefault();
                  saveDueDate();
                }}
              >
                <label for="issue-due-date">Prazo · data civil</label>
                <input
                  id="issue-due-date"
                  type="date"
                  bind:value={selectedDueDate}
                  disabled={dueDateState.type === 'pending' ||
                    dueDateState.type === 'uncertain'}
                />
                <p class="hint">
                  O prazo permanece nesta data no calendário da organização;
                  datas passadas são aceitas.
                </p>
                <div class="form-actions">
                  <button
                    type="submit"
                    disabled={dueDateState.type === 'pending' ||
                      dueDateState.type === 'uncertain'}>Salvar prazo</button
                  >
                  <button
                    type="button"
                    disabled={dueDateState.type === 'pending' ||
                      dueDateState.type === 'uncertain'}
                    onclick={() => saveDueDate(null)}>Limpar prazo</button
                  >
                </div>
              </form>
              {#if dueDateState.type === 'pending'}<p role="status">
                  Salvando prazo…
                </p>{/if}
              {#if dueDateState.type === 'uncertain'}<div role="alert">
                  <p>
                    Não foi possível confirmar o prazo. Não crie outro comando.
                  </p>
                  <button onclick={replayDueDate}
                    >Repetir o mesmo comando com a mesma chave</button
                  >
                </div>{/if}
              {#if dueDateState.type === 'conflict'}<p role="alert">
                  A issue mudou. Os dados foram atualizados; revise a data
                  escolhida antes de salvar novamente.
                </p>{/if}
              {#if dueDateState.type === 'error'}<p role="alert">
                  O prazo não foi alterado. Verifique o acesso e tente
                  novamente.
                </p>{/if}
              {#if dueDateState.type === 'confirmed'}<p role="status">
                  {dueDateState.dueDate === null
                    ? 'Prazo removido.'
                    : `Prazo definido para ${dueDateState.dueDate}.`}
                </p>{/if}
            </section>{/if}
          <section aria-labelledby="description-title">
            <h3 id="description-title">Descrição · fonte Markdown</h3>
            <p class="hint">
              Texto original, sem renderização de links ou imagens.
            </p>
            <pre>{issue.description ?? 'Sem descrição.'}</pre>
          </section>
          <section aria-labelledby="history-title">
            <h3 id="history-title">Histórico desta página</h3>
            {#if data.history.data.length === 0}<p role="status">
                Nenhum evento nesta página.
              </p>{/if}
            <ul class="journal" aria-label="Histórico da issue">
              {#each data.history.data as event (event.id)}<li>
                  <strong>{eventLabels[event.event_kind]}</strong><time
                    datetime={event.occurred_at}>{date(event.occurred_at)}</time
                  ><span class="hint"
                    >Referência do autor {event.actor_user_id}</span
                  >
                  {#if event.change !== undefined}<span>
                      {#if event.change.field === 'priority'}Prioridade:
                        {priorities[event.change.before]} →
                        {priorities[event.change.after]}
                      {:else if event.change.field === 'due_date'}
                        Prazo: {event.change.before ?? 'Sem prazo'} →
                        {event.change.after ?? 'Sem prazo'}
                      {:else}
                        Responsável:
                        {event.change.before ?? 'Sem responsável'} →
                        {event.change.after ?? 'Sem responsável'}
                      {/if}
                    </span>{/if}
                </li>{/each}
            </ul>
            {#if data.history.has_more}<a
                class="next-page"
                href={detailLink(
                  issue.project_id,
                  issue.issue_number,
                  data.history.next_cursor,
                )}>Próxima página do histórico</a
              >{/if}
          </section>
        </article>
      {/if}
    </div>
    <button onclick={() => void refresh()}>Atualizar consulta</button>
  {/if}
</main>

<style>
  .board-grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 210px), 1fr));
    gap: 24px;
  }
  .work {
    max-width: 800px;
    margin-top: 48px;
  }
  .work-links {
    display: flex;
    gap: 20px;
    flex-wrap: wrap;
    margin: 20px 0;
  }
  a {
    color: var(--primary);
    min-height: 44px;
    display: inline-flex;
    align-items: center;
  }
  a:focus-visible,
  h1:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 4px;
  }
  .work-index,
  .journal {
    list-style: none;
    padding: 0;
    margin: 24px 0;
  }
  .search-form {
    display: grid;
    gap: 10px;
    margin: 24px 0;
  }
  .search-form label {
    font-weight: 600;
  }
  .search-form > div {
    display: flex;
    gap: 10px;
  }
  .search-form input,
  .search-form select {
    box-sizing: border-box;
    min-width: 0;
    min-height: 44px;
    flex: 1;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--surface);
    color: var(--text);
    font: inherit;
  }
  .search-form button {
    min-height: 44px;
    padding: 8px 14px;
    font: inherit;
  }
  .search-form a {
    width: fit-content;
  }
  .search-form input:focus-visible,
  .search-form select:focus-visible,
  .search-form button:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 3px;
  }
  .work-index li {
    border-bottom: 1px solid var(--border);
    padding: 16px 0;
    overflow-wrap: anywhere;
  }
  .work-index li > a {
    display: grid;
    grid-template-columns: minmax(90px, auto) 1fr;
    gap: 16px;
    text-decoration: none;
    align-items: baseline;
  }
  .work-index strong {
    font-weight: 600;
    line-height: 1.5;
  }
  .work-index li > a:hover strong {
    text-decoration: underline;
  }
  .key {
    color: var(--muted);
    font-size: 14px;
    font-weight: 500;
    overflow-wrap: anywhere;
  }
  .work-index p {
    margin: 4px 0 0;
  }
  .next-page {
    margin: 8px 20px 16px 0;
    font-weight: 600;
  }
  .issue-title {
    font-size: 28px;
    line-height: 1.3;
    overflow-wrap: anywhere;
  }
  dl {
    display: grid;
    gap: 12px;
    margin: 28px 0;
  }
  dl > div {
    display: grid;
    grid-template-columns: 110px 1fr;
    gap: 12px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  section {
    margin-top: 32px;
    border-top: 1px solid var(--border);
    padding-top: 20px;
  }
  .due-date-form {
    display: grid;
    gap: 12px;
  }
  .due-date-form label {
    font-weight: 600;
  }
  .due-date-form input {
    box-sizing: border-box;
    width: 100%;
    min-height: 44px;
    padding: 10px 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--surface);
    color: var(--text);
    font: inherit;
  }
  .due-date-form input:focus-visible,
  .due-date-form button:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 3px;
  }
  .form-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
  }
  .due-date-form button {
    min-height: 44px;
    padding: 8px 12px;
    font: inherit;
  }
  h3 {
    font-size: 16px;
    margin: 0 0 12px;
  }
  pre {
    font-family: inherit;
    font-size: 16px;
    line-height: 1.65;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    margin: 12px 0;
  }
  .journal li {
    display: grid;
    gap: 6px;
    padding: 12px 0;
    overflow-wrap: anywhere;
  }
  @media (max-width: 480px) {
    .work-index li > a {
      grid-template-columns: 1fr;
      gap: 6px;
    }
    dl > div {
      grid-template-columns: 1fr;
      gap: 4px;
    }
  }
</style>
