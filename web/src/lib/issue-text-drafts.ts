import { fields, uuid } from './issue-transport.ts';

const PREFIX = 'puxiri:issue-text-draft:v1:';
const MAX_RECORD_BYTES = 512 * 1024;

export type IssueTextDraftIdentity = {
  actor_id: string;
  session_id: string;
};

export type IssueTextDraftTarget = {
  project_id: string;
  issue_id: string;
  issue_number: string;
};

export type PendingIssueTextCommand = {
  idempotency_key: string;
  title: string;
  description: string;
  expected_version: string;
};

export type PersistedIssueTextDraft = IssueTextDraftIdentity &
  IssueTextDraftTarget & {
    schema_version: 1;
    base_version: string;
    title: string;
    description: string;
    pending_command?: PendingIssueTextCommand;
  };

export interface IssueTextDraftRepository {
  save(value: PersistedIssueTextDraft): void;
  load(
    identity: IssueTextDraftIdentity,
    target: IssueTextDraftTarget,
  ): PersistedIssueTextDraft | undefined;
  remove(identity: IssueTextDraftIdentity, target: IssueTextDraftTarget): void;
  clearProject(projectId: string): void;
  clearAll(): void;
}

export class IssueTextDraftStorageError extends Error {
  constructor() {
    super('Issue text draft storage is unavailable');
    this.name = 'IssueTextDraftStorageError';
  }
}

function decimal(value: unknown): string {
  if (
    typeof value !== 'string' ||
    !/^[1-9][0-9]{0,18}$/.test(value) ||
    BigInt(value) > 9223372036854775807n
  ) {
    throw new Error('Invalid issue text draft');
  }
  return value;
}

function draftKey(
  identity: IssueTextDraftIdentity,
  target: IssueTextDraftTarget,
): string {
  return `${PREFIX}${uuid(identity.actor_id)}:${uuid(identity.session_id)}:${uuid(target.project_id)}:${uuid(target.issue_id)}:${decimal(target.issue_number)}`;
}

function parseRecord(value: unknown): PersistedIssueTextDraft {
  const item = fields(value, [
    'schema_version',
    'actor_id',
    'session_id',
    'project_id',
    'issue_id',
    'issue_number',
    'base_version',
    'title',
    'description',
    'pending_command',
  ]);
  if (
    item.schema_version !== 1 ||
    typeof item.title !== 'string' ||
    typeof item.description !== 'string'
  ) {
    throw new Error('Invalid issue text draft');
  }
  const result: PersistedIssueTextDraft = {
    schema_version: 1,
    actor_id: uuid(item.actor_id),
    session_id: uuid(item.session_id),
    project_id: uuid(item.project_id),
    issue_id: uuid(item.issue_id),
    issue_number: decimal(item.issue_number),
    base_version: decimal(item.base_version),
    title: item.title,
    description: item.description,
  };
  if (item.pending_command !== undefined) {
    const command = fields(item.pending_command, [
      'idempotency_key',
      'title',
      'description',
      'expected_version',
    ]);
    if (
      typeof command.title !== 'string' ||
      typeof command.description !== 'string'
    ) {
      throw new Error('Invalid issue text draft');
    }
    result.pending_command = {
      idempotency_key: uuid(command.idempotency_key),
      title: command.title,
      description: command.description,
      expected_version: decimal(command.expected_version),
    };
    if (
      result.pending_command.expected_version !== result.base_version ||
      result.pending_command.title !== result.title ||
      result.pending_command.description !== result.description
    ) {
      throw new Error('Invalid issue text draft');
    }
  }
  return result;
}

function storage(): Storage {
  try {
    const result = globalThis.sessionStorage;
    if (result === undefined || typeof result.getItem !== 'function') {
      throw new Error('Session storage is unavailable');
    }
    return result;
  } catch {
    throw new IssueTextDraftStorageError();
  }
}

function storageKeys(source: Storage): string[] {
  const keys: string[] = [];
  for (let index = 0; index < source.length; index += 1) {
    const key = source.key(index);
    if (key?.startsWith(PREFIX)) {
      keys.push(key);
    }
  }
  return keys;
}

