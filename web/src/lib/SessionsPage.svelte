<script lang="ts">
  import { onMount, tick } from 'svelte';
  import {
    SessionScreen,
    type SessionScreenState,
    type TerminationTarget,
  } from './session-screen';

  let view = $state<SessionScreenState>({ type: 'idle' });
  let container: HTMLElement | undefined;
  const screen = new SessionScreen((state) => {
    view = state;
  });
  const dates = new Intl.DateTimeFormat('pt-BR', {
    dateStyle: 'medium',
    timeStyle: 'short',
    timeZone: 'UTC',
  });

  function date(value: string): string {
    return `${dates.format(new Date(value))} UTC`;
  }

  async function focusHeading(): Promise<void> {
    await tick();
    container?.querySelector<HTMLElement>('[data-view-heading]')?.focus();
  }

  async function choose(id?: string): Promise<void> {
    if (id === undefined) {
      screen.chooseAll();
    } else {
      screen.choose(id);
    }
    await focusHeading();
  }

  async function cancel(target: TerminationTarget): Promise<void> {
    const id = target.type === 'all' ? 'end-all' : `end-${target.session.id}`;
    screen.cancel();
    await tick();
    container?.querySelector<HTMLElement>(`#${id}`)?.focus();
  }

  async function perform(action: () => Promise<void>): Promise<void> {
    await action();
    await focusHeading();
  }

  onMount(() => {
    void screen.start();
    return () => screen.dispose();
  });
</script>

