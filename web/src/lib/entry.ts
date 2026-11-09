export type InstallationStatus = {
  setup_required: boolean;
  login?: 'puxiri' | 'muira';
};

export type MuiraSetupInput = {
  setup_token: string;
  organization_name: string;
  locale: 'pt-BR' | 'en';
  timezone: string;
  display_name: string;
  email: string;
  issuer: string;
  client_id: string;
  client_secret: string;
};

export type PuxiriSetupInput = {
  setup_token: string;
  organization_name: string;
  locale: 'pt-BR' | 'en';
  timezone: string;
  display_name: string;
  email: string;
  password: string;
};

export type CreatedProject = {
  id: string;
  project_key: string;
  name: string;
  description?: string;
  default_view: 'board' | 'list';
};

export class EntryRequestError extends Error {
  readonly status: number;

  constructor(status: number) {
    super('Entry request failed');
    this.name = 'EntryRequestError';
    this.status = status;
  }
}

export class EntryClosed extends Error {
  constructor() {
    super('Browser entry is not mounted');
    this.name = 'EntryClosed';
  }
}

function invalid(): never {
  throw new Error('Invalid entry API response');
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function fields(
  value: unknown,
  allowed: readonly string[],
): Record<string, unknown> {
  if (!record(value)) {
    return invalid();
  }
  const result: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(value)) {
    if (!allowed.includes(key)) {
      return invalid();
    }
    result[key] = item;
  }
  return result;
}

function uuid(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(
      value,
    )
  ) {
    return invalid();
  }
  return value;
}

export function parseInstallationStatus(value: unknown): InstallationStatus {
  const body = fields(value, ['setup_required', 'login']);
  if (typeof body.setup_required !== 'boolean') {
    return invalid();
  }
  if (body.login === undefined) {
    return { setup_required: body.setup_required };
  }
  if (body.login !== 'puxiri' && body.login !== 'muira') {
    return invalid();
  }
  return { setup_required: body.setup_required, login: body.login };
}

export function parseAuthorizationUrl(value: unknown): string {
  const body = fields(value, ['authorization_url']);
  if (
    typeof body.authorization_url !== 'string' ||
    !body.authorization_url.startsWith('https://')
  ) {
    return invalid();
  }
  return body.authorization_url;
}

export function parseCreatedProject(value: unknown): CreatedProject {
  const body = fields(value, [
    'id',
    'project_key',
    'name',
    'description',
    'default_view',
  ]);
  if (typeof body.project_key !== 'string' || typeof body.name !== 'string') {
    return invalid();
  }
  if (body.default_view !== 'board' && body.default_view !== 'list') {
    return invalid();
  }
  const project: CreatedProject = {
    id: uuid(body.id),
    project_key: body.project_key,
    name: body.name,
    default_view: body.default_view,
  };
  if (body.description !== undefined) {
    if (typeof body.description !== 'string') {
      return invalid();
    }
    project.description = body.description;
  }
  return project;
}

export function entryFailureMessage(search: string): string | undefined {
  const code = new URLSearchParams(search).get('erro');
  if (code === 'identidade') {
    return 'O Muira não confirmou essa identidade. Comece o acesso de novo.';
  }
  if (code === 'setup') {
    return 'A instalação não pôde ser concluída. Gere outro token de setup e tente de novo.';
  }
  if (code === 'login') {
    return 'Não foi possível entrar agora. Tente de novo em instantes.';
  }
  if (code === 'indisponivel') {
    return 'A instalação está temporariamente indisponível.';
  }
  return undefined;
}

async function send(
  path: string,
  method: 'GET' | 'POST',
  signal: AbortSignal,
  body?: unknown,
  csrf?: string,
): Promise<Response> {
  const headers: Record<string, string> = { Accept: 'application/json' };
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
  }
  if (csrf !== undefined) {
    headers['x-csrf-token'] = csrf;
  }
  try {
    return await fetch(path, {
      method,
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
    });
  } catch {
    signal.throwIfAborted();
    throw new Error('Entry request could not be completed');
  }
}

export async function getInstallationStatus(
  signal: AbortSignal,
): Promise<InstallationStatus> {
  const response = await send('/api/v1/setup/status', 'GET', signal);
  if (response.status === 404) {
    throw new EntryClosed();
  }
  if (response.status !== 200) {
    throw new EntryRequestError(response.status);
  }
  return parseInstallationStatus(await response.json());
}

export async function startMuiraSetup(
  input: MuiraSetupInput,
  signal: AbortSignal,
): Promise<string> {
  const response = await send('/api/v1/setup/muira', 'POST', signal, input);
  if (response.status !== 200) {
    throw new EntryRequestError(response.status);
  }
  return parseAuthorizationUrl(await response.json());
}

export async function completePuxiriSetup(
  input: PuxiriSetupInput,
  signal: AbortSignal,
): Promise<void> {
  const response = await send('/api/v1/setup/puxiri', 'POST', signal, input);
  if (response.status !== 200) {
    throw new EntryRequestError(response.status);
  }
}

export async function startMuiraLogin(signal: AbortSignal): Promise<string> {
  const response = await send('/api/v1/auth/muira/login', 'POST', signal, {});
  if (response.status !== 200) {
    throw new EntryRequestError(response.status);
  }
  return parseAuthorizationUrl(await response.json());
}

export async function loginPuxiri(
  email: string,
  password: string,
  signal: AbortSignal,
): Promise<void> {
  const response = await send('/api/v1/auth/puxiri/login', 'POST', signal, {
    email,
    password,
  });
  if (response.status !== 200) {
    throw new EntryRequestError(response.status);
  }
}

export async function createProject(
  input: { project_key: string; name: string; description?: string },
  csrf: string,
  signal: AbortSignal,
): Promise<CreatedProject> {
  const response = await send('/api/v1/projects', 'POST', signal, input, csrf);
  if (response.status !== 201) {
    throw new EntryRequestError(response.status);
  }
  return parseCreatedProject(await response.json());
}
