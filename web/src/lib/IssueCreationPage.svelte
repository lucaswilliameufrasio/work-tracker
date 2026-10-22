<script lang="ts">
  import { flushSync, onMount, tick } from 'svelte';
  import IssueCreationForm from './IssueCreationForm.svelte';
  import {
    creationProject,
    IssueCreationScreen,
    type CreationState,
  } from './issue-creation-screen';
  import type { IssueCreationInput } from './issue-creation';

  const project = creationProject(
    globalThis.location.pathname,
    globalThis.location.search,
  );
  let view = $state<CreationState>({ type: 'idle' });
  let heading: HTMLElement | undefined;
  const screen = new IssueCreationScreen(project, (state) => {
    view = state;
  });

  async function submit(input: IssueCreationInput): Promise<void> {
    await screen.submit(input);
    await tick();
    heading?.focus();
  }
  async function retry(): Promise<void> {
    await screen.retry();
    await tick();
    heading?.focus();
  }
  async function check(): Promise<void> {
    await screen.start();
    await tick();
    heading?.focus();
  }

  onMount(() => {
    const conceal = () => flushSync(() => screen.suspend());
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
    globalThis.addEventListener('blur', conceal);
    globalThis.addEventListener('pagehide', conceal);
    globalThis.addEventListener('focus', restore);
    globalThis.addEventListener('pageshow', pageShow);
    document.addEventListener('visibilitychange', visibility);
    if (document.visibilityState === 'visible') {
      void screen.start();
    } else {
      screen.suspend();
    }
    return () => {
      globalThis.removeEventListener('blur', conceal);
      globalThis.removeEventListener('pagehide', conceal);
      globalThis.removeEventListener('focus', restore);
      globalThis.removeEventListener('pageshow', pageShow);
      document.removeEventListener('visibilitychange', visibility);
      screen.dispose();
    };
  });
</script>

<main class="creation">
  <h1 tabindex="-1" bind:this={heading}>Criar issue</h1>
  <p class="intro">
    Criação em desenvolvimento. Sua permissão de membro ou gerente e o estado do
    projeto serão verificados no envio.
  </p>
  <nav aria-label="Navegação da criação">
    <a href="/projects">Meus projetos</a>
    {#if project !== undefined}<a href={`/projects/${project}/issues`}
        >Voltar à lista de issues</a
      >{/if}
  </nav>
  {#if view.type === 'idle' || view.type === 'checking'}
    <p role="status">Verificando sessão e acesso…</p>
  {:else if view.type === 'suspended'}
    <p role="status">
      Dados ocultos até nova verificação. Rascunhos não enviados são descartados
      ao sair do foco.
    </p>
    <button onclick={() => void check()}>Verificar acesso novamente</button>
  {:else if view.type === 'signed-out'}
    <h2>Sessão encerrada ou expirada</h2>
    <p role="status">
      Nenhum dado privado é exibido. Login ainda não está disponível nesta
      versão.
    </p>
  {:else if view.type === 'unavailable'}
    <h2>Conteúdo indisponível</h2>
    <p role="status">O endereço não está disponível para este acesso.</p>
    <button onclick={() => void check()}>Verificar acesso novamente</button>
  {:else if view.type === 'read-error'}
    <p role="status">
      Não foi possível verificar o acesso. Nenhuma solicitação foi reenviada.
    </p>
    <button onclick={() => void check()}>Consultar novamente</button>
  {:else if view.type === 'editing'}
    <section data-private-creation aria-label="Nova issue">
      <p class="hint">
        Rascunho apenas nesta página: ao perder foco, ele será descartado. Não
        use dados reais nesta versão.
      </p>
      <IssueCreationForm {submit} />
    </section>
  {:else if view.type === 'submitting'}
    <p role="status">
      Enviando uma solicitação. Não inicie outra criação; uma interrupção não
      garante cancelamento.
    </p>
  {:else if view.type === 'uncertain'}
    <section data-private-creation>
      <h2>Resultado não confirmado</h2>
      <p role="status">
        A issue pode ter sido criada, ou o servidor pode ter recusado o envio.
        Não iniciaremos outra criação nem mudaremos a chave. O botão abaixo
        reenvia apenas a mesma solicitação para confirmar o resultado.
      </p>
      <p class="hint">
        Não recarregue nem saia desta página antes de confirmar: a solicitação e
        sua chave não são guardadas após a navegação. Confira também a lista de
        issues para evitar duplicação manual.
      </p>
      <button onclick={() => void retry()}>Reenviar a mesma solicitação</button>
    </section>
  {:else if view.type === 'confirmed'}
    <section data-private-creation>
      <h2>Criação confirmada</h2>
      <p role="status">{view.receipt.issue_key}</p>
      <a
        href={`/projects/${view.receipt.project_id}/issues/${view.receipt.issue_number}`}
        >Abrir issue criada</a
      >
      {#if project !== undefined}<a href={`/projects/${project}/issues/new`}
          >Criar outra issue</a
        >{/if}
    </section>
  {/if}
</main>

<style>
  .creation {
    max-width: 44rem;
    width: 100%;
    min-width: 0;
    text-align: left;
  }
  nav,
  section {
    margin-top: 1.5rem;
  }
  nav {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem 1.25rem;
  }
  a {
    color: var(--primary);
    display: inline-flex;
    align-items: center;
    min-height: 44px;
    margin-right: 1rem;
  }
  a:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 3px;
  }
  button {
    min-height: 44px;
  }
  h1:focus {
    outline: none;
  }
</style>
