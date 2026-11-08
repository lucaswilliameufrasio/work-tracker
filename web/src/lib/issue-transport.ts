import { notifySessionEnded } from './session-lifecycle.ts';

export class IssueRequestError extends Error {
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
    super('Issue request failed');
    this.name = 'IssueRequestError';
    this.status = status;
    this.code = code;
    this.request_id = requestId;
    this.retryable = retryable;
  }
}

export function invalid(): never {
  throw new Error('Invalid issue API response');
}

export function fields(
  value: unknown,
  allowed: readonly string[],
): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
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

export function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function uuid(value: unknown): string {
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

export async function issueJson(
  response: Response,
  signal: AbortSignal,
  limit: number,
): Promise<unknown> {
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
  let bytes = 0;
  let text = '';
  try {
    while (true) {
      signal.throwIfAborted();
      const chunk = await reader.read();
      signal.throwIfAborted();
      if (chunk.done) {
        break;
      }
      bytes += chunk.value.byteLength;
      if (bytes > limit) {
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

// Internal transport for issue/catalog read clients, whose public methods build
// fixed relative paths and do not accept an arbitrary URL or HTTP method.
export async function readIssueJson(
  path: string,
  signal: AbortSignal,
): Promise<unknown> {
  const deadline = AbortSignal.any([signal, AbortSignal.timeout(8000)]);
  deadline.throwIfAborted();
  let response: Response;
  try {
    response = await fetch(path, {
      method: 'GET',
      mode: 'same-origin',
      credentials: 'same-origin',
      cache: 'no-store',
      redirect: 'error',
      signal: deadline,
      headers: { Accept: 'application/json' },
    });
  } catch {
    deadline.throwIfAborted();
    throw new Error('Issue request could not be completed');
  }
  if (response.status === 200) {
    return issueJson(response, deadline, 512 * 1024);
  }
  return issueFailure(response, deadline);
}

export async function issueFailure(
  response: Response,
  signal: AbortSignal,
): Promise<never> {
  if (response.status === 401) {
    notifySessionEnded();
  }
  const error = fields(await issueJson(response, signal, 64 * 1024), [
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
    typeof error.field_errors !== 'object' ||
    error.field_errors === null ||
    Array.isArray(error.field_errors)
  ) {
    return invalid();
  }
  for (const message of Object.values(error.field_errors)) {
    if (typeof message !== 'string') {
      return invalid();
    }
  }
  throw new IssueRequestError(
    response.status,
    error.code,
    uuid(error.request_id),
    error.retryable,
  );
}
