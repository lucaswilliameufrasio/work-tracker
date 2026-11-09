<script lang="ts">
  import { onMount } from 'svelte';
  import {
    CommentRequestError,
    createComment,
    editComment,
    listComments,
    removeComment,
    type IssueComment,
    type IssueCommentEvent,
  } from './comments';

  interface Props {
    projectId: string;
    issueNumber: string;
  }

  let { projectId, issueNumber }: Props = $props();

  type View =
    | { type: 'loading' }
    | {
        type: 'ready';
        comments: IssueComment[];
        events: IssueCommentEvent[];
        draft: string;
        editingId: string | undefined;
        notice: string | undefined;
      }
    | { type: 'saving'; draft: string }
    | { type: 'unavailable' }
    | { type: 'error' };

  let view = $state<View>({ type: 'loading' });
  const eventLabels: Record<IssueCommentEvent['event_kind'], string> = {
    'comment.created': 'Comentário criado',
    'comment.edited': 'Comentário editado',
    'comment.removed': 'Comentário removido',
  };

  function currentDraft(): string {
    if (view.type === 'ready' || view.type === 'saving') {
      return view.draft;
    }
    return '';
  }

  async function load(): Promise<View> {
    const draft = currentDraft();
    try {
      const page = await listComments(
        projectId,
        issueNumber,
        AbortSignal.timeout(8000),
      );
      return {
        type: 'ready',
        comments: page.comments,
        events: page.events,
        draft,
        editingId: undefined,
        notice: undefined,
      };
    } catch (error) {
      if (error instanceof CommentRequestError && error.status === 404) {
        return { type: 'unavailable' };
      }
      return { type: 'error' };
    }
  }

  async function submit(event: globalThis.SubmitEvent): Promise<void> {
    event.preventDefault();
    if (view.type !== 'ready') {
      return;
    }
    const draft = view.draft;
    view = { type: 'saving', draft };
    try {
      await createComment(
        projectId,
        issueNumber,
        draft,
        AbortSignal.timeout(8000),
      );
      const loaded = await load();
      if (loaded.type === 'ready') {
        view = {
          type: 'ready',
          comments: loaded.comments,
          events: loaded.events,
          draft: '',
          editingId: undefined,
          notice: 'Comentário registrado.',
        };
        return;
      }
      view = loaded;
    } catch (error) {
      if (error instanceof CommentRequestError && error.status === 403) {
        const loaded = await load();
        if (loaded.type === 'ready') {
          view = {
            type: 'ready',
            comments: loaded.comments,
            events: loaded.events,
            draft,
            editingId: undefined,
            notice: 'Você pode ler, mas não comentar.',
          };
          return;
        }
        view = loaded;
        return;
      }
      view = { type: 'error' };
    }
  }

  async function saveEdit(comment: IssueComment, body: string): Promise<void> {
    try {
      await editComment(
        projectId,
        issueNumber,
        comment.id,
        body,
        comment.version,
        AbortSignal.timeout(8000),
      );
      view = await load();
    } catch (error) {
      if (error instanceof CommentRequestError && error.status === 409) {
        const loaded = await load();
        if (loaded.type === 'ready') {
          view = {
            type: 'ready',
            comments: loaded.comments,
            events: loaded.events,
            draft: loaded.draft,
            editingId: undefined,
            notice:
              'O comentário mudou. Revise o texto antes de salvar de novo.',
          };
          return;
        }
        view = loaded;
        return;
      }
      view = { type: 'error' };
    }
  }

  async function remove(comment: IssueComment): Promise<void> {
    try {
      await removeComment(
        projectId,
        issueNumber,
        comment.id,
        AbortSignal.timeout(8000),
      );
      view = await load();
    } catch {
      view = { type: 'error' };
    }
  }

  onMount(() => {
    void load().then((loaded) => {
      view = loaded;
    });
  });
</script>

<section aria-labelledby="comments-title">
  <h3 id="comments-title">Comentários</h3>
  <p class="hint">
    O texto fica guardado como Markdown e aparece sem renderizar links ou
    imagens. Quem escreveu pode editar e apagar. Um manager pode apagar o
    comentário de outra pessoa, sem reescrever o texto.
  </p>
  {#if view.type === 'loading'}<p role="status">Carregando comentários…</p>{/if}
  {#if view.type === 'unavailable'}<p role="status">
      Comentários indisponíveis para esta issue.
    </p>{/if}
  {#if view.type === 'error'}<p role="alert">
      Não foi possível carregar ou salvar os comentários.
    </p>{/if}
  {#if view.type === 'saving'}<p role="status">Salvando comentário…</p>{/if}
  {#if view.type === 'ready'}
    {#if view.notice !== undefined}<p role="status">{view.notice}</p>{/if}
    {#if view.comments.length === 0}<p role="status">Nenhum comentário.</p>{/if}
    <ul>
      {#each view.comments as comment (comment.id)}
        <li>
          <strong>{comment.author_display_name}</strong>
          <pre>{comment.body}</pre>
          {#if comment.can_edit}
            <form
              onsubmit={(event) => {
                event.preventDefault();
                const form = event.currentTarget;
                if (!(form instanceof globalThis.HTMLFormElement)) {
                  return;
                }
                const data = new globalThis.FormData(form);
                const body = data.get('body');
                if (typeof body === 'string') {
                  void saveEdit(comment, body);
                }
              }}
            >
              <label>
                Editar comentário
                <textarea name="body">{comment.body}</textarea>
              </label>
              <button type="submit">Salvar comentário</button>
            </form>
          {/if}
          {#if comment.can_delete}
            <button type="button" onclick={() => void remove(comment)}
              >Apagar</button
            >
          {/if}
        </li>
      {/each}
    </ul>
    {#if view.events.length > 0}
      <ul aria-label="Histórico dos comentários">
        {#each view.events as event (event.id)}
          <li>{eventLabels[event.event_kind]}</li>
        {/each}
      </ul>
    {/if}
    <form onsubmit={(event) => void submit(event)}>
      <label>
        Novo comentário
        <textarea bind:value={view.draft}></textarea>
      </label>
      <button type="submit">Comentar</button>
    </form>
  {/if}
</section>
