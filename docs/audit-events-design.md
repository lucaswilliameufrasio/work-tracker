# Authentication audit events — feature design

**Status:** Minimal transactional audit records are implemented for email address changes and confirmation, password reset/change, and session revocation. HTTP administration, audit browsing/retention, and failed-attempt recording are not implemented.

**Requirements:** `docs/development-plan.md`, sections 6 and 13.

`audit_events` stores an allowlisted event kind, success/failure result, optional actor user ID, opaque target kind/ID, and UTC timestamp. The schema rejects unknown kinds, outcomes, and target kinds; update and delete triggers make records immutable. It has no details/payload column, so callers cannot attach passwords, tokens, email addresses, issue content, or provider responses.

Mutation hooks append the success event to the same SQLite transaction as the protected write. This applies to email address add/confirm/promote/remove, password reset/change, and session revoke/revoke-all. If writing the audit event fails, the mutation and its related revocations roll back. Password changes require the caller to supply an explicit optional actor separately from the target user.

Current mutation hooks record successful committed operations only. An operation rejected or rolled back is not represented as a failed audit event; a future HTTP/security-attempt audit path must define its own transaction and privacy semantics rather than weakening atomicity of these records. No audit query/UI, actor authorization policy, retention/cleanup, or export exists yet.

Integration tests cover schema constraints, immutability, caller-owned rollback, successful password-change audit, and rollback of password/session changes when audit persistence fails.
