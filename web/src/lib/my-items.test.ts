import { describe, expect, it } from 'vitest';
import { parseAssignedIssuePage } from './my-items';

const issue = {
  id: '01890f3e-7b1c-7cc4-98a7-123456789abc',
  project_id: '01890f3e-7b1c-7cc4-98a7-123456789abd',
  issue_number: '4',
  issue_key: 'ITM-4',
  issue_type_id: '01890f3e-7b1c-7cc4-98a7-123456789abe',
  status_id: '01890f3e-7b1c-7cc4-98a7-123456789abf',
  title: 'Fechar o convite',
  assignee_user_id: '01890f3e-7b1c-7cc4-98a7-123456789ab0',
  creator_user_id: '01890f3e-7b1c-7cc4-98a7-123456789ab1',
  priority: 'none',
  is_overdue: false,
  version: '1',
  created_at: '2026-11-08T00:00:00.000000000Z',
  updated_at: '2026-11-08T00:00:00.000000000Z',
};

describe('assigned issue page', () => {
  it('accepts items from more than one project and omits the cursor', () => {
    const page = parseAssignedIssuePage(
      {
        data: [
          issue,
          {
            ...issue,
            id: '01890f3e-7b1c-7cc4-98a7-123456789ac2',
            project_id: '01890f3e-7b1c-7cc4-98a7-123456789ac3',
            issue_number: '1',
            issue_key: 'ARC-1',
          },
        ],
        has_more: false,
      },
      50,
    );
    expect(page.data).toHaveLength(2);
    expect(page.next_cursor).toBeUndefined();
  });

  it('rejects a description and a null cursor', () => {
    expect(() =>
      parseAssignedIssuePage(
        { data: [{ ...issue, description: 'segredo' }], has_more: false },
        50,
      ),
    ).toThrow('Invalid issue API response');
    expect(() =>
      parseAssignedIssuePage(
        { data: [], has_more: false, next_cursor: null },
        50,
      ),
    ).toThrow('Invalid issue API response');
  });
});