<main class="sessions" bind:this={container}>
  <h1>Minhas sessões</h1>
  <p class="intro">Revise seus acessos ativos e encerre os que não usa.</p>

  {#if view.type === 'idle' || view.type === 'loading'}
    <p role="status">Carregando sessões…</p>
  {:else if view.type === 'ready'}
    {#if view.notice}<p role="status" class="success">{view.notice}</p>{/if}
    <h2 tabindex="-1" data-view-heading>Sessões ativas nesta página</h2>
    <p class="hint">
      Esta lista não registra dispositivo, IP ou localização. As datas estão em
      UTC.
    </p>
    {#if view.page.data.length === 0}
      <p role="status">Nenhuma sessão ativa nesta página.</p>
    {:else}
      <ul class="session-list" aria-label="Sessões ativas">
        {#each view.page.data as session (session.id)}
          <li data-session-id={session.id}>
            <div>
              <h3>{session.is_current ? 'Sessão atual' : 'Outra sessão'}</h3>
              <p class="reference">Referência {session.id.slice(-8)}</p>
              <dl>
                <div>
                  <dt>Iniciada</dt>
                  <dd>
                    <time datetime={session.created_at}
                      >{date(session.created_at)}</time
                    >
                  </dd>
                </div>
                <div>
                  <dt>Última atividade</dt>
                  <dd>
                    <time datetime={session.last_activity_at}
                      >{date(session.last_activity_at)}</time
                    >
                  </dd>
                </div>
                <div>
                  <dt>Expiração</dt>
                  <dd>
                    <time datetime={session.expires_at}
                      >{date(session.expires_at)}</time
                    >
                  </dd>
                </div>
              </dl>
            </div>
            <button
              class="secondary"
              id={`end-${session.id}`}
              onclick={() => void choose(session.id)}
              aria-label={session.is_current
                ? 'Encerrar sessão atual'
                : `Encerrar sessão ${session.id.slice(-8)}`}
              >Encerrar sessão</button
            >
          </li>
        {/each}
      </ul>
    {/if}
    <nav class="page-actions" aria-label="Paginação de sessões">
      <button
        class="secondary"
        onclick={() => void perform(() => screen.refresh())}
        >Voltar ao início</button
      >
      {#if view.page.has_more}<button
          onclick={() => void perform(() => screen.next())}
          >Próxima página</button
        >{/if}
    </nav>
    <section class="global-action" aria-labelledby="end-all-title">
      <h2 id="end-all-title">Encerrar todos os acessos</h2>
      <p>
        Encerra todas as suas sessões, incluindo a que você está usando agora.
      </p>
      <button class="secondary" id="end-all" onclick={() => void choose()}
        >Encerrar todas as sessões</button
      >
    </section>
  {:else if view.type === 'confirm'}
    {@const target = view.target}
    <section class="confirmation" aria-labelledby="confirmation-title">
      <h2 id="confirmation-title" tabindex="-1" data-view-heading>
        {target.type === 'all'
          ? 'Encerrar todas as sessões?'
          : target.session.is_current
            ? 'Encerrar a sessão atual?'
            : 'Encerrar esta sessão?'}
      </h2>
      {#if target.type === 'all'}
        <p>
          Todos os seus acessos serão encerrados. Você também sairá desta
          sessão.
        </p>
      {:else}
        <p>
          Referência {target.session.id.slice(-8)}. Iniciada em {date(
            target.session.created_at,
          )}.
        </p>
        <p>
          {target.session.is_current
            ? 'Você sairá desta sessão ao confirmar.'
            : 'As outras sessões continuarão ativas.'}
        </p>
      {/if}
      <div class="page-actions">
        <button class="secondary" onclick={() => void cancel(target)}
          >Cancelar</button
        >
        <button
          class="danger"
          onclick={() => void perform(() => screen.submit())}
          >{target.type === 'all'
            ? 'Confirmar encerramento de todas'
            : 'Confirmar encerramento'}</button
        >
      </div>
    </section>
  {:else if view.type === 'submitting'}
    <p role="status">Encerrando sessões… Aguarde a confirmação.</p>
  {:else if view.type === 'signed-out'}
    <h2 tabindex="-1" data-view-heading>
      {view.reason === 'completed'
        ? 'Encerramento concluído'
        : 'Sua sessão não está ativa'}
    </h2>
    <p role="status">
      {view.reason === 'completed'
        ? 'Este acesso foi encerrado. A lista privada foi removida desta tela.'
        : 'Não é possível mostrar os acessos sem uma sessão ativa.'}
    </p>
    <a href="/">Voltar à instalação</a>
  {:else if view.type === 'unavailable'}
    <h2 tabindex="-1" data-view-heading>Gestão de sessões não habilitada</h2>
    <p role="status">
      Este recurso ainda não está habilitado nesta instalação.
    </p>
    <button
      class="secondary"
      onclick={() => void perform(() => screen.refresh())}
      >Consultar novamente</button
    >
  {:else if view.type === 'mutation-uncertain'}
    <h2 tabindex="-1" data-view-heading>Encerramento não confirmado</h2>
    <p role="alert">
      Não foi possível confirmar o resultado. Consulte as sessões antes de
      decidir se deve encerrá-las novamente.
    </p>
    <button onclick={() => void perform(() => screen.refresh())}
      >Consultar sessões</button
    >
  {:else}
    {#if view.notice}<p role="status" class="success">{view.notice}</p>{/if}
    <h2 tabindex="-1" data-view-heading>
      Não foi possível carregar as sessões
    </h2>
    <p role="alert">
      Confira sua conexão e consulte novamente. Nenhum encerramento será
      repetido automaticamente.
    </p>
    <button onclick={() => void perform(() => screen.refresh())}
      >Consultar novamente</button
    >
  {/if}
</main>

<style>
  .sessions {
    --danger: #a52b32;
    --on-danger: #ffffff;
  }
  h2:focus {
    outline: 2px solid var(--border);
    outline-offset: 6px;
  }
  h3 {
    margin: 0;
    font-size: 1rem;
    font-weight: 600;
  }
  .session-list {
    list-style: none;
    padding: 0;
    margin: 24px 0;
  }
  li {
    display: flex;
    justify-content: space-between;
    align-items: start;
    gap: 20px;
    padding: 24px 0;
    border-top: 1px solid var(--border);
  }
  li:last-child {
    border-bottom: 1px solid var(--border);
  }
  .reference {
    color: var(--muted);
    font-size: 0.8125rem;
    margin: 8px 0 16px;
  }
  dl {
    margin: 0;
    font-size: 0.875rem;
    line-height: 1.7;
  }
  dl div {
    display: grid;
    grid-template-columns: 120px 1fr;
    gap: 12px;
  }
  dt {
    color: var(--muted);
  }
  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }
  .page-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    align-items: center;
  }
  .secondary {
    color: var(--text);
    background: var(--surface);
    border: 1px solid var(--border);
  }
  .danger {
    color: var(--on-danger);
    background: var(--danger);
  }
  .global-action {
    margin-top: 48px;
    padding-top: 24px;
    border-top: 1px solid var(--border);
  }
  .global-action h2,
  .confirmation h2 {
    font-size: 1.25rem;
  }
  .success {
    color: var(--primary);
  }
  a {
    color: var(--primary);
    display: inline-flex;
    align-items: center;
    min-height: 44px;
  }
  a:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 4px;
  }
  @media (prefers-color-scheme: dark) {
    .sessions {
      --danger: #ffb4b7;
      --on-danger: #3b1017;
    }
  }
  @media (max-width: 540px) {
    li {
      flex-direction: column;
    }
    dl div {
      grid-template-columns: 104px 1fr;
      gap: 8px;
    }
  }
</style>
