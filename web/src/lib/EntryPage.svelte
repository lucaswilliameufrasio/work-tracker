<script lang="ts">
  import { onMount } from 'svelte';
  import {
    completePuxiriSetup,
    EntryRequestError,
    entryFailureMessage,
    loginPuxiri,
    startMuiraLogin,
    startMuiraSetup,
    type InstallationStatus,
  } from './entry.ts';

  let { status }: { status: InstallationStatus } = $props();

  type Provider = 'muira' | 'puxiri';
  type SubmitState =
    | { type: 'idle' }
    | { type: 'submitting' }
    | { type: 'rejected'; message: string };

  let provider = $state<Provider>('muira');
  let submitState = $state<SubmitState>({ type: 'idle' });
  let failure = $state<string | undefined>(undefined);
  let setupToken = $state('');
  let organizationName = $state('');
  let locale = $state<'pt-BR' | 'en'>('pt-BR');
  let timezone = $state('America/Sao_Paulo');
  let displayName = $state('');
  let email = $state('');
  let password = $state('');
  let issuer = $state('');
  let clientId = $state('');
  let clientSecret = $state('');
  const controller = new AbortController();

  onMount(() => {
    failure = entryFailureMessage(globalThis.location.search);
    return () => controller.abort();
  });

  function rejected(error: unknown): SubmitState {
    if (error instanceof EntryRequestError) {
      if (error.status === 401) {
        return {
          type: 'rejected',
          message: 'Esses dados não foram aceitos.',
        };
      }
      if (error.status === 409) {
        return {
          type: 'rejected',
          message: 'Esta instalação não aceita outro setup agora.',
        };
      }
      if (error.status === 422) {
        return {
          type: 'rejected',
          message: 'Confira os campos e tente de novo.',
        };
      }
      if (error.status === 503) {
        return {
          type: 'rejected',
          message: 'A instalação está temporariamente indisponível.',
        };
      }
    }
    return {
      type: 'rejected',
      message: 'Não foi possível concluir o acesso.',
    };
  }

  async function submit(): Promise<void> {
    submitState = { type: 'submitting' };
    try {
      if (status.setup_required) {
        if (provider === 'muira') {
          const url = await startMuiraSetup(
            {
              setup_token: setupToken,
              organization_name: organizationName,
              locale,
              timezone,
              display_name: displayName,
              email,
              issuer,
              client_id: clientId,
              client_secret: clientSecret,
            },
            controller.signal,
          );
          globalThis.location.assign(url);
          return;
        }
        await completePuxiriSetup(
          {
            setup_token: setupToken,
            organization_name: organizationName,
            locale,
            timezone,
            display_name: displayName,
            email,
            password,
          },
          controller.signal,
        );
        globalThis.location.assign('/projects');
        return;
      }
      if (status.login === 'muira') {
        const url = await startMuiraLogin(controller.signal);
        globalThis.location.assign(url);
        return;
      }
      await loginPuxiri(email, password, controller.signal);
      globalThis.location.assign('/projects');
    } catch (error) {
      if (!controller.signal.aborted) {
        submitState = rejected(error);
      }
    }
  }
</script>

<section class="entry" aria-labelledby="entry-title">
  <h2 id="entry-title">
    {status.setup_required ? 'Configurar a instalação' : 'Entrar'}
  </h2>
  <p>
    O Muira confirma quem é a pessoa. O Puxiri decide o que ela pode ver. Sem
    convite, a entrada pelo Muira não abre projetos.
  </p>
  {#if failure !== undefined}
    <p role="alert">{failure}</p>
  {/if}
  {#if submitState.type === 'rejected'}
    <p role="alert">{submitState.message}</p>
  {/if}
  <form
    onsubmit={(event) => {
      event.preventDefault();
      void submit();
    }}
  >
    {#if status.setup_required}
      <fieldset>
        <legend>Como as pessoas vão entrar</legend>
        <label>
          <input
            type="radio"
            name="provider"
            value="muira"
            bind:group={provider}
          />
          Muira
        </label>
        <label>
          <input
            type="radio"
            name="provider"
            value="puxiri"
            bind:group={provider}
          />
          Senha desta instalação
        </label>
      </fieldset>
      <label>
        Token de setup
        <input
          name="setup_token"
          autocomplete="off"
          required
          bind:value={setupToken}
        />
      </label>
      <label>
        Nome da organização
        <input
          name="organization_name"
          required
          bind:value={organizationName}
        />
      </label>
      <label>
        Idioma
        <select name="locale" bind:value={locale}>
          <option value="pt-BR">Português (Brasil)</option>
          <option value="en">English</option>
        </select>
      </label>
      <label>
        Fuso horário
        <input name="timezone" required bind:value={timezone} />
      </label>
      <label>
        Seu nome
        <input
          name="display_name"
          autocomplete="name"
          required
          bind:value={displayName}
        />
      </label>
      <label>
        E-mail
        <input
          name="email"
          type="email"
          autocomplete="username"
          required
          bind:value={email}
        />
      </label>
      {#if provider === 'muira'}
        <label>
          Emissor do Muira
          <input name="issuer" type="url" required bind:value={issuer} />
        </label>
        <label>
          Identificador do cliente
          <input name="client_id" required bind:value={clientId} />
        </label>
        <label>
          Segredo do cliente
          <input
            name="client_secret"
            type="password"
            autocomplete="off"
            required
            bind:value={clientSecret}
          />
        </label>
      {:else}
        <label>
          Senha
          <input
            name="password"
            type="password"
            autocomplete="new-password"
            required
            bind:value={password}
          />
        </label>
      {/if}
    {:else if status.login === 'muira'}
      <p>A instalação usa o Muira para confirmar a sua identidade.</p>
    {:else}
      <label>
        E-mail
        <input
          name="email"
          type="email"
          autocomplete="username"
          required
          bind:value={email}
        />
      </label>
      <label>
        Senha
        <input
          name="password"
          type="password"
          autocomplete="current-password"
          required
          bind:value={password}
        />
      </label>
    {/if}
    <button type="submit" disabled={submitState.type === 'submitting'}>
      {status.setup_required ? 'Continuar' : 'Entrar'}
    </button>
  </form>
  <p>Não use dados reais nesta versão.</p>
</section>

<style>
  .entry {
    margin-top: 32px;
    padding: 20px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 8px;
  }
  fieldset {
    border: 1px solid var(--border);
    border-radius: 8px;
    margin: 16px 0;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 14px;
    font-weight: 600;
    margin-top: 16px;
  }
  input,
  select {
    font: inherit;
    min-height: 44px;
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 8px 12px;
    background: var(--surface);
    color: var(--text);
  }
  input:focus-visible,
  select:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 4px;
  }
</style>
