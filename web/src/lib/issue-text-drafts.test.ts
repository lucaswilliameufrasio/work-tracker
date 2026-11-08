import { expect, it } from 'vitest';
import {
  IssueTextDraftStorageError,
  IssueTextDraftStore,
  type PersistedIssueTextDraft,
} from './issue-text-drafts';

const actor = '019a0000-0000-7000-8000-000000000001';
const session = '019a0000-0000-7000-8000-000000000002';
const project = '019a0000-0000-7000-8000-000000000003';
const issue = '019a0000-0000-7000-8000-000000000004';
const number = '9007199254740993';
const key = '019a0000-0000-7000-8000-000000000005';

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();
  get length(): number {
    return this.values.size;
  }
  clear(): void {
    this.values.clear();
  }
  getItem(name: string): string | null {
    return this.values.get(name) ?? null;
  }
  key(index: number): string | null {
    const keys = [...this.values.keys()];
    return keys[index] ?? null;
  }
  removeItem(name: string): void {
    this.values.delete(name);
  }
  setItem(name: string, value: string): void {
    this.values.set(name, value);
  }
}

function draft(): PersistedIssueTextDraft {
  return {
    schema_version: 1,
    actor_id: actor,
    session_id: session,
    project_id: project,
    issue_id: issue,
    issue_number: number,
    base_version: '7',
    title: 'Private draft title',
    description: 'Private draft description',
  };
}

const target = {
  project_id: project,
  issue_id: issue,
  issue_number: number,
};

it('round-trips only the exact actor/session/project/immutable issue across store instances', () => {
  const backing = new MemoryStorage();
  const first = new IssueTextDraftStore(() => backing);
  first.save(draft());
  const reloadedStore = new IssueTextDraftStore(() => backing);
  expect(
    reloadedStore.load({ actor_id: actor, session_id: session }, target),
  ).toEqual(draft());
  expect(
    reloadedStore.load({ actor_id: actor, session_id: key }, target),
  ).toBeUndefined();
  expect(backing.length).toBe(0);
});

it('retains the exact uncertain command/key for an explicit same-key recovery after reload', () => {
  const backing = new MemoryStorage();
  const value = draft();
  value.title = 'Command title';
  value.description = 'Command description';
  value.pending_command = {
    idempotency_key: key,
    title: 'Command title',
    description: 'Command description',
    expected_version: '7',
  };
  const store = new IssueTextDraftStore(() => backing);
  store.save(value);
  expect(
    new IssueTextDraftStore(() => backing).load(
      { actor_id: actor, session_id: session },
      target,
    ),
  ).toEqual(value);
  backing.setItem(
    `puxiri:issue-text-draft:v1:${actor}:${session}:${project}:${issue}:${number}`,
    JSON.stringify({
      ...value,
      pending_command: { ...value.pending_command, title: 'Different payload' },
    }),
  );
  expect(
    new IssueTextDraftStore(() => backing).load(
      { actor_id: actor, session_id: session },
      target,
    ),
  ).toBeUndefined();
});

it('drops malformed, unknown-schema, mismatched-identity and replaced-issue drafts fail-closed', () => {
  const backing = new MemoryStorage();
  const keyForIssue = `puxiri:issue-text-draft:v1:${actor}:${session}:${project}:${issue}:${number}`;
  backing.setItem(keyForIssue, '{broken');
  const store = new IssueTextDraftStore(() => backing);
  expect(
    store.load({ actor_id: actor, session_id: session }, target),
  ).toBeUndefined();
  expect(backing.length).toBe(0);
  store.save(draft());
  backing.setItem(
    keyForIssue,
    JSON.stringify({ ...draft(), schema_version: 2 }),
  );
  expect(
    store.load({ actor_id: actor, session_id: session }, target),
  ).toBeUndefined();
  store.save(draft());
  expect(
    store.load(
      { actor_id: actor, session_id: session },
      {
        ...target,
        issue_id: key,
      },
    ),
  ).toBeUndefined();
  expect(backing.length).toBe(0);
});

it('clears one inaccessible project, all drafts on logout, and never another session identity', () => {
  const backing = new MemoryStorage();
  const store = new IssueTextDraftStore(() => backing);
  const secondProject = '019a0000-0000-7000-8000-000000000006';
  const secondIssue = '019a0000-0000-7000-8000-000000000007';
  store.save(draft());
  store.save({
    ...draft(),
    project_id: secondProject,
    issue_id: secondIssue,
    issue_number: '2',
  });
  expect(backing.length).toBe(2);
  store.clearProject(project);
  expect(backing.length).toBe(1);
  store.load(
    { actor_id: actor, session_id: key },
    { project_id: secondProject, issue_id: secondIssue, issue_number: '2' },
  );
  expect(backing.length).toBe(0);
  store.save(draft());
  store.clearAll();
  expect(backing.length).toBe(0);
});

it('rejects oversized data and maps unavailable browser storage to a safe error', () => {
  const backing = new MemoryStorage();
  const store = new IssueTextDraftStore(() => backing);
  expect(() =>
    store.save({ ...draft(), description: 'x'.repeat(512 * 1024) }),
  ).toThrow(IssueTextDraftStorageError);
  const unavailable = new IssueTextDraftStore(() => {
    throw new Error('private details');
  });
  expect(() => unavailable.save(draft())).toThrow(IssueTextDraftStorageError);
  expect(() =>
    unavailable.load({ actor_id: actor, session_id: session }, target),
  ).toThrow(IssueTextDraftStorageError);
});
