import { describe, expect, it } from 'vitest';
import { parseReadiness, parseReadinessResponse } from './health';

describe('public readiness contract', () => {
  it.each(['ready', 'not_ready'] as const)('accepts %s', (status) => {
    expect(parseReadiness({ status })).toBe(status);
  });

  it.each([null, undefined, 'ready', {}, { status: 'live' }, { status: true }])(
    'rejects malformed or unrelated data: %j',
    (value) => {
      expect(() => parseReadiness(value)).toThrow('Invalid readiness response');
    },
  );
});

describe('readiness HTTP response validation', () => {
  it.each([
    [200, 'ready'],
    [503, 'not_ready'],
  ] as const)('accepts consistent HTTP %s / %s', async (code, status) => {
    expect(
      await parseReadinessResponse(Response.json({ status }, { status: code })),
    ).toBe(status);
  });

  it.each([
    [200, 'not_ready'],
    [503, 'ready'],
  ] as const)('rejects inconsistent HTTP %s / %s', async (code, status) => {
    await expect(
      parseReadinessResponse(Response.json({ status }, { status: code })),
    ).rejects.toThrow('Inconsistent readiness response');
  });

  it('rejects unexpected status without trusting its payload', async () => {
    await expect(
      parseReadinessResponse(
        Response.json({ status: 'ready' }, { status: 500 }),
      ),
    ).rejects.toThrow('Readiness request failed');
  });

  it('rejects a non-JSON body', async () => {
    await expect(
      parseReadinessResponse(new Response('<html>not an API</html>')),
    ).rejects.toThrow();
  });
});
