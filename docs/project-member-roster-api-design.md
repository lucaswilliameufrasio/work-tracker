# Project member roster API

**Status:** Implemented and verified in slice 250. The underlying authorization
policy and internal `ProjectMembershipStore` are established in
`docs/access-control-design.md`; this slice exposes them through session-bound
browser reads. Assignment writes remain a separate operation and revalidate
eligibility transactionally.

## Read contract

- `GET /api/v1/projects/{project_id}/members` returns a bounded page with
  `data`, `has_more`, and optional `next_cursor`. `page_size` defaults to 50
  and is limited to 1–100. The canonical UUID cursor is an exclusive
  `user_id` boundary in ascending order; it contains no display name or email.
- `GET /api/v1/projects/{project_id}/members/{user_id}` reads one participant
  for displaying a current assignment, including an assignment whose member is
  no longer eligible to receive new assignments.
- Each record contains `user_id`, `display_name`, `role`, and
  `can_be_assigned`; `primary_email` is omitted unless the current reader is a
  project manager or organization owner/admin. Pending email is never exposed.
- `can_be_assigned` is true only for an active project manager/member. It is a
  read projection, not authorization for a later mutation.
- Session authentication, current access policy, and roster selection share a
  single SQLite read transaction. Explicit project members may read the roster;
  organization owner/admin may read it for management without gaining issue
  access. Installation-admin status alone does not grant access. Archived
  project rosters remain readable. Missing/inaccessible projects and members
  use the generic not-found response.
- Pages use a UUID keyset, not a frozen snapshot. The frontend can request more
  participants explicitly; it must not treat a page as the complete roster.

## Boundaries

Roster reads do not write audit events or change membership. Assignment writes
remain limited to active project managers/members (including guests with an
explicit eligible project membership), use the current issue version and
idempotency receipt, and revalidate the selected user inside the write
transaction. A removed, inactive, or otherwise ineligible current assignee is
retained on the issue and presented as having no current project access; losing
access does not trigger a notification or change the assignment automatically.

This API does not implement project membership administration, a general
organization directory, notifications, or outbox delivery.
