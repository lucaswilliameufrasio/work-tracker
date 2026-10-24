# Issue due-date editing

**Status:** Implemented and locally verified on 2026-10-24. The overall development plan remains open.

**Requirements:** `docs/development-plan.md`, sections 4, 5.1, 5.3 and 5.6. Organization-local date conversion is defined in [ADR 0004](adr/0004-organization-timezone-conversion.md).

## Domain contract

- A due date is a civil calendar date stored and transported as canonical `YYYY-MM-DD`; it is never an instant and is never shifted between UTC, organization timezone, or browser timezone.
- Past dates are valid. The due-date command accepts a date or explicit `null` to clear it; an omitted field is invalid.
- `is_overdue` is a server-derived, non-persisted boolean: `due_date < today_in_organization_timezone` and status category is not `done` or `canceled`. A due date equal to the organization's current date is not overdue. Reopening an issue causes the projection to be recalculated from its current status.
- Resolve the IANA zone from the organization row and the local date from the request's UTC instant. Never use the host/browser timezone or silently fall back to UTC. An invalid persisted zone fails closed for due-date mutations and issue reads that project overdue state.

## Mutation contract

`PUT /api/v1/projects/{project_id}/issues/{issue_number}/due-date` accepts exactly `expected_version` and required `due_date` (`YYYY-MM-DD` or `null`). It uses the existing browser-session, exact Origin, CSRF, canonical `Idempotency-Key`, JSON admission, and `MutateIssues` rules. Authorization, session, project state, issue trash state, timezone validity, receipt lookup, expected version, issue write, typed history diff, outbox envelope, and immutable receipt are checked/written in one `BEGIN IMMEDIATE` transaction.

A changed date increments version once and writes a `due_date` history scalar (`null` or a date string), without putting the date into the outbox. A no-op receives a stable receipt but does not change version/history/outbox. Exact retries replay the original receipt after current authorization revalidation; key reuse with another payload conflicts. Stale new commands return 409. No optimistic UI update or automatic retry is allowed.

## UI and read behavior

The existing issue detail presents a labeled native date input and explicit save/clear action only when current project capability and issue state permit field updates. Conflict refreshes server data but preserves the selected date for review. An uncertain result preserves the exact command and key for explicit replay; identity/access loss removes them. Successful writes trigger a fresh read before displaying the derived overdue state. List, detail, and board reads expose `is_overdue`; the UI announces overdue textually and does not rely on color alone. Dates are rendered as civil dates without browser-zone conversion.

## Migration and boundaries

Migration 0036 extends validated structured history to date-or-null values and adds an immutable due-date receipt table while preserving existing events, outbox envelopes, and priority receipts. The existing outbox envelope remains content-free. Slice255 adds exact-date matching in project issue search; it does not add date ranges, overdue/missing-date filters, an outbox consumer, overdue automation, assignment editing, timezone settings UI, or recurrence scheduling.

## Verification plan

Use real migrated file-backed SQLite for migration preservation, authorization, retries/no-op/conflict, rollback, history/outbox, and malformed persisted data. Exercise exact API payload/headers/errors and the real TypeScript client against the router. Unit-test IANA local-day boundaries (including a daylight-saving transition), invalid zones, leap/invalid dates, and `done`/`canceled` exclusions. Exercise edit, clear, conflict, explicit replay, and desktop/mobile rendering in Chromium. Run `mise exec -- make check`; this is local evidence only, not provider-live or deployment validation.

## Verification record

The full local gate passed after the final code and test changes with `TMPDIR=/home/lucaseufrasio/projects/work-tracker/.test-data/quality-due-date-20261024-final3 DOCKER_CONTEXT=rootless PUXIRI_E2E_PORT=18627 mise exec -- make check` (log: `.test-data/quality-due-date-20261024-final3/make-check.log`). This includes web build/format/lint/Svelte check (0 errors and warnings), 136 Vitest tests, Rust format/Clippy with warnings denied and all locked test targets, Rust build, 12 Playwright tests (due-date desktop and mobile-dark included), and Floci S3 38/38 with no skips. Due-date regressions cover transactional mutation/replay/no-op/clear/rollback, access and eligibility revalidation, migration preservation, invalid timezone fail-closed behavior, organization-local midnight/DST boundaries, terminal `done`/`canceled` exclusions, strict API/OpenAPI contracts, and browser conflict preservation plus explicit same-key replay. All evidence is local/synthetic; it does not certify provider-live behavior, deployment, or completion of the overall product plan.
