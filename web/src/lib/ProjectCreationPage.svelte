<script lang="ts">
  import { onMount } from 'svelte';
  import { createProject, EntryRequestError } from './entry.ts';
  import { getCurrentSession, SessionRequestError } from './sessions.ts';

  type View =
    | { type: 'checking' }
    | { type: 'editing' }
    | { type: 'submitting' }
    | { type: 'rejected'; message: string }
    | { type: 'unauthenticated' };

  let view = $state<View>({ type: 'checking' });
  let projectKey = $state('');
  let name = $state('');
  let description = $state('');
  let csrf = $state<string | undefined>(undefined);
  const controller = new AbortController();

  onMount(() => {
    void load();
    return () => controller.abort();
  });

  async function load(): Promise<void> {
    try {
      const session = await getCurrentSession(controller.signal);
      csrf = session.csrf_token;
      view = { type: 'editing' };
    } catch (error) {
      if (controller.signal.aborted) {
        return;
      }
      if (error instanceof SessionRequestError && error.status === 401) {
        view = { type: 'unauthenticated' };
        return;
      }
      view = {
        type: 'rejected',
        message: 'Não foi possível confirmar a sessão.',
      };
    }
  }

  async function submit(): Promise<void> {
    if (csrf === undefined) {
      view = { type: 'unauthenticated' };
      return;
    }
    view = { type: 'submitting' };
    const input: { project_key: string; name: string; description?: string } = {
      project_key: projectKey.trim(),
      name: name.trim(),
    };
    const trimmed = description.trim();
    if (trimmed !== '') {
      input.description = trimmed;
    }
    try {
      const project = await createProject(input, csrf, controller.signal);
      globalThis.location.assign(`/projects/${project.id}/board`);
    } catch (error) {
      if (controller.signal.aborted) {
        return;
      }
      if (error instanceof EntryRequestError && error.status === 403) {
        view = {
          type: 'rejected',
          message: 'Sua conta não pode criar projetos.',
        };
        return;
      }
      if (error instanceof EntryRequestError && error.status === 409) {
        view = {
          type: 'rejected',
          message: 'Essa chave de projeto já existe.',
        };
        return;
      }
      if (error instanceof EntryRequestError && error.status === 422) {
        view = {
          type: 'rejected',
          message: 'Use uma chave de 2 a 6 letras maiúsculas e um nome.',
        };
        return;
      }
      view = {
        type: 'rejected',
        message: 'Não foi possível criar o projeto.',
      };
    }
  }
</script>

<main>
  <h1>Criar projeto</h1>
  <p class="intro">
    O primeiro projeto fica com você como gerente. Quem é membro da organização
    não cria projeto.
  </p>
  {#if view.type === 'checking'}
    <p role="status">Confirmando a sessão…</p>
  {:else if view.type === 'unauthenticated'}
    <p role="alert">Entre na instalação antes de criar um projeto.</p>
    <p><a href="/">Entrar</a></p>
  {:else}
    {#if view.type === 'rejected'}
      <p role="alert">{view.message}</p>
    {/if}
    <form
      onsubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <label>
        Chave
        <input
          name="project_key"
          required
          minlength="2"
          maxlength="6"
          autocapitalize="characters"
          bind:value={projectKey}
        />
      </label>
      <label>
        Nome
        <input name="name" required bind:value={name} />
      </label>
      <label>
        Descrição
        <input name="description" bind:value={description} />
      </label>
      <button type="submit" disabled={view.type === 'submitting'}>
        Criar projeto
      </button>
    </form>
  {/if}
</main>

<style>
  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 14px;
    font-weight: 600;
    margin-top: 16px;
  }
  input {
    font: inherit;
    min-height: 44px;
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 12px;
    background: var(--surface);
    color: var(--text);
  }
  input:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 4px;
  }
</style>
