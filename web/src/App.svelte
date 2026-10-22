<script lang="ts">
  import { onMount } from 'svelte';
  import { getReadiness, type Readiness } from './lib/health';
  import SessionsPage from './lib/SessionsPage.svelte';
  import WorkPage from './lib/WorkPage.svelte';
  import IssueCreationPage from './lib/IssueCreationPage.svelte';

  const sessionsPage = globalThis.location.pathname === '/settings/sessions';
  const creationPage =
    globalThis.location.pathname.startsWith('/projects/') &&
    globalThis.location.pathname.endsWith('/issues/new');
  const workPage =
    globalThis.location.pathname === '/projects' ||
    globalThis.location.pathname.startsWith('/projects/');

  let status = $state<Readiness | 'checking' | 'unavailable'>('checking');
  const controller = new AbortController();

  async function refresh() {
    status = 'checking';
    try {
      const result = await getReadiness(
        AbortSignal.any([controller.signal, AbortSignal.timeout(8000)]),
      );
      if (!controller.signal.aborted) status = result;
    } catch {
      if (!controller.signal.aborted) status = 'unavailable';
    }
  }

  onMount(() => {
    if (!sessionsPage && !workPage) {
      void refresh();
    }
    return () => controller.abort();
  });
</script>

<div class="shell">
  <header class="brand">
    <svg
      width="36"
      height="36"
      viewBox="0 0 36 36"
      fill="none"
      aria-hidden="true"
    >
      <path d="M7 28C4 12 12 5 27 6C29 20 23 31 7 28Z" fill="currentColor" />
      <path
        d="M10 26C13 18 18 12 25 8"
        stroke="var(--background)"
        stroke-width="2"
      />
    </svg>
    <span>Puxiri</span>
  </header>

  <nav class="account-navigation" aria-label="Conta">
    <a href="/">Instalação</a>
    <a href="/projects" aria-current={workPage ? 'page' : undefined}
      >Meus projetos</a
    >
    <a
      href="/settings/sessions"
      aria-current={sessionsPage ? 'page' : undefined}>Minhas sessões</a
    >
  </nav>

  {#if sessionsPage}
    <SessionsPage />
  {:else if creationPage}
    <IssueCreationPage />
  {:else if workPage}
    <WorkPage />
  {:else}
    <main>
      <h1>Trabalho em movimento.</h1>
      <p class="intro">
        Estamos preparando a base para organizar o trabalho da sua equipe.
      </p>

      <section class="connection" aria-labelledby="connection-title">
        <h2 id="connection-title">Conexão com a instalação</h2>
        <p
          role="status"
          aria-live="polite"
          class="status"
          class:healthy={status === 'ready'}
        >
          {#if status === 'ready'}
            <span aria-hidden="true">✓</span> Servidor pronto
          {:else if status === 'not_ready'}
            <span aria-hidden="true">!</span> Servidor ainda não está pronto
          {:else if status === 'unavailable'}
            <span aria-hidden="true">!</span> Não foi possível conectar
          {:else}
            Verificando conexão…
          {/if}
        </p>
        <p class="hint">
          {#if status === 'not_ready' || status === 'unavailable'}
            Confira se o processo está em execução e tente novamente.
          {:else}
            Esta verificação não mostra dados privados da instalação.
          {/if}
        </p>
        <button onclick={() => void refresh()} disabled={status === 'checking'}>
          Verificar novamente
        </button>
      </section>

      <aside class="notice" aria-label="Disponibilidade do produto">
        <strong>Versão em desenvolvimento</strong>
        <p>
          Login, edição e integrações ainda não estão disponíveis. Não use dados
          reais nesta versão.
        </p>
      </aside>
    </main>

    <footer>Fase 0 · Base técnica em validação</footer>
  {/if}
</div>

<style>
  .account-navigation {
    display: flex;
    flex-wrap: wrap;
    gap: 20px;
    margin-top: 20px;
  }
  .account-navigation a {
    color: var(--primary);
    min-height: 44px;
    display: inline-flex;
    align-items: center;
  }
  .account-navigation a:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 4px;
  }
</style>
