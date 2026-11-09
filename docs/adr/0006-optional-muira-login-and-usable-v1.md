# ADR 0006: Optional Muira login and the usable first version

- **Status:** Accepted
- **Date:** 2026-11-08
- **Email clarification:** 2026-11-08 — the usable version ships Sabiá, MillionSend, and Resend.
- **Context:** `docs/development-plan.md`, sections 7 and 10; [ADR 0001](0001-authorization-server-selection.md); [ADR 0002](0002-backup-archive-format-deferred.md)

## Context

The Kanban surface can create, move, edit, and search issues once a Puxiri session exists. There is no setup completion, no browser login, and no way to create the first project from the interface. The operator wants to use one local installation to track the remaining work, choosing Muira during setup rather than waiting for Puxiri's own authentication to be complete.

Muira is an existing OIDC identity provider (Rust, Axum, PostgreSQL, authorization code and PKCE, RS256 and JWKS). `sqlite-replication` is an existing Rust library for single-writer SQLite capture, spool, and restore. Its local gate covers the tested Linux x86-64 combinations and one coordinated writer; it does not claim physical power-loss safety or a homologated AWS S3, R2, or other production store. `database-migrator` is an existing Rust migration library and `dbm` tool with SQLite support. Its API and metadata are provisional, and its own plan says the complete V1 is not implemented. Puxiri already applies its own numbered SQL migrations.

## Decision

- Setup offers two authentication sources. **Muira** is an optional OIDC relying-party login. **Puxiri** remains the built-in source and may stay incomplete while Muira is the selected source. The operator will select Muira for the installation used to develop Puxiri.
- Muira authenticates. It does not authorize. After a successful Muira login, Puxiri validates the issuer, audience, signature, expiry, and subject, then issues its own session cookie. That cookie identifies the person. Membership, roles, and permissions stay in Puxiri. A Muira login does not create an invitation or a membership. A person who authenticated and has no invitation sees a screen that says an invitation is required. The initial administrator created by setup is the exception, because setup creates that membership directly. A Muira token does not authorize an issue, project, or MCP call by itself.
- Muira's PostgreSQL and process stay outside the Puxiri data directory. Selecting Muira means that installation depends on a reachable Muira. Selecting Puxiri does not require Muira.
- The usable version required before calling the installation fit for daily use is: this setup choice, the first project, comments, a view of the signed-in person's items, invitation with email, and MCP access to the work surface. That email support includes all three providers: Sabiá, MillionSend, and Resend. Labels, custom fields, live updates, product file storage, automations, and the administrative panel wait until that use is underway.
- Continuous disaster recovery of the Puxiri SQLite database uses the Rust library in `sqlite-replication`, with one coordinated writer and an explicit capture boundary. This does not select the encrypted archive format. ADR 0002 stays Deferred.
- `database-migrator` is the intended migration runner. Existing Puxiri SQL migrations remain the source of schema history until a later slice validates them through that runner. Adopting the runner must not block the first login.
- MCP uses Muira to authenticate the person. Puxiri remains responsible for invitation, membership, project consent, and every tool check. A Muira token does not authorize an MCP call by itself. ADR 0001 remains the record that no embedded authorization-server crate was selected. It does not forbid this Muira authentication flow.
- The usable version delivers three email providers: Sabiá, MillionSend, and Resend. All three are part of the product, not a menu from which only one integration is built. An installation that does not send email leaves them unconfigured.

## Alternatives considered

- Finishing Puxiri password, TOTP, and the full setup wizard before any use delays the board behind authentication work the operator will not exercise on this installation.
- Making Muira mandatory would remove the built-in authentication the plan already started and would force every installation to run PostgreSQL.
- Treating a successful Muira login as permission to enter the installation was rejected. Authentication without an invitation shows the invitation message and grants no work access.
- Replacing ADR 0002 with replication would drop the offline encrypted bundle and recovery kit without a format decision.
- Rewriting the applied migrations into `database-migrator` before login would put a provisional runner on the critical path of an already migrated schema.
- Shipping only Sabiá, and treating MillionSend and Resend as optional later providers, was the rejected reading of the operator's list. The usable version includes the three integrations.

## Consequences and limits

- A Muira installation is no longer a single process. Puxiri's own SQLite file remains one writer; Muira is a separate identity service.
- The built-in authentication is an explicit incomplete path, not a claim that password login, TOTP enrollment, or account recovery are ready for use.
- Invitation email is sent through one of the three delivered providers, not by Muira. Each provider keeps its own API key in installation configuration, not in the repository. Sabiá receives a published template, recipient, and data such as the invite link; its `202` means the message was accepted into Sabiá's queue, not delivered. MillionSend and Resend are separate direct integrations. Sabiá posts MillionSend to `https://api.millionsend.com` with a Resend-shaped request, but that does not make the two Puxiri integrations the same client. Acceptance by a provider is not proof of delivery.
- Replication inherits the current limits of `sqlite-replication`: no power-loss guarantee, no production object-store homologation, and no replacement for the deferred encrypted bundle.
- The migration runner's API may change. Pin the reviewed revision when the adoption slice starts, and keep checksums of the SQL Puxiri has already applied.
- MCP tools can be specified against the existing work surface, but remote production OAuth stays blocked by ADR 0001 until a separate authorization-server decision.

## Evidence

- The operator confirmed this scope on 2026-11-08: Muira is optional at setup and is the selected source for the installation they will use; Puxiri authentication may remain incomplete; comments, the personal item view, invitation and email, and MCP belong to the usable version; the rest can follow after the product is in use. Disaster recovery and migrations should use `sqlite-replication` and `database-migrator`. The same day, the operator required the usable version to deliver three email providers: Sabiá, MillionSend, and Resend. An earlier note that treated this as a single selected integration was corrected. The operator then confirmed that Muira only authenticates: a person who logs in through Muira without an invitation sees a message that an invitation is required, and authorization remains in Puxiri. The operator also allowed Muira in the MCP flow for authentication, with Puxiri still authorizing the call.
- Sabiá's repository documents `POST /v1/messages` with an application API key, `Idempotency-Key`, `channel: email`, and a published template. The response `202` accepts the message into Sabiá's queue. Its consumer sends Resend to `https://api.resend.com/emails` and MillionSend to `https://api.millionsend.com` through the same request shape. Sabiá's own gap note says that partial compatibility does not prove the MillionSend contract equals Resend.
- Muira's repository documents OIDC discovery, authorization code with PKCE, and RS256/JWKS. It requires PostgreSQL.
- The local `sqlite-replication` README records the verified capture and restore scope and the limits stated above.
- The `database-migrator` README documents SQLite execution and states that its complete V1 and API compatibility are not yet guaranteed.
