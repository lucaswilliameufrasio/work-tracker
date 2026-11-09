<script lang="ts">
  import { onMount } from 'svelte';
  import { getCurrentSession, SessionRequestError } from './sessions.ts';

  type Replication = {
    snapshot: boolean;
    txid: string | undefined;
    sameFilesystem: boolean;
  };

  type Person = {
    userId: string;
    displayName: string;
    organizationRole: string;
    state: string;
    installationAdmin: boolean;
    primaryEmail: string | undefined;
  };

  type Project = {
    id: string;
    projectKey: string;
    name: string;
    archivedAt: string | undefined;
    membershipCount: number;
    issueCount: number;
  };

  type AuditRow = {
    id: string;
    eventKind: string;
    result: string;
    actorUserId: string | undefined;
    targetKind: string;
    targetId: string;
    reason: string | undefined;
    occurredAt: string;
  };

  type Panel = {
    loginSource: string | undefined;
    mailConfigured: boolean;
    replication: Replication;
    people: Person[];
    projects: Project[];
    projectsContinue: boolean;
    audit: AuditRow[];
    notice: string | undefined;
  };

  type View =
    | { type: 'checking' }
    | { type: 'unauthenticated' }
    | { type: 'forbidden' }
    | { type: 'failed'; message: string }
    | { type: 'ready'; panel: Panel }
    | {
        type: 'working';
        panel: Panel;
        action: 'capture' | 'restore' | 'mail' | 'ban' | 'unban';
      };

  let view = $state<View>({ type: 'checking' });
  let csrf = $state<string | undefined>(undefined);
  let provider = $state('sabia');
  let apiKey = $state('');
  let fromAddress = $state('');
  let fromName = $state('');
  let template = $state('');
  let origin = $state('');
  let banReason = $state('');
  const controller = new AbortController();

  onMount(() => {
    void load();
    return () => {
      controller.abort();
    };
  });

  function record(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
  }

  function text(value: unknown): string | undefined {
    if (typeof value !== 'string' || value.length === 0) {
      return undefined;
    }
    return value;
  }

  async function load(): Promise<View> {
    try {
      const session = await getCurrentSession(controller.signal);
      csrf = session.csrf_token;
      const [overview, people, catalog, audit, mail] = await Promise.all([
        getJson('/api/v1/admin/overview'),
        getJson('/api/v1/admin/people'),
        getJson('/api/v1/admin/catalog'),
        getJson('/api/v1/admin/audit'),
        getJson('/api/v1/mail/sender'),
      ]);
      if (controller.signal.aborted) {
        return view;
      }
      if (
        overview === 'unauthenticated' ||
        people === 'unauthenticated' ||
        catalog === 'unauthenticated' ||
        audit === 'unauthenticated' ||
        mail === 'unauthenticated'
      ) {
        view = { type: 'unauthenticated' };
        return view;
      }
      if (
        overview === 'forbidden' ||
        people === 'forbidden' ||
        catalog === 'forbidden' ||
        audit === 'forbidden'
      ) {
        view = { type: 'forbidden' };
        return view;
      }
      if (
        overview === 'failed' ||
        people === 'failed' ||
        catalog === 'failed' ||
        audit === 'failed' ||
        mail === 'failed' ||
        !record(overview) ||
        !record(people) ||
        !record(catalog) ||
        !record(audit) ||
        !record(mail)
      ) {
        view = {
          type: 'failed',
          message: 'Não foi possível abrir a gestão.',
        };
        return view;
      }
      const panel = parsePanel(overview, people, catalog, audit);
      if (panel === undefined) {
        view = {
          type: 'failed',
          message: 'A resposta da gestão veio incompleta.',
        };
        return view;
      }
      applyMail(mail);
      view = { type: 'ready', panel };
      return view;
    } catch (error) {
      if (controller.signal.aborted) {
        return view;
      }
      if (error instanceof SessionRequestError && error.status === 401) {
        view = { type: 'unauthenticated' };
        return view;
      }
      view = {
        type: 'failed',
        message: 'Não foi possível confirmar a sessão.',
      };
      return view;
    }
  }

  function parsePanel(
    overview: Record<string, unknown>,
    people: Record<string, unknown>,
    catalog: Record<string, unknown>,
    audit: Record<string, unknown>,
  ): Panel | undefined {
    if (
      typeof overview.mail_configured !== 'boolean' ||
      !record(overview.replication) ||
      typeof overview.replication.snapshot !== 'boolean' ||
      typeof overview.replication.same_filesystem !== 'boolean' ||
      !Array.isArray(people.data) ||
      typeof people.has_more !== 'boolean' ||
      !Array.isArray(catalog.data) ||
      typeof catalog.has_more !== 'boolean' ||
      !Array.isArray(audit.data) ||
      typeof audit.has_more !== 'boolean'
    ) {
      return undefined;
    }
    const parsedPeople: Person[] = [];
    for (const item of people.data) {
      const person = parsePerson(item);
      if (person === undefined) {
        return undefined;
      }
      parsedPeople.push(person);
    }
    const projects: Project[] = [];
    for (const item of catalog.data) {
      const project = parseProject(item);
      if (project === undefined) {
        return undefined;
      }
      projects.push(project);
    }
    const rows: AuditRow[] = [];
    for (const item of audit.data) {
      const row = parseAudit(item);
      if (row === undefined) {
        return undefined;
      }
      rows.push(row);
    }
    const loginSource = text(overview.login_source);
    if (
      loginSource !== undefined &&
      loginSource !== 'puxiri' &&
      loginSource !== 'muira'
    ) {
      return undefined;
    }
    return {
      loginSource,
      mailConfigured: overview.mail_configured,
      replication: {
        snapshot: overview.replication.snapshot,
        txid: text(overview.replication.txid),
        sameFilesystem: overview.replication.same_filesystem,
      },
      people: parsedPeople,
      projects,
      projectsContinue: catalog.has_more,
      audit: rows,
      notice: undefined,
    };
  }

  function parsePerson(value: unknown): Person | undefined {
    if (
      !record(value) ||
      typeof value.user_id !== 'string' ||
      typeof value.display_name !== 'string' ||
      typeof value.organization_role !== 'string' ||
      typeof value.state !== 'string' ||
      typeof value.installation_admin !== 'boolean'
    ) {
      return undefined;
    }
    return {
      userId: value.user_id,
      displayName: value.display_name,
      organizationRole: value.organization_role,
      state: value.state,
      installationAdmin: value.installation_admin,
      primaryEmail: text(value.primary_email),
    };
  }

  function parseProject(value: unknown): Project | undefined {
    if (
      !record(value) ||
      typeof value.id !== 'string' ||
      typeof value.project_key !== 'string' ||
      typeof value.name !== 'string' ||
      typeof value.membership_count !== 'number' ||
      typeof value.issue_count !== 'number'
    ) {
      return undefined;
    }
    return {
      id: value.id,
      projectKey: value.project_key,
      name: value.name,
      archivedAt: text(value.archived_at),
      membershipCount: value.membership_count,
      issueCount: value.issue_count,
    };
  }

  function parseAudit(value: unknown): AuditRow | undefined {
    if (
      !record(value) ||
      typeof value.id !== 'string' ||
      typeof value.event_kind !== 'string' ||
      typeof value.result !== 'string' ||
      typeof value.target_kind !== 'string' ||
      typeof value.target_id !== 'string' ||
      typeof value.occurred_at !== 'string'
    ) {
      return undefined;
    }
    return {
      id: value.id,
      eventKind: value.event_kind,
      result: value.result,
      actorUserId: text(value.actor_user_id),
      targetKind: value.target_kind,
      targetId: value.target_id,
      reason: text(value.reason),
      occurredAt: value.occurred_at,
    };
  }

  function applyMail(mail: Record<string, unknown>): void {
    if (mail.configured !== true) {
      return;
    }
    const selected = text(mail.provider);
    if (
      selected === 'sabia' ||
      selected === 'resend' ||
      selected === 'millionsend'
    ) {
      provider = selected;
    }
    const address = text(mail.from_address);
    if (address !== undefined) {
      fromAddress = address;
    }
    const name = text(mail.from_name);
    if (name !== undefined) {
      fromName = name;
    }
    const selectedTemplate = text(mail.template);
    if (selectedTemplate !== undefined) {
      template = selectedTemplate;
    }
    const selectedOrigin = text(mail.origin);
    if (selectedOrigin !== undefined) {
      origin = selectedOrigin;
    }
  }

  async function getJson(
    path: string,
  ): Promise<
    Record<string, unknown> | 'unauthenticated' | 'forbidden' | 'failed'
  > {
    try {
      const response = await fetch(path, {
        credentials: 'same-origin',
        cache: 'no-store',
        signal: controller.signal,
      });
      if (response.status === 401) {
        return 'unauthenticated';
      }
      if (response.status === 403) {
        return 'forbidden';
      }
      if (!response.ok) {
        return 'failed';
      }
      const payload: unknown = await response.json();
      if (!record(payload)) {
        return 'failed';
      }
      return payload;
    } catch {
      if (controller.signal.aborted) {
        return 'failed';
      }
      return 'failed';
    }
  }

  function panelOf(current: View): Panel | undefined {
    if (current.type === 'ready' || current.type === 'working') {
      return current.panel;
    }
    return undefined;
  }

  async function mutate(
    action: 'capture' | 'restore' | 'mail' | 'ban' | 'unban',
    path: string,
    body: Record<string, string>,
    success: string,
  ): Promise<void> {
    const panel = panelOf(view);
    if (panel === undefined || csrf === undefined || view.type === 'working') {
      return;
    }
    view = { type: 'working', panel, action };
    try {
      const response = await fetch(path, {
        method: 'POST',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'content-type': 'application/json',
          'x-csrf-token': csrf,
        },
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      if (response.status === 401) {
        view = { type: 'unauthenticated' };
        return;
      }
      if (!response.ok) {
        view = {
          type: 'ready',
          panel: { ...panel, notice: await failureMessage(response) },
        };
        return;
      }
      const payload: unknown = await response.json();
      let notice = success;
      if (
        action === 'restore' &&
        record(payload) &&
        typeof payload.file_name === 'string'
      ) {
        notice = `A cópia foi gravada como ${payload.file_name}. O banco em uso não foi trocado.`;
      }
      if (
        action === 'capture' &&
        record(payload) &&
        typeof payload.txid === 'string'
      ) {
        notice = `Captura gravada no ponto ${payload.txid}.`;
      }
      const loaded = await load();
      if (loaded.type === 'ready') {
        view = { type: 'ready', panel: { ...loaded.panel, notice } };
      }
    } catch {
      if (!controller.signal.aborted) {
        view = {
          type: 'ready',
          panel: { ...panel, notice: 'Não foi possível falar com o Puxiri.' },
        };
      }
    }
  }

  async function failureMessage(
    response: globalThis.Response,
  ): Promise<string> {
    try {
      const payload: unknown = await response.json();
      if (record(payload) && typeof payload.message === 'string') {
        return payload.message;
      }
    } catch {
      return 'A operação não foi concluída.';
    }
    return 'A operação não foi concluída.';
  }

  async function saveMail(event: globalThis.SubmitEvent): Promise<void> {
    event.preventDefault();
    const panel = panelOf(view);
    if (panel === undefined || csrf === undefined || view.type === 'working') {
      return;
    }
    const body: Record<string, string> = { provider, api_key: apiKey };
    if (provider === 'sabia') {
      body.template = template;
      body.origin = origin;
    } else {
      body.from_address = fromAddress;
      body.from_name = fromName;
    }
    view = { type: 'working', panel, action: 'mail' };
    try {
      const response = await fetch('/api/v1/mail/sender', {
        method: 'PUT',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'content-type': 'application/json',
          'x-csrf-token': csrf,
        },
        body: JSON.stringify(body),
        signal: controller.signal,
      });
      apiKey = '';
      if (response.status === 401) {
        view = { type: 'unauthenticated' };
        return;
      }
      if (!response.ok) {
        view = {
          type: 'ready',
          panel: { ...panel, notice: await failureMessage(response) },
        };
        return;
      }
      const loaded = await load();
      if (loaded.type === 'ready') {
        view = {
          type: 'ready',
          panel: {
            ...loaded.panel,
            notice:
              'O remetente de email foi substituído. A chave não fica visível.',
          },
        };
      }
    } catch {
      if (!controller.signal.aborted) {
        view = {
          type: 'ready',
          panel: { ...panel, notice: 'Não foi possível falar com o Puxiri.' },
        };
      }
    }
  }

  function roleLabel(role: string): string {
    if (role === 'owner') {
      return 'dono';
    }
    if (role === 'admin') {
      return 'administrador da organização';
    }
    if (role === 'guest') {
      return 'convidado';
    }
    return 'membro';
  }

  function stateLabel(state: string): string {
    if (state === 'banned') {
      return 'banido';
    }
    if (state === 'disabled') {
      return 'desativado';
    }
    return 'ativo';
  }
