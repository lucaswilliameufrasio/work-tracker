# Versioned issue assignment

**Status:** Implemented and verified in slice 250 after the session-bound
project-member roster slice. The roster contract is in
[`project-member-roster-api-design.md`](project-member-roster-api-design.md).

## Command contract

`PUT /api/v1/projects/{project_id}/issues/{issue_number}/assignee` accepts an
exact JSON object containing `expected_version` and `assignee_user_id`.
`expected_version` is a positive canonical decimal string. `assignee_user_id`
is either a canonical lowercase UUID or explicit `null` to clear; omission,
unknown fields, and noncanonical UUIDs are invalid. The request requires the
current browser session, configured Origin, matching CSRF token, and a
canonical `Idempotency-Key` UUID. There are no query parameters.

Only the existing `MutateIssues` capability may execute the command. A new
non-null assignee must, at the transaction's current state, be an active user
with an explicit project membership in `manager` or `member` role. Eligible
guests with an explicit member role are included; viewers, inactive users,
removed memberships, and organization/installation authority alone are not.
The browser roster's `can_be_assigned` flag is only advisory. The command
revalidates the selected target inside its write transaction.

The writer uses `BEGIN IMMEDIATE` and atomically updates the assignee/version,
appends a structured `issue.field_updated` event, content-free outbox envelope,
and immutable idempotency receipt. A no-op records a receipt but does not bump
version or append history/outbox. Exact replay returns the original receipt
after current actor authorization and issue checks; it never reapplies the
assignment. A changed command with a stale issue version returns 409 and the
draft must be reviewed, not automatically overwritten. Uncertain responses are
resolved only by replaying the same request with the same key.

History stores only the before/after user UUID or `null`, never a name or email
snapshot. Existing assignments are not automatically cleared when the member
becomes inactive or loses an eligible role. The UI can display that current
member through the single-member roster read and must offer an explicit clear
or a new eligible selection.

The operation does not send notifications, publish outbox events, or alter
membership. Creation-time assignment remains its own validation path.
