<script lang="ts">
  import { untrack } from 'svelte';
  import { serializeIssueTextInput } from './issue-text';
  import type { TextDraft } from './issue-text-screen';
  let {
    draft,
    version,
    change,
    save,
  }: {
    draft: TextDraft;
    version: string;
    change: (draft: TextDraft) => void;
    save: () => Promise<void>;
  } = $props();
  let title = $state(untrack(() => draft.title));
  let description = $state(untrack(() => draft.description));
  let error = $state('');
  async function submit(event: globalThis.SubmitEvent): Promise<void> {
    event.preventDefault();
    try {
      serializeIssueTextInput({
        title,
        ...(description === '' ? {} : { description }),
        expected_version: version,
      });
      change({ title, description });
      error = '';
      await save();
    } catch {
      error =
        'Confira o título (1 a 240 caracteres) e a descrição (até 64 KiB UTF-8). Nenhum novo envio foi preparado.';
    }
  }
</script>

<form onsubmit={(event) => void submit(event)} data-issue-text-form>
  <label for="edit-title">Título <span class="hint">(obrigatório)</span></label>
  <input
    id="edit-title"
    name="title"
    bind:value={title}
    oninput={(event) =>
      change({ title: event.currentTarget.value, description })}
    required
    autocomplete="off"
    aria-describedby="edit-limits edit-error"
  />
  <label for="edit-description"
    >Descrição <span class="hint">(opcional)</span></label
  >
  <textarea
    id="edit-description"
    name="description"
    bind:value={description}
    oninput={(event) =>
      change({ title, description: event.currentTarget.value })}
    rows="8"
    autocomplete="off"
    aria-describedby="edit-limits edit-error"></textarea>
  <p id="edit-limits" class="hint">
    Até 240 caracteres no título e 64 KiB UTF-8 na descrição. Descrição vazia é
    removida. Markdown permanece texto, sem prévia.
  </p>
  <p id="edit-error" role="alert">{error}</p>
  <button type="submit">Salvar texto</button>
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
