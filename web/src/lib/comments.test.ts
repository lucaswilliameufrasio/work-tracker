import { describe, expect, it } from 'vitest';
import { parseCommentPage } from './comments';

const comment = {
  id: '01890f3e-7b1c-7cc4-98a7-123456789abc',
  author_user_id: '01890f3e-7b1c-7cc4-98a7-123456789abd',
  author_display_name: 'Ada',
  body: 'Um comentário',
  version: '1',
  can_edit: true,
  can_delete: true,
  created_at: '2026-11-08T00:00:00.000000000Z',
  updated_at: '2026-11-08T00:00:00.000000000Z',
};

describe('comment page', () => {
  it('accepts comments and events and omits the cursor', () => {
    expect(
      parseCommentPage({
        comments: [comment],
        events: [
          {
            id: '01890f3e-7b1c-7cc4-98a7-123456789abe',
            comment_id: comment.id,
            actor_user_id: comment.author_user_id,
            event_kind: 'comment.created',
            occurred_at: comment.created_at,
          },
        ],
        has_more: false,
      }).next_cursor,
    ).toBeUndefined();
  });

  it('rejects an unknown field and a null cursor', () => {
    expect(() =>
      parseCommentPage({
        comments: [],
        events: [],
        has_more: false,
        extra: true,
      }),
    ).toThrow('Invalid comment API response');
    expect(() =>
      parseCommentPage({
        comments: [],
        events: [],
        has_more: false,
        next_cursor: null,
      }),
    ).toThrow('Invalid comment API response');
  });
});
