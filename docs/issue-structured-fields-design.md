# Structured issue field updates

The first vertical slice edits only `priority`, one structured issue attribute already present in the schema and summary/detail reads. It does not add batch editing, assignment, due-date editing, labels, custom fields, search, or an outbox consumer. Assignment editing must wait for an authorized participant-picker journey; due-date editing can follow as a separate field command.

The follow-on due-date contract, including organization-local overdue calculation, is recorded in [`issue-due-date-design.md`](issue-due-date-design.md) and [ADR 0004](adr/0004-organization-timezone-conversion.md); it does not change the scope or evidence of this priority slice.

## Command contract

`PUT /api/v1/projects/{project_id}/issues/{issue_number}/priority` accepts exactly `expected_version` and `priority`. The version is a positive canonical decimal string; priority is one of `none`, `low`, `medium`, `high`, or `urgent`. It requires the browser session, configured origin, CSRF token and canonical `Idempotency-Key`. There are no query parameters, explicit `null`, unknown fields, or multi-field patches.

The writer revalidates current session and `MutateIssues` authority inside `BEGIN IMMEDIATE`, and rejects archived/read-only projects and trashed issues. It checks an existing receipt before the expected version so an exact retry returns the original stable result after a lost response. Reusing a key with another payload conflicts. A real change increments the issue version once; an effective no-op records a receipt but changes neither version nor history/outbox. Issue, history, outbox envelope, and immutable receipt commit atomically. A stale new command returns 409 and must be based on a fresh issue read.

## History and outbox

Structured changes use one `issue.field_updated` event per changed field. History stores a bounded field key plus JSON scalar `before`/`after` values, allowing typed structured diffs without retaining free-text snapshots. Text edits remain content-free. The outbox remains a version-1 immutable envelope containing only event identity, actor, issue version and time; it carries no field value and does not imply a worker or delivery guarantee.

The migration preserves prior event/outbox rows and rebuilds their existing constraints/indexes/triggers while permitting the new event kind. The current slice allows `priority` changes only; later structured fields must extend the validated field-value contract deliberately.

## UI behavior

The issue detail page shows a priority select and explicit save only after current session, project capability, and issue reads succeed. The advisory capability never authorizes the write; the mutation rechecks independently. A version conflict refreshes the issue without silently applying the user's selection. A lost/uncertain response retains the exact command and idempotency key for explicit replay. Identity/session loss clears the command; the page's existing synchronous concealment and generation/abort guards continue to apply.

This note records the scope of the first field slice, not completion of the structured-fields, Kanban, or full development-plan requirements.
