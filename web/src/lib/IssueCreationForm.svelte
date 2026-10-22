<script lang="ts">
  import {
    serializeIssueCreationInput,
    type IssueCreationInput,
  } from './issue-creation';
  let { submit }: { submit: (input: IssueCreationInput) => Promise<void> } =
    $props();
  let title = $state('');
  let description = $state('');
  let error = $state('');

  async function send(event: globalThis.SubmitEvent): Promise<void> {
    event.preventDefault();
    const input = { title, ...(description === '' ? {} : { description }) };
    try {
      serializeIssueCreationInput(input);
    } catch {
      error =
        'Confira o título (1 a 240 caracteres, sem contar espaços nas pontas) e a descrição (até 64 KiB UTF-8).';
      return;
    }
    error = '';
    try {
      await submit(input);
    } catch {
      error =
        'Não foi possível preparar o envio. Confira os campos e tente novamente.';
    }
  }
</script>

<form onsubmit={(event) => void send(event)} data-issue-create-form>
  <label for="issue-title">Título <span class="hint">(obrigatório)</span></label
  >
  <input
    id="issue-title"
    name="title"
    bind:value={title}
    required
    aria-describedby="creation-limits creation-error"
    autocomplete="off"
  />
  <label for="issue-description"
    >Descrição <span class="hint">(opcional)</span></label
  >
  <textarea
    id="issue-description"
    name="description"
    bind:value={description}
    rows="8"
    aria-describedby="creation-limits creation-error"
    autocomplete="off"></textarea>
  <p id="creation-limits" class="hint">
    Até 240 caracteres no título e 64 KiB UTF-8 na descrição. Markdown é
    guardado como texto, sem prévia. A issue começa como tarefa, em A fazer, sem
    responsável.
  </p>
  <p id="creation-error" role="alert">{error}</p>
  <button type="submit">Criar issue</button>
</form>

<style>
  form {
    display: grid;
    gap: 0.75rem;
  }
  label {
    font-weight: 600;
  }
  input,
  textarea {
    box-sizing: border-box;
    width: 100%;
    min-height: 44px;
    padding: 0.75rem;
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    background: var(--background);
    color: var(--text);
    font: inherit;
  }
  textarea {
    resize: vertical;
  }
  input:focus-visible,
  textarea:focus-visible {
    outline: 3px solid var(--primary);
    outline-offset: 3px;
  }
  button {
    justify-self: start;
    min-height: 44px;
  }
  [role='alert'] {
    margin: 0;
  }
</style>
