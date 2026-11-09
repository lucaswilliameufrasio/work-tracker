<script lang="ts">
  import { onMount } from 'svelte';
  import { getCurrentSession, SessionRequestError } from './sessions.ts';

  type View =
    | { type: 'checking' }
    | { type: 'editing' }
    | { type: 'submitting' }
    | { type: 'issued'; delivery: string; inviteUrl: string; token: string }
    | { type: 'rejected'; message: string }
    | { type: 'unauthenticated' };

  type GrantView =
    | { type: 'hidden' }
    | { type: 'editing'; configured: boolean; notice: string | undefined }
    | { type: 'saving'; configured: boolean };

  let view = $state<View>({ type: 'checking' });
  let grantView = $state<GrantView>({ type: 'hidden' });
  let email = $state('');
  let role = $state('member');
  let projectId = $state('');
  let csrf = $state<string | undefined>(undefined);
  let grantScope = $state('read');
  let allProjects = $state(false);
  let grantProjectId = $state('');
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
      await loadGrant();
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

  async function loadGrant(): Promise<void> {
    try {
      const response = await fetch('/api/v1/mcp/grant', {
        credentials: 'same-origin',
        cache: 'no-store',
        signal: controller.signal,
      });
      if (!response.ok) {
        grantView = { type: 'hidden' };
        return;
      }
      const payload: unknown = await response.json();
      const configured =
        typeof payload === 'object' &&
        payload !== null &&
        'configured' in payload &&
        payload.configured === true;
      grantView = { type: 'editing', configured, notice: undefined };
    } catch {
      if (!controller.signal.aborted) {
        grantView = {
          type: 'editing',
          configured: false,
          notice: 'Não foi possível ler o acesso do MCP.',
        };
      }
    }
  }

  async function saveGrant(event: globalThis.SubmitEvent): Promise<void> {
    event.preventDefault();
    if (csrf === undefined || grantView.type !== 'editing') {
      return;
    }
    const configured = grantView.configured;
    grantView = { type: 'saving', configured };
    try {
      const response = await fetch('/api/v1/mcp/grant', {
        method: 'PUT',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'content-type': 'application/json',
          'x-csrf-token': csrf,
        },
        body: JSON.stringify({
          scope: grantScope,
          all_projects: allProjects,
          projects: allProjects ? [] : [grantProjectId],
        }),
        signal: controller.signal,
      });
      if (!response.ok) {
        grantView = {
          type: 'editing',
          configured,
          notice: 'O acesso do MCP não foi gravado.',
        };
        return;
      }
      grantView = {
        type: 'editing',
        configured: true,
        notice: 'O acesso do MCP foi gravado nesta instalação.',
      };
    } catch {
      if (!controller.signal.aborted) {
        grantView = {
          type: 'editing',
          configured,
          notice: 'Não foi possível falar com o Puxiri.',
        };
      }
    }
  }

  async function revokeGrant(): Promise<void> {
    if (
      csrf === undefined ||
      grantView.type !== 'editing' ||
      !grantView.configured
    ) {
      return;
    }
    grantView = { type: 'saving', configured: true };
    try {
      const response = await fetch('/api/v1/mcp/grant', {
        method: 'DELETE',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'x-csrf-token': csrf,
        },
        signal: controller.signal,
      });
      if (!response.ok) {
        grantView = {
          type: 'editing',
          configured: true,
          notice: 'O acesso do MCP não foi revogado.',
        };
        return;
      }
      grantView = {
        type: 'editing',
        configured: false,
        notice: 'O acesso do MCP foi revogado.',
      };
    } catch {
      if (!controller.signal.aborted) {
        grantView = {
          type: 'editing',
          configured: true,
          notice: 'Não foi possível falar com o Puxiri.',
        };
      }
    }
  }

  async function submit(event: globalThis.SubmitEvent): Promise<void> {
    event.preventDefault();
    if (view.type !== 'editing' || csrf === undefined) {
      return;
    }
    view = { type: 'submitting' };
    try {
      const response = await fetch('/api/v1/invitations', {
        method: 'POST',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'content-type': 'application/json',
          'x-csrf-token': csrf,
        },
        body: JSON.stringify({
          email,
          role,
          projects:
            role === 'guest' ? [{ project_id: projectId, role: 'viewer' }] : [],
        }),
        signal: controller.signal,
      });
      if (response.status === 401) {
        view = { type: 'unauthenticated' };
        return;
      }
      if (!response.ok) {
        view = {
          type: 'rejected',
          message: 'O convite não foi emitido.',
        };
        return;
      }
      const payload: unknown = await response.json();
      if (
        typeof payload !== 'object' ||
        payload === null ||
        !('delivery' in payload) ||
        !('invite_url' in payload) ||
        !('token' in payload) ||
        typeof payload.delivery !== 'string' ||
        typeof payload.invite_url !== 'string' ||
        typeof payload.token !== 'string'
      ) {
        view = {
          type: 'rejected',
          message: 'A resposta do convite veio incompleta.',
        };
        return;
      }
      view = {
        type: 'issued',
        delivery: payload.delivery,
        inviteUrl: payload.invite_url,
        token: payload.token,
      };
    } catch {
      if (!controller.signal.aborted) {
        view = {
          type: 'rejected',
          message: 'Não foi possível falar com o Puxiri.',
        };
      }
    }
  }
