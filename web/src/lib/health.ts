export type Readiness = 'ready' | 'not_ready';

export function parseReadiness(value: unknown): Readiness {
  if (
    typeof value !== 'object' ||
    value === null ||
    !('status' in value) ||
    (value.status !== 'ready' && value.status !== 'not_ready')
  ) {
    throw new Error('Invalid readiness response');
  }
  return value.status;
}

export async function getReadiness(signal: AbortSignal): Promise<Readiness> {
  const response = await fetch('/health/ready', {
    cache: 'no-store',
    signal,
    credentials: 'same-origin',
  });
  return parseReadinessResponse(response);
}

export async function parseReadinessResponse(
  response: Response,
): Promise<Readiness> {
  if (response.status !== 200 && response.status !== 503) {
    throw new Error('Readiness request failed');
  }
  const status = parseReadiness(await response.json());
  if ((response.status === 200) !== (status === 'ready')) {
    throw new Error('Inconsistent readiness response');
  }
  return status;
}