</script>

<main class="management">
  <h1>Gestão</h1>
  <p class="intro">
    Administração da instalação. Quem é só dono da organização não configura
    email, pessoas nem a recuperação. O conteúdo dos itens não aparece aqui.
  </p>

  {#if view.type === 'checking'}
    <p role="status">Abrindo a gestão…</p>
  {:else if view.type === 'unauthenticated'}
    <p role="status">Entre na instalação antes de administrar.</p>
    <p><a href="/">Ir para a entrada</a></p>
  {:else if view.type === 'forbidden'}
    <p role="status">
      Esta conta não é administradora da instalação. Ser dono da organização não
      abre esta tela.
    </p>
  {:else if view.type === 'failed'}
    <p role="status">{view.message}</p>
  {:else}
    {@const panel = view.panel}
    {@const busy = view.type === 'working'}
    {#if panel.notice !== undefined}
      <p class="notice" role="status">{panel.notice}</p>
    {/if}

    <section aria-labelledby="overview-title">
      <h2 id="overview-title">Instalação</h2>
      <p>
        Entrada:
        {#if panel.loginSource === 'muira'}
          Muira
        {:else if panel.loginSource === 'puxiri'}
          Puxiri
        {:else}
          ainda não escolhida
        {/if}
      </p>
      <p>
        Email:
        {panel.mailConfigured ? 'um remetente configurado' : 'nenhum remetente'}
      </p>
      <p>
        Replicação:
        {#if panel.replication.snapshot}
          snapshot presente{panel.replication.txid === undefined
            ? ''
            : `, ponto ${panel.replication.txid}`}
        {:else}
          ainda sem snapshot
        {/if}
      </p>
      {#if panel.replication.sameFilesystem}
        <p class="hint">
          A réplica está no mesmo sistema de arquivos do banco. Ela não
          sobrevive à perda desse disco. Aponte
          <code>PUXIRI_REPLICATION_DIR</code> para outro disco.
        </p>
      {/if}
      <p class="hint">
        A captura não envia o banco para um armazenamento remoto e não promete
        resistir a queda de energia. A restauração grava um arquivo novo. Pare o
        Puxiri antes de substituir o banco em uso por essa cópia.
      </p>
      <button
        type="button"
        disabled={busy}
        onclick={() =>
          void mutate(
            'capture',
            '/api/v1/admin/replication/capture',
            {},
            'Captura gravada.',
          )}
      >
        Capturar agora
      </button>
      <button
        type="button"
        disabled={busy}
        onclick={() =>
          void mutate(
            'restore',
            '/api/v1/admin/replication/restore',
            {},
            'Cópia gravada.',
          )}
      >
        Restaurar em arquivo novo
      </button>
    </section>

    <section aria-labelledby="people-title">
      <h2 id="people-title">Pessoas</h2>
      {#if panel.people.length === 0}
        <p>Nenhuma pessoa nesta instalação.</p>
      {/if}
      {#each panel.people as person (person.userId)}
        <article class="person">
          <h3>{person.displayName}</h3>
          <p>
            {roleLabel(person.organizationRole)} · {stateLabel(person.state)}
            {#if person.installationAdmin}
              · administrador da instalação
            {/if}
          </p>
          {#if person.primaryEmail !== undefined}
            <p>{person.primaryEmail}</p>
          {/if}
          {#if person.state === 'banned'}
            <button
              type="button"
              disabled={busy}
              onclick={() =>
                void mutate(
                  'unban',
                  `/api/v1/admin/people/${person.userId}/unban`,
                  {},
                  'A conta voltou a ficar ativa. As sessões antigas não voltam.',
                )}
            >
              Reativar
            </button>
          {:else if person.state === 'active'}
            <label>
              Motivo do banimento
              <input bind:value={banReason} autocomplete="off" />
            </label>
            <button
              type="button"
              disabled={busy || banReason.trim().length === 0}
              onclick={() =>
                void mutate(
                  'ban',
                  `/api/v1/admin/people/${person.userId}/ban`,
                  { reason: banReason },
                  'A conta foi banida. Os convites de participação permanecem.',
                )}
            >
              Banir
            </button>
          {/if}
        </article>
      {/each}
    </section>

    <section aria-labelledby="projects-title">
      <h2 id="projects-title">Projetos</h2>
      <p class="hint">Só metadados. O texto dos itens fica no projeto.</p>
      {#if panel.projects.length === 0}
        <p>Nenhum projeto.</p>
      {/if}
      {#each panel.projects as project (project.id)}
        <article class="record">
          <h3>{project.projectKey} · {project.name}</h3>
          <p>
            {project.membershipCount} participantes · {project.issueCount} itens
            {#if project.archivedAt !== undefined}
              · arquivado
            {/if}
          </p>
        </article>
      {/each}
      {#if panel.projectsContinue}
        <p class="hint">A lista mostra os primeiros 100 projetos.</p>
      {/if}
    </section>

    <section aria-labelledby="mail-title">
      <h2 id="mail-title">Email</h2>
      <p>
        Um remetente por instalação, sem troca automática. Sabiá, Resend e
        MillionSend são integrações separadas. A aceitação na fila não é
        entrega.
      </p>
      <p><a href="/convites">Abrir convites</a></p>
      <form onsubmit={(event) => void saveMail(event)}>
        <label>
          Provedor
          <select bind:value={provider} disabled={busy}>
            <option value="sabia">Sabiá</option>
            <option value="resend">Resend</option>
            <option value="millionsend">MillionSend</option>
          </select>
        </label>
        <label>
          Chave da API
          <input
            bind:value={apiKey}
            type="password"
            autocomplete="off"
            required
            disabled={busy}
          />
        </label>
        {#if provider === 'sabia'}
          <label>
            Modelo publicado
            <input
              bind:value={template}
              autocomplete="off"
              required
              disabled={busy}
            />
          </label>
          <label>
            Origem do Sabiá
            <input
              bind:value={origin}
              autocomplete="off"
              required
              disabled={busy}
            />
          </label>
        {:else}
          <label>
            Endereço do remetente
            <input
              bind:value={fromAddress}
              type="email"
              autocomplete="off"
              required
              disabled={busy}
            />
          </label>
          <label>
            Nome do remetente
            <input
              bind:value={fromName}
              autocomplete="off"
              required
              disabled={busy}
            />
          </label>
        {/if}
        <button type="submit" disabled={busy}>Salvar remetente</button>
      </form>
    </section>

    <section aria-labelledby="audit-title">
      <h2 id="audit-title">Auditoria recente</h2>
      {#if panel.audit.length === 0}
        <p>Nenhum evento ainda.</p>
      {/if}
      {#each panel.audit as row (row.id)}
        <article class="record">
          <h3>{row.eventKind}</h3>
          <p>
            {row.result} · {row.occurredAt}
          </p>
          <p>
            alvo {row.targetKind}
            {row.targetId}
            {#if row.actorUserId !== undefined}
              · ator {row.actorUserId}
            {/if}
          </p>
          {#if row.reason !== undefined}
            <p>Motivo: {row.reason}</p>
          {/if}
        </article>
      {/each}
    </section>
  {/if}
</main>