</script>

<main>
  <h1>Convites</h1>
  <p class="intro">
    Quem entra pelo Muira sem convite vê apenas o aviso de que precisa ser
    convidado. O email sai por um único provedor, quando ele está configurado.
  </p>
  {#if view.type === 'checking' || view.type === 'submitting'}
    <p role="status">Preparando o convite…</p>
  {:else if view.type === 'unauthenticated'}
    <p role="status">Entre na instalação antes de convidar.</p>
    <p><a href="/">Ir para a entrada</a></p>
  {:else if view.type === 'rejected'}
    <p role="status">{view.message}</p>
    <button type="button" onclick={() => (view = { type: 'editing' })}>
      Voltar
    </button>
  {:else if view.type === 'issued'}
    <p role="status">
      {#if view.delivery === 'queued'}
        O provedor aceitou o email na fila. Isso ainda não é entrega.
      {:else if view.delivery === 'unconfigured'}
        Nenhum provedor está configurado. Copie o convite.
      {:else}
        O email não foi confirmado. Copie o convite.
      {/if}
    </p>
    <p>
      Endereço: <a href={view.inviteUrl}>{view.inviteUrl}</a>
    </p>
    <p>Token de uso único: <code>{view.token}</code></p>
  {:else}
    <form onsubmit={(event) => void submit(event)}>
      <label>
        Email
        <input bind:value={email} type="email" autocomplete="off" required />
      </label>
      <label>
        Papel
        <select bind:value={role}>
          <option value="member">Membro</option>
          <option value="guest">Convidado</option>
        </select>
      </label>
      {#if role === 'guest'}
        <label>
          Projeto
          <input bind:value={projectId} autocomplete="off" required />
        </label>
      {/if}
      <button type="submit">Emitir convite</button>
    </form>
  {/if}

  <section aria-labelledby="mcp-grant-title">
    <h2 id="mcp-grant-title">Acesso do MCP</h2>
    <p>
      O Muira só identifica a pessoa. Sem este consentimento, o token não abre
      projeto, item nem ferramenta.
    </p>
    {#if grantView.type === 'saving'}
      <p role="status">Gravando o acesso…</p>
    {:else if grantView.type === 'editing'}
      {#if grantView.notice !== undefined}
        <p role="status">{grantView.notice}</p>
      {/if}
      <p>
        {grantView.configured
          ? 'Já existe um consentimento. Substituí-lo vale na hora.'
          : 'Nenhum consentimento está ativo.'}
      </p>
      <form onsubmit={(event) => void saveGrant(event)}>
        <label>
          Alcance
          <select bind:value={grantScope}>
            <option value="read">Leitura</option>
            <option value="write">Leitura e escrita</option>
          </select>
        </label>
        <label>
          <input bind:checked={allProjects} type="checkbox" />
          Projetos atuais e futuros
        </label>
        {#if !allProjects}
          <label>
            Projeto
            <input bind:value={grantProjectId} autocomplete="off" required />
          </label>
        {/if}
        <button type="submit">Gravar acesso</button>
        {#if grantView.configured}
          <button type="button" onclick={() => void revokeGrant()}>
            Revogar acesso
          </button>
        {/if}
      </form>
    {/if}
  </section>
</main>

<style>
  form {
    display: grid;
    gap: 16px;
    max-width: 32rem;
  }
  label {
    display: grid;
    gap: 8px;
  }
  input,
  select,
  button {
    min-height: 44px;
  }
  code {
    overflow-wrap: anywhere;
  }
</style>
