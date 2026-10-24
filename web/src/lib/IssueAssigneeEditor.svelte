<script lang="ts">
  import { onDestroy } from 'svelte';
  import { IssueRequestError } from './issue-transport.ts';
  import {
    IssueAssigneeIdentityChanged,
    setIssueAssignee,
    type IssueAssigneeInput,
    type IssueAssigneeTarget,
  } from './issue-assignee.ts';
  import {
    getProjectMember,
    listProjectMembers,
    ProjectMemberIdentityChanged,
    type ProjectMember,
  } from './project-members.ts';
  import type { IssueDetail } from './issues.ts';

  type AssigneeCommand = {
    projectId: string;
    actorId: string;
    issue: IssueAssigneeTarget;
    idempotencyKey: string;
    input: IssueAssigneeInput;
  };
  type AssigneeState =
    | { type: 'idle' }
    | { type: 'pending'; command: AssigneeCommand }
    | { type: 'uncertain'; command: AssigneeCommand }
    | { type: 'confirmed'; assigneeUserId: string | null }
    | { type: 'conflict' }
    | { type: 'error' };
  type RosterPage = {
    members: ProjectMember[];
    hasMore: boolean;
    nextCursor?: string;
  };
  type RosterState =
    | { type: 'idle' }
    | { type: 'loading' }
    | { type: 'ready'; page: RosterPage }
    | { type: 'loading-more'; page: RosterPage }
    | { type: 'error'; page?: RosterPage };
  type CurrentMemberState =
    | { type: 'none' }
    | { type: 'loading'; userId: string }
    | { type: 'loaded'; member: ProjectMember }
    | { type: 'unavailable'; userId: string };

  let {
    projectId,
    actorId,
    issue,
    canEdit,
    onRefresh,
    onNotice,
  }: {
    projectId: string;
    actorId: string;
    issue: IssueDetail;
    canEdit: boolean;
    onRefresh: () => Promise<void>;
    onNotice: (
      notice:
        | {
            type: 'confirmed';
            issueId: string;
            assigneeUserId: string | null;
          }
        | { type: 'conflict'; issueId: string }
        | undefined,
    ) => void;
  } = $props();

  let selectedAssignee = $state('');
  let assigneeState = $state<AssigneeState>({ type: 'idle' });
  let rosterState = $state<RosterState>({ type: 'idle' });
  let currentMemberState = $state<CurrentMemberState>({ type: 'none' });
  let rosterOperation = new AbortController();
  let currentMemberOperation = new AbortController();
  let assigneeOperation = new AbortController();
  let rosterGeneration = 0;
  let currentMemberGeneration = 0;
  let assigneeGeneration = 0;
  let rosterKey = '';
  let currentMemberKey = '';

  $effect(() => {
    const key = `${projectId}/${actorId}/${issue.id}/${canEdit}`;
    if (key !== rosterKey) {
      rosterKey = key;
      rosterOperation.abort();
      rosterOperation = new AbortController();
      rosterGeneration += 1;
      assigneeOperation.abort();
      assigneeOperation = new AbortController();
      assigneeGeneration += 1;
      selectedAssignee = issue.assignee_user_id ?? '';
      assigneeState = { type: 'idle' };
      if (canEdit) {
        void loadRoster();
      } else {
        rosterState = { type: 'idle' };
      }
    }
  });

  $effect(() => {
    const assignedUserId = issue.assignee_user_id;
    const key = `${projectId}/${actorId}/${issue.id}/${assignedUserId ?? ''}`;
    if (key !== currentMemberKey) {
      currentMemberKey = key;
      void loadCurrentMember(assignedUserId);
    }
  });

  function pageFromState(state: RosterState): RosterPage | undefined {
    if (state.type === 'ready' || state.type === 'loading-more') {
      return state.page;
    }
    if (state.type === 'error') {
      return state.page;
    }
    return undefined;
  }

  async function loadRoster(): Promise<void> {
    const generation = ++rosterGeneration;
    rosterOperation.abort();
    rosterOperation = new AbortController();
    rosterState = { type: 'loading' };
    try {
      const page = await listProjectMembers(
        projectId,
        actorId,
        { page_size: 50 },
        rosterOperation.signal,
      );
      if (generation !== rosterGeneration) {
        return;
      }
      rosterState = {
        type: 'ready',
        page: {
          members: page.data,
          hasMore: page.has_more,
          ...(page.next_cursor === undefined
            ? {}
            : { nextCursor: page.next_cursor }),
        },
      };
      const selected = page.data.find(
        (member) => member.user_id === selectedAssignee,
      );
      if (selected !== undefined) {
        currentMemberState = { type: 'loaded', member: selected };
      }
    } catch (error) {
      if (generation !== rosterGeneration) {
        return;
      }
      if (error instanceof ProjectMemberIdentityChanged) {
        selectedAssignee = '';
      }
      rosterState = { type: 'error' };
    }
  }

  async function loadMoreMembers(): Promise<void> {
    if (rosterState.type !== 'ready' || !rosterState.page.hasMore) {
      return;
    }
    const previousPage = rosterState.page;
    const cursor = previousPage.nextCursor;
    if (cursor === undefined) {
      rosterState = { type: 'error', page: previousPage };
      return;
    }
    const generation = ++rosterGeneration;
    rosterOperation.abort();
    rosterOperation = new AbortController();
    rosterState = { type: 'loading-more', page: previousPage };
    try {
      const nextPage = await listProjectMembers(
        projectId,
        actorId,
        { cursor, page_size: 50 },
        rosterOperation.signal,
      );
      if (generation !== rosterGeneration) {
        return;
      }
      const existingIds = new Set(
        previousPage.members.map((member) => member.user_id),
      );
      if (nextPage.data.some((member) => existingIds.has(member.user_id))) {
        rosterState = { type: 'error', page: previousPage };
        return;
      }
      const page: RosterPage = {
        members: [...previousPage.members, ...nextPage.data],
        hasMore: nextPage.has_more,
        ...(nextPage.next_cursor === undefined
          ? {}
          : { nextCursor: nextPage.next_cursor }),
      };
      rosterState = { type: 'ready', page };
      const selected = nextPage.data.find(
        (member) => member.user_id === selectedAssignee,
      );
      if (selected !== undefined) {
        currentMemberState = { type: 'loaded', member: selected };
      }
    } catch {
      if (generation === rosterGeneration) {
        rosterState = { type: 'error', page: previousPage };
      }
    }
  }

  async function loadCurrentMember(userId: string | undefined): Promise<void> {
    const generation = ++currentMemberGeneration;
    currentMemberOperation.abort();
    currentMemberOperation = new AbortController();
    if (userId === undefined) {
      currentMemberState = { type: 'none' };
      return;
    }
    const fromRoster = pageFromState(rosterState)?.members.find(
      (member) => member.user_id === userId,
    );
    if (fromRoster !== undefined) {
      currentMemberState = { type: 'loaded', member: fromRoster };
      return;
    }
    currentMemberState = { type: 'loading', userId };
    try {
      const member = await getProjectMember(
        projectId,
        userId,
        actorId,
        currentMemberOperation.signal,
      );
      if (generation === currentMemberGeneration) {
        currentMemberState = { type: 'loaded', member };
      }
    } catch (error) {
      if (generation !== currentMemberGeneration) {
        return;
      }
      if (error instanceof ProjectMemberIdentityChanged) {
        currentMemberState = { type: 'unavailable', userId };
        return;
      }
      if (error instanceof IssueRequestError && error.status === 404) {
        currentMemberState = { type: 'unavailable', userId };
        return;
      }
      if (error instanceof IssueRequestError && error.status === 401) {
        currentMemberState = { type: 'none' };
        return;
      }
      currentMemberState = { type: 'unavailable', userId };
    }
  }

  function availableMembers(): ProjectMember[] {
    const page = pageFromState(rosterState);
    const members =
      page?.members.filter(
        (member) =>
          member.can_be_assigned || member.user_id === issue.assignee_user_id,
      ) ?? [];
    if (currentMemberState.type === 'loaded') {
      const currentMember = currentMemberState.member;
      if (!members.some((member) => member.user_id === currentMember.user_id)) {
        members.push(currentMember);
      }
    }
    return members;
  }

  async function sendAssignment(command: AssigneeCommand): Promise<void> {
    if (assigneeState.type === 'pending') {
      return;
    }
    const generation = ++assigneeGeneration;
    assigneeOperation.abort();
    assigneeOperation = new AbortController();
    assigneeState = { type: 'pending', command };
    try {
      const result = await setIssueAssignee(
        command.projectId,
        command.issue,
        command.actorId,
        command.idempotencyKey,
        command.input,
        assigneeOperation.signal,
      );
      if (generation !== assigneeGeneration) {
        return;
      }
      selectedAssignee = result.receipt.assignee_user_id ?? '';
      assigneeState = {
        type: 'confirmed',
        assigneeUserId: result.receipt.assignee_user_id,
      };
      onNotice({
        type: 'confirmed',
        issueId: issue.id,
        assigneeUserId: result.receipt.assignee_user_id,
      });
      await onRefresh();
    } catch (error) {
      if (generation !== assigneeGeneration) {
        return;
      }
      if (error instanceof IssueAssigneeIdentityChanged) {
        selectedAssignee = '';
        assigneeState = { type: 'idle' };
        onNotice(undefined);
        await onRefresh();
        return;
      }
      if (error instanceof IssueRequestError) {
        if (error.status === 401) {
          selectedAssignee = '';
          assigneeState = { type: 'idle' };
          onNotice(undefined);
          return;
        }
        if (error.status === 409) {
          assigneeState = { type: 'conflict' };
          onNotice({ type: 'conflict', issueId: issue.id });
          await onRefresh();
          return;
        }
        if (error.status >= 500) {
          assigneeState = { type: 'uncertain', command };
          return;
        }
        assigneeState = { type: 'error' };
        if (error.status === 403 || error.status === 404) {
          onNotice(undefined);
          await onRefresh();
        }
        return;
      }
      assigneeState = { type: 'uncertain', command };
    }
  }

  function saveAssignment(): void {
    if (
      !canEdit ||
      issue.trashed_at !== undefined ||
      assigneeState.type === 'pending' ||
      assigneeState.type === 'uncertain'
    ) {
      return;
    }
    onNotice(undefined);
    void sendAssignment({
      projectId,
      actorId,
      issue: {
        id: issue.id,
        issue_number: issue.issue_number,
        issue_key: issue.issue_key,
      },
      idempotencyKey: globalThis.crypto.randomUUID(),
      input: {
        expected_version: issue.version,
        assignee_user_id: selectedAssignee === '' ? null : selectedAssignee,
      },
    });
  }

  function replayAssignment(): void {
    if (assigneeState.type === 'uncertain') {
      void sendAssignment(assigneeState.command);
    }
  }

  function roleLabel(role: ProjectMember['role']): string {
    if (role === 'manager') {
      return 'gerente';
    }
    if (role === 'viewer') {
      return 'leitor';
    }
    return 'membro';
  }

  onDestroy(() => {
    rosterGeneration += 1;
    currentMemberGeneration += 1;
    assigneeGeneration += 1;
    rosterOperation.abort();
    currentMemberOperation.abort();
    assigneeOperation.abort();
  });
