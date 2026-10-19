export type CurrentSession = {
  user_id: string;
  session_id: string;
  csrf_token: string;
};

export type SessionSummary = {
  id: string;
  created_at: string;
  last_activity_at: string;
  expires_at: string;
  is_current: boolean;
};

export type SessionPage = {
  data: SessionSummary[];
  has_more: boolean;
  next_cursor?: string;
};

export class SessionRequestError extends Error {
  readonly status: number;
  readonly code: string;
  readonly request_id: string;
  readonly retryable: boolean;

  constructor(
    status: number,
    code: string,
    requestId: string,
    retryable: boolean,
  ) {
    super('Session request failed');
    this.name = 'SessionRequestError';
    this.status = status;
    this.code = code;
    this.request_id = requestId;
    this.retryable = retryable;
  }
}

function invalid(): never {
  throw new Error('Invalid session API response');
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function fields(value: unknown, allowed: string[]): Record<string, unknown> {
  if (!record(value)) {
    return invalid();
  }
  for (const name of Object.keys(value)) {
    if (!allowed.includes(name)) {
      return invalid();
    }
  }
  return value;
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

function timestamp(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{9}Z$/.test(value) ||
    !Number.isFinite(Date.parse(value))
  ) {
    return invalid();
  }
  if (new Date(value).toISOString().slice(0, 19) !== value.slice(0, 19)) {
    return invalid();
  }
  return value;
}

export function parseCurrentSession(value: unknown): CurrentSession {
  const item = fields(value, ['user_id', 'session_id', 'csrf_token']);
  if (
    typeof item.csrf_token !== 'string' ||
    !/^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/.test(item.csrf_token)
  ) {
    return invalid();
  }
  return {
    user_id: uuid(item.user_id),
    session_id: uuid(item.session_id),
    csrf_token: item.csrf_token,
  };
}

export function parseSessionPage(
  value: unknown,
  pageSize: number,
  after?: string,
): SessionPage {
  if (!Number.isInteger(pageSize) || pageSize < 1 || pageSize > 100) {
    return invalid();
  }
  if (after !== undefined) {
    uuid(after);
  }
  const page = fields(value, ['data', 'has_more', 'next_cursor']);
  if (
    !Array.isArray(page.data) ||
    page.data.length > pageSize ||
    typeof page.has_more !== 'boolean'
  ) {
    return invalid();
  }
  const data: SessionSummary[] = [];
  let previous = after;
  let currentCount = 0;
  for (const raw of page.data) {
    const item = fields(raw, [
      'id',
      'created_at',
      'last_activity_at',
      'expires_at',
      'is_current',
    ]);
    const id = uuid(item.id);
    if (
      typeof item.is_current !== 'boolean' ||
      (previous !== undefined && id >= previous)
    ) {
      return invalid();
    }
    const created = timestamp(item.created_at);
    const activity = timestamp(item.last_activity_at);
    const expiry = timestamp(item.expires_at);
    if (created > activity || activity >= expiry) {
      return invalid();
    }
    if (item.is_current) {
      currentCount += 1;
    }
    if (currentCount > 1) {
      return invalid();
    }
    data.push({
      id,
      created_at: created,
      last_activity_at: activity,
      expires_at: expiry,
      is_current: item.is_current,
    });
    previous = id;
  }
  if (page.has_more) {
    const cursor = uuid(page.next_cursor);
    if (
      (after !== undefined && cursor >= after) ||
      (previous !== undefined && cursor > previous)
    ) {
      return invalid();
    }
    return { data, has_more: true, next_cursor: cursor };
  }
  if ('next_cursor' in page) {
    return invalid();
  }
  return { data, has_more: false };
}

async function json(response: Response, signal: AbortSignal): Promise<unknown> {
  if (
    response.headers
      .get('content-type')
      ?.split(';')[0]
      ?.trim()
      .toLowerCase() !== 'application/json' ||
    response.body === null
  ) {
    try {
      await response.body?.cancel();
    } catch {
      signal.throwIfAborted();
      return invalid();
    }
    return invalid();
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let size = 0;
  let text = '';
  try {
    while (true) {
      signal.throwIfAborted();
      const chunk = await reader.read();
      signal.throwIfAborted();
      if (chunk.done) {
        break;
      }
      size += chunk.value.byteLength;
      if (size > 64 * 1024) {
        await reader.cancel();
        return invalid();
      }
      text += decoder.decode(chunk.value, { stream: true });
    }
    text += decoder.decode();
    const value: unknown = JSON.parse(text);
    return value;
  } catch {
    try {
      await reader.cancel();
    } catch {
      signal.throwIfAborted();
      return invalid();
    }
    signal.throwIfAborted();
    return invalid();
  } finally {
    reader.releaseLock();
  }
}

async function failure(
  response: Response,
  signal: AbortSignal,
): Promise<never> {
  const error = fields(await json(response, signal), [
    'code',
    'message',
    'request_id',
    'field_errors',
    'retryable',
  ]);
  if (
    response.status < 400 ||
    typeof error.code !== 'string' ||
    !/^[a-z][a-z0-9_]{0,63}$/.test(error.code) ||
    typeof error.message !== 'string' ||
    typeof error.retryable !== 'boolean' ||
    !record(error.field_errors)
  ) {
    return invalid();
  }
  for (const message of Object.values(error.field_errors)) {
    if (typeof message !== 'string') {
      return invalid();
    }
  }
  throw new SessionRequestError(
    response.status,
    error.code,
    uuid(error.request_id),
    error.retryable,
  );
}

async function send(
  path: string,
  signal: AbortSignal,
  csrf?: string,
): Promise<Response> {
  signal.throwIfAborted();
  try {
    return await fetch(path, {
      method: csrf === undefined ? 'GET' : 'DELETE',
      mode: 'same-origin',
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
      signal,
      headers:
        csrf === undefined
          ? { Accept: 'application/json' }
          : { Accept: 'application/json', 'x-csrf-token': csrf },
    });
  } catch {
    signal.throwIfAborted();
    throw new Error('Session request could not be completed');
  }
}

export async function getCurrentSession(
  signal: AbortSignal,
): Promise<CurrentSession> {
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  const response = await send('/api/v1/auth/session', deadline);
  if (response.status !== 200) {
    return failure(response, deadline);
  }
  return parseCurrentSession(await json(response, deadline));
}

export async function getSessionPage(
  options: { cursor?: string; page_size?: number },
  signal: AbortSignal,
): Promise<SessionPage> {
  const size = options.page_size ?? 50;
  if (!Number.isInteger(size) || size < 1 || size > 100) {
    throw new Error('Invalid session page size');
  }
  if (options.cursor !== undefined) {
    uuid(options.cursor);
  }
  const query = new URLSearchParams({ page_size: String(size) });
  if (options.cursor !== undefined) {
    query.set('cursor', options.cursor);
  }
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  const response = await send(`/api/v1/auth/sessions?${query}`, deadline);
  if (response.status !== 200) {
    return failure(response, deadline);
  }
  return parseSessionPage(await json(response, deadline), size, options.cursor);
}

async function terminate(path: string, signal: AbortSignal): Promise<void> {
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  const current = await getCurrentSession(deadline);
  const response = await send(path, deadline, current.csrf_token);
  if (response.status !== 204) {
    return failure(response, deadline);
  }
}

export async function revokeSession(
  sessionId: string,
  signal: AbortSignal,
): Promise<void> {
  uuid(sessionId);
  await terminate(`/api/v1/auth/sessions/${sessionId}`, signal);
}

export async function revokeAllSessions(signal: AbortSignal): Promise<void> {
  await terminate('/api/v1/auth/sessions', signal);
}

export async function logoutCurrentSession(signal: AbortSignal): Promise<void> {
  await terminate('/api/v1/auth/session', signal);
}
