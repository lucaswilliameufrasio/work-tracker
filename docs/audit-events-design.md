# Audit events — feature design

**Status:** Minimal transactional audit records are implemented for email address changes and confirmation, password reset/change, session revocation, project creation, and project membership changes. HTTP administration, audit browsing/retention, and failed-attempt recording are not implemented.

**Requirements:** `docs/development-plan.md`, sections 5, 6, and 13.

`audit_events` stores an allowlisted event kind, success/failure result, optional actor user ID, opaque target kind/ID, and UTC timestamp. Project-membership events additionally store the affected `subject_user_id` and typed before/after project roles; all other events require those fields to remain empty. The schema rejects unknown kinds, outcomes, targets, and invalid role transitions; update and delete triggers make records immutable. There is no arbitrary details/payload column, so callers cannot attach passwords, tokens, email addresses, issue content, or provider responses.

Mutation hooks append the success event to the same SQLite transaction as the protected write. This applies to email address add/confirm/promote/remove, password reset/change, session revoke/revoke-all, project creation with its initial manager membership, and project-member add/role-change/remove/self-leave. Membership events identify both the project and affected user and retain only the project role transition; self-grants and self-exits are distinguishable because actor and subject match. If writing the audit event fails, the mutation and its related revocations roll back. Password changes require the caller to supply an explicit optional actor separately from the target user.

Current mutation hooks record successful committed operations only. An operation rejected or rolled back is not represented as a failed audit event; a future HTTP/security-attempt audit path must define its own transaction and privacy semantics rather than weakening atomicity of these records. No audit query/UI, actor authorization policy, retention/cleanup, or export exists yet.

Integration tests cover schema constraints, immutability, caller-owned rollback, successful password-change, project-creation, and project-membership audit, and rollback when audit persistence fails. Migration 0013 extends project targets; migration 0023 preserves prior rows while adding constrained project-membership subjects and role transitions.