function parts(key: string): string[] | undefined {
  if (!key.startsWith(PREFIX)) {
    return undefined;
  }
  const values = key.slice(PREFIX.length).split(':');
  if (values.length !== 5) {
    return undefined;
  }
  try {
    for (const value of values.slice(0, 4)) {
      uuid(value);
    }
    decimal(values[4]);
    return values;
  } catch {
    return undefined;
  }
}

export class IssueTextDraftStore implements IssueTextDraftRepository {
  constructor(private readonly getStorage: () => Storage = storage) {}

  save(value: PersistedIssueTextDraft): void {
    let parsed: PersistedIssueTextDraft;
    let encoded: string;
    try {
      parsed = parseRecord(value);
      encoded = JSON.stringify(parsed);
      if (new TextEncoder().encode(encoded).byteLength > MAX_RECORD_BYTES) {
        throw new IssueTextDraftStorageError();
      }
      this.getStorage().setItem(draftKey(parsed, parsed), encoded);
    } catch (error) {
      if (error instanceof IssueTextDraftStorageError) {
        throw error;
      }
      throw new IssueTextDraftStorageError();
    }
  }

  load(
    identity: IssueTextDraftIdentity,
    target: IssueTextDraftTarget,
  ): PersistedIssueTextDraft | undefined {
    this.retainIdentity(identity);
    let source: Storage;
    try {
      source = this.getStorage();
    } catch {
      throw new IssueTextDraftStorageError();
    }
    const key = draftKey(identity, target);
    let raw: string | null;
    try {
      raw = source.getItem(key);
    } catch {
      throw new IssueTextDraftStorageError();
    }
    if (raw === null) {
      this.clearReplacedIssue(identity, target);
      return undefined;
    }
    let result: PersistedIssueTextDraft;
    try {
      if (new TextEncoder().encode(raw).byteLength > MAX_RECORD_BYTES) {
        throw new Error('Invalid issue text draft');
      }
      const parsed: unknown = JSON.parse(raw);
      result = parseRecord(parsed);
      if (draftKey(result, result) !== key) {
        throw new Error('Invalid issue text draft');
      }
    } catch {
      try {
        source.removeItem(key);
      } catch {
        throw new IssueTextDraftStorageError();
      }
      this.clearReplacedIssue(identity, target);
      return undefined;
    }
    this.clearReplacedIssue(identity, target);
    return result;
  }

  remove(identity: IssueTextDraftIdentity, target: IssueTextDraftTarget): void {
    try {
      this.getStorage().removeItem(draftKey(identity, target));
    } catch {
      throw new IssueTextDraftStorageError();
    }
  }

  clearProject(projectId: string): void {
    this.removeMatching((values) => values[2] === uuid(projectId));
  }

  clearAll(): void {
    this.removeMatching(() => true);
  }

  private retainIdentity(identity: IssueTextDraftIdentity): void {
    const actor = uuid(identity.actor_id);
    const session = uuid(identity.session_id);
    this.removeMatching(
      (values) => values[0] !== actor || values[1] !== session,
    );
  }

  private clearReplacedIssue(
    identity: IssueTextDraftIdentity,
    target: IssueTextDraftTarget,
  ): void {
    const actor = uuid(identity.actor_id);
    const session = uuid(identity.session_id);
    const project = uuid(target.project_id);
    const issue = uuid(target.issue_id);
    const number = decimal(target.issue_number);
    this.removeMatching(
      (values) =>
        values[0] === actor &&
        values[1] === session &&
        values[2] === project &&
        values[4] === number &&
        values[3] !== issue,
    );
  }

  private removeMatching(matches: (values: string[]) => boolean): void {
    let source: Storage;
    try {
      source = this.getStorage();
      const keys = storageKeys(source);
      for (const key of keys) {
        const values = parts(key);
        if (values === undefined || matches(values)) {
          source.removeItem(key);
        }
      }
    } catch {
      throw new IssueTextDraftStorageError();
    }
  }
}

export const issueTextDraftStore = new IssueTextDraftStore();
