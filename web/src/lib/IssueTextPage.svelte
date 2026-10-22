<script lang="ts">
  import { flushSync, onMount, tick } from 'svelte';
  import IssueTextForm from './IssueTextForm.svelte';
  import {
    editorRoute,
    IssueTextScreen,
    type EditorState,
  } from './issue-text-screen';
  const route = editorRoute(
    globalThis.location.pathname,
    globalThis.location.search,
  );
  let view = $state<EditorState>({ type: 'idle' });
  let heading: HTMLElement | undefined;
  const screen = new IssueTextScreen(route, (state) => {
    view = state;
  });
  async function action(
    kind: 'save' | 'retry' | 'review' | 'check',
  ): Promise<void> {
    if (kind === 'save') {
      await screen.save();
    } else if (kind === 'retry') {
      await screen.retry();
    } else if (kind === 'review') {
      await screen.review();
    } else {
      await screen.start();
    }
    await tick();
    heading?.focus();
  }
  async function choose(source: 'draft' | 'current'): Promise<void> {
    screen.choose(source);
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

<main class="editor">
  <h1 tabindex="-1" bind:this={heading}>Editar texto da issue</h1>
  <p class="intro">
    Somente título e descrição. Mudanças concorrentes exigem sua revisão antes
    de salvar novamente.
  </p>
  <nav aria-label="Navegação da edição">
    <a href="/projects">Meus projetos</a>{#if route !== undefined}<a
        href={`/projects/${route.project}/issues/${route.number}`}
        >Voltar à issue</a
      >{/if}
  </nav>
  {#if view.type === 'idle' || view.type === 'loading'}
    <p role="status">Verificando sessão, acesso e versão…</p>
  {:else if view.type === 'suspended'}
    <p role="status">
      Dados ocultos até nova verificação. Nenhum pedido será reenviado
      automaticamente.
    </p>
    <button onclick={() => void action('check')}
      >Verificar acesso novamente</button
    >
  {:else if view.type === 'signed-out'}
    <h2>Sessão encerrada ou expirada</h2>
    <p role="status">
      O rascunho foi removido desta página. Login ainda não está disponível
      nesta versão.
    </p>
  {:else if view.type === 'unavailable' || view.type === 'read-only'}
    <h2>Edição indisponível para este acesso</h2>
    <p role="status">Nenhum rascunho privado permanece nesta página.</p>
    <button onclick={() => void action('check')}
      >Verificar acesso novamente</button
    >
  {:else if view.type === 'read-error'}
    <p role="status">
      Não foi possível verificar o acesso. O rascunho fica oculto, sem novo
      envio, até revalidar esta sessão.
    </p>
    <button onclick={() => void action('check')}>Consultar novamente</button>
  {:else if view.type === 'editing'}
    <section data-private-editor>
      <p class="hint">
        Versão de base {view.version}. Rascunho apenas em memória: oculto ao
        sair do foco e perdido ao recarregar ou navegar. Não use dados reais
        nesta versão.
      </p>
      <IssueTextForm
        draft={view.draft}
        version={view.version}
        change={(draft) => screen.change(draft)}
        save={() => action('save')}
      />
    </section>
  {:else if view.type === 'saving'}
    <p role="status">
      Salvando uma solicitação. Uma interrupção não garante cancelamento.
    </p>
  {:else if view.type === 'uncertain'}
    <section data-private-editor>
      <h2>Resultado não confirmado</h2>
      <p role="status">
        O pedido pode ter sido aplicado. Não alteraremos o texto, a versão ou a
        chave. Confirme reenviando somente a mesma solicitação.
      </p>
      <p class="hint">
        Não recarregue nem navegue antes de confirmar: a chave não é guardada
        fora desta página.
      </p>
      <button onclick={() => void action('retry')}
        >Reenviar a mesma edição</button
      >
    </section>
  {:else if view.type === 'conflict'}
    <section data-private-editor>
      <h2>A issue mudou</h2>
      <p role="status">
        Este pedido não alterou a issue. Seu rascunho da versão {view.version} está
        preservado; carregue a versão atual para revisar antes de preparar outro envio.
      </p>
      <h3>Seu rascunho</h3>
      <pre
        data-edit-draft>{`${view.draft.title}\n${view.draft.description}`}</pre>
      <button onclick={() => void action('review')}
        >Carregar versão atual para revisão</button
      >
    </section>
  {:else if view.type === 'review'}
    <section data-private-editor>
      <h2>Revisar versões antes de editar</h2>
      <h3>Seu rascunho</h3>
      <pre
        data-edit-draft>{`${view.draft.title}\n${view.draft.description}`}</pre>
      <h3>Versão atual {view.current.version}</h3>
      <pre
        data-edit-current>{`${view.current.title}\n${view.current.description ?? ''}`}</pre>
      <p class="hint">
        Escolher não envia nada. Depois da escolha, confira os campos e use
        Salvar texto. Outra mudança concorrente ainda pode impedir o envio.
      </p>
      <div class="choices">
        <button onclick={() => void choose('draft')}
          >Usar meu rascunho na versão {view.current.version}</button
        ><button onclick={() => void choose('current')}
          >Descartar rascunho e usar versão atual</button
        >
      </div>
    </section>
  {:else if view.type === 'saved'}
    <section data-private-editor>
      <h2>Edição confirmada</h2>
      <p role="status">
        {view.receipt.changed ? 'Texto salvo' : 'Nenhuma mudança necessária'} · {view
          .receipt.issue_key} · versão {view.receipt.result_version}
      </p>
      <a
        href={`/projects/${view.receipt.project_id}/issues/${view.receipt.issue_number}`}
        >Abrir issue atualizada</a
      >{#if route !== undefined}<a
          href={`/projects/${route.project}/issues/${route.number}/edit`}
          >Editar novamente</a
        >{/if}
    </section>
  {/if}
</main>

<style>
  .editor {
    max-width: 44rem;
    width: 100%;
    min-width: 0;
    text-align: left;
  }
  nav,
  section {
    margin-top: 1.5rem;
  }
  nav,
  .choices {
    display: flex;
    flex-wrap: wrap;
    gap: 0.75rem 1.25rem;
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
  pre {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    max-height: 20rem;
    overflow: auto;
    padding: 1rem;
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    background: var(--surface);
    color: var(--text);
  }
</style>