</script>

<section aria-labelledby="assignee-title" data-assignee-editor={issue.id}>
  <h3 id="assignee-title">Responsável</h3>
  {#if issue.assignee_user_id === undefined}<p data-current-assignee="none">
      Sem responsável.
    </p>
  {:else if currentMemberState.type === 'loading'}<p role="status">
      Carregando pessoa responsável…
    </p>
  {:else if currentMemberState.type === 'loaded'}<p
      data-current-assignee={currentMemberState.member.user_id}
    >
      {currentMemberState.member.display_name} · {roleLabel(
        currentMemberState.member.role,
      )}{currentMemberState.member.can_be_assigned
        ? ''
        : ' · atribuição atual não elegível'}
    </p>
  {:else}<p data-current-assignee={issue.assignee_user_id}>
      Responsável atual sem cadastro visível ({issue.assignee_user_id}).
    </p>{/if}

  {#if canEdit && issue.trashed_at === undefined}<form
      onsubmit={(event) => {
        event.preventDefault();
        saveAssignment();
      }}
    >
      <label for={`issue-assignee-${issue.id}`}>Atribuir a</label>
      <select
        id={`issue-assignee-${issue.id}`}
        bind:value={selectedAssignee}
        disabled={assigneeState.type === 'pending' ||
          assigneeState.type === 'uncertain'}
      >
        <option value="">Sem responsável</option>
        {#each availableMembers() as member (member.user_id)}<option
            value={member.user_id}
            disabled={!member.can_be_assigned &&
              member.user_id !== issue.assignee_user_id}
            >{member.display_name} · {roleLabel(
              member.role,
            )}{member.can_be_assigned ? '' : ' · atribuição atual'}</option
          >{/each}
        {#if currentMemberState.type === 'unavailable'}<option
            value={currentMemberState.userId}
            disabled>Cadastro atual indisponível</option
          >{/if}
      </select>
      {#if rosterState.type === 'loading'}<p role="status">
          Carregando participantes…
        </p>
      {:else if rosterState.type === 'error'}<div role="alert">
          <p>Não foi possível carregar os participantes do projeto.</p>
          <button type="button" onclick={() => void loadRoster()}
            >Tentar carregar novamente</button
          >
        </div>
      {:else if rosterState.type === 'ready' || rosterState.type === 'loading-more'}
        {@const rosterPage = rosterState.page}
        {#if rosterPage.hasMore}<button
            type="button"
            disabled={rosterState.type === 'loading-more'}
            onclick={() => void loadMoreMembers()}
            >{rosterState.type === 'loading-more'
              ? 'Carregando mais pessoas…'
              : 'Carregar mais pessoas'}</button
          >{/if}{/if}
      <button
        type="submit"
        disabled={assigneeState.type === 'pending' ||
          assigneeState.type === 'uncertain'}>Salvar responsável</button
      >
    </form>{/if}
  {#if assigneeState.type === 'pending'}<p role="status">
      Salvando responsável…
    </p>{/if}
  {#if assigneeState.type === 'uncertain'}<div role="alert">
      <p>Não foi possível confirmar a atribuição. Não crie outro comando.</p>
      <button onclick={replayAssignment}
        >Repetir o mesmo comando com a mesma chave</button
      >
    </div>{/if}
  {#if assigneeState.type === 'conflict'}<p role="alert">
      A issue mudou. Os dados foram atualizados; revise a pessoa escolhida antes
      de salvar novamente.
    </p>{/if}
  {#if assigneeState.type === 'error'}<p role="alert">
      A atribuição não foi alterada. Escolha uma pessoa elegível e verifique o
      acesso ao projeto.
    </p>{/if}
  {#if assigneeState.type === 'confirmed'}<p role="status">
      {assigneeState.assigneeUserId === null
        ? 'Responsável removido.'
        : 'Responsável atualizado.'}
    </p>{/if}
</section>

<style>
  section {
    margin: 24px 0;
    border-top: 1px solid var(--border, #b9c1ca);
    padding-top: 16px;
  }
  form {
    display: grid;
    justify-items: start;
    gap: 10px;
  }
  label {
    font-weight: 650;
  }
  select,
  button {
    min-height: 44px;
    font: inherit;
  }
  select {
    max-width: 100%;
  }
  button {
    padding: 8px 14px;
  }
</style>
