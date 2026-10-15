# Account anonymization — feature design

**Status:** Internal transactional operation implemented in slice 209; no HTTP/MCP/CLI transport or request-review workflow.
**Requirements:** `docs/development-plan.md`, section 7 and the account privacy requirements in section 13.
**Decision source:** The operator confirmed preserving organization/project memberships and scrubbing invitation recipient addresses.

## Authority and lifecycle

An active installation admin or organization owner may anonymize another account. Self-anonymization is rejected so the actor remains distinct in the audit event. The operation accepts active, banned, or disabled accounts, but is irreversible and idempotent: an already-anonymized account returns unchanged. It marks the user disabled, stores `anonymized_at`, and replaces the display name with `Usuário removido`. A database trigger prevents inserting a user already marked anonymized; the transition must start from an existing identity. Reactivation is permanently denied.

Before mutation, the transaction requires another **active** organization owner, installation admin, and project manager for every project managed by the target, when applicable. Organization/project membership rows and authored work remain to preserve attribution and existing history. The installation-admin grant is removed; organization and project roles remain attached to the retained pseudonymous user row.

## Data handling

In one `BEGIN IMMEDIATE` transaction, the operation removes user email addresses, external auth identities, password credentials, all TOTP credentials, sessions, password-reset/email-verification tokens, recovery codes/sessions/enrollments, MFA login challenges, and the installation-admin grant. Future user-bound credentials or grants must join this deletion path before they are exposed.

Authentication rate-limit bucket keys are HMACs of identities/IPs and have no user foreign key or raw email value. The anonymization transaction does not remove these opaque buckets; a bounded maintenance task must purge aged windows, and that maintenance is not yet wired.

Invitation records addressed to any former account email are retained. Their recipient is replaced in storage by a per-invitation `invalid.invalid` marker and `recipient_redacted_at` is set. Any not-yet-accepted invitation is canceled in the same transaction; accepted/canceled lifecycle history and project assignments remain unchanged. Invitation listings return no email for redacted recipients. The database permits this one-way redaction and no other mutation of invite identity.

The user row remains because immutable audit events, issue authorship, memberships, and invitation-creator history reference it. `auth.user.anonymized` records a distinct active actor and the opaque user ID in the same transaction. Historical immutable audit text, user-authored free text, backups, and external exports are not rewritten. This operation is not by itself a legal-compliance determination or a guarantee that all copies/free text contain no personal data; requests require a separate retention and backup review.

## Failure and verification

Any database or audit failure rolls back the identity change, invite redaction, credential deletion, and role-grant removal together. Integration tests use the migrated SQLite database and cover authorization, responsibility replacements, lifecycle/credential deletion, membership/authorship retention, invite redaction, immutability/idempotence, and complete rollback on audit failure.
