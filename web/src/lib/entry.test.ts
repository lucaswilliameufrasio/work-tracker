import { describe, expect, it } from 'vitest';
import {
  entryFailureMessage,
  parseAuthorizationUrl,
  parseCreatedProject,
  parseInstallationStatus,
} from './entry';

describe('installation status', () => {
  it('accepts setup and an omitted login source', () => {
    expect(parseInstallationStatus({ setup_required: true })).toEqual({
      setup_required: true,
    });
  });

  it('accepts the active login source', () => {
    expect(
      parseInstallationStatus({ setup_required: false, login: 'muira' }),
    ).toEqual({ setup_required: false, login: 'muira' });
  });

  it('rejects an unknown field or login source', () => {
    expect(() =>
      parseInstallationStatus({ setup_required: true, extra: true }),
    ).toThrow('Invalid entry API response');
    expect(() =>
      parseInstallationStatus({ setup_required: false, login: 'google' }),
    ).toThrow('Invalid entry API response');
  });
});

describe('Muira authorization', () => {
  it('accepts only an https URL', () => {
    expect(
      parseAuthorizationUrl({
        authorization_url: 'https://muira.example.test/oauth/authorize',
      }),
    ).toBe('https://muira.example.test/oauth/authorize');
    expect(() =>
      parseAuthorizationUrl({ authorization_url: 'http://muira.example.test' }),
    ).toThrow('Invalid entry API response');
  });
});

describe('created project', () => {
  it('omits an absent description', () => {
    expect(
      parseCreatedProject({
        id: '019a0000-0000-7000-8000-000000000001',
        project_key: 'ENT',
        name: 'Entrada',
        default_view: 'board',
      }).description,
    ).toBeUndefined();
  });
});

describe('callback failure copy', () => {
  it('maps the known redirect codes', () => {
    expect(entryFailureMessage('?erro=identidade')).toContain('Muira');
    expect(entryFailureMessage('?erro=outro')).toBeUndefined();
  });
});
