# ADR 0004: IANA timezone conversion

- **Status:** Accepted for organization-local date calculations
- **Date:** 2026-10-24
- **Context:** `docs/development-plan.md`, sections 4 and 5.1; due-date behavior in `docs/issue-due-date-design.md`

## Context

The organization stores an IANA timezone. Due dates are civil dates (`YYYY-MM-DD`), while overdue state depends on the current calendar day in that timezone. Using UTC, the host timezone, or a browser's timezone can classify an issue incorrectly near a local-day boundary. The product is a self-hosted single binary and must not require a separately installed Node process or timezone service.

## Decision

- Use the exact `chrono-tz` 0.10.4 crate, locked in `Cargo.lock`, to resolve organization timezone identifiers and convert UTC instants to organization-local dates.
- Treat a due date as a date-only value. Never convert it to midnight UTC or to a timestamp.
- Compute `today` from the request's UTC instant and the configured IANA zone. An issue is overdue only when `due_date < today` and its status category is neither `done` nor `canceled`.
- Reject an invalid persisted timezone for operations that need the local date. Never fall back to UTC, the host timezone, or a fixed offset.
- Keep the timezone database embedded in the application dependency graph. Updating `chrono-tz` and `Cargo.lock` is the supported way to adopt updated IANA rules.

## Alternatives considered

- Host-installed tzdata would allow independent system updates but make identical application binaries behave differently across hosts and require a runtime OS dependency.
- Fixed offsets do not model IANA daylight-saving or historical rule changes.
- Manual timezone parsing or rule tables would duplicate a maintained dataset and are not acceptable.

## Consequences and limits

- The timezone is resolved locally and offline; no request to a provider or system service is made.
- A release can carry a newer IANA database only after updating the locked crate and rebuilding. Operators must be able to identify the bundled application version when investigating a timezone-rule change.
- Organization timezone authoring is not part of this ADR. Future configuration writes must validate IANA identifiers before persistence; current due-date reads fail closed if persisted configuration is invalid.
- This decision covers conversion of a UTC instant to a local date. It does not define ambiguous/nonexistent local wall-clock times for recurring schedules; the separate scheduling policy in the development plan still applies.

## Evidence

- User selected the timezone-aware due-date contract on 2026-10-24.
- [`chrono-tz` 0.10.4 crate metadata](https://crates.io/api/v1/crates/chrono-tz/0.10.4) identifies it as an implementation of chrono time zones from the IANA database, MIT OR Apache-2.0, with Rust 1.65 MSRV.
