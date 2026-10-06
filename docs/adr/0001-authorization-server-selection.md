# ADR 0001: Authorization server selection is deferred

- **Status:** Deferred — product/architecture decision required
- **Date:** 2026-10-06
- **Context:** `docs/development-plan.md`, sections 10.1–10.3 and Phase 0 evidence 37–41

## Context

Puxiri needs OAuth authorization for remote MCP clients while preserving a single Rust/Axum binary, the existing SQLite database and migrations, local Puxiri identity/session/consent, PKCE S256, current membership authorization, audience/resource binding, revocation, MCP protected-resource metadata and client-specific consent. CIMD is a separate trust-review and user-consent state and remains unavailable until its SSRF/cache/security boundary is implemented.

The candidate review and RustAuth 0.3.0 probes are recorded in the development plan. RustAuth compiles with Axum and passed a small, temporary shared-SQLite-file smoke, but its composed core/OAuth schema conflicts with Puxiri's `users`, `sessions` and OAuth domain and would require uncontrolled schema integration. `mcp-oauth`, `auth-framework`, Garrison and TurboMCP Auth do not provide the required combination of AS persistence, Puxiri identity/consent and MCP authorization profile. `oxide-auth` exposes useful OAuth 2.0 primitives and required PKCE, but its release/async integration and the additional MCP protocol boundary remain unvalidated.

## Decision

Do not select or add an authorization-server dependency yet. Do not implement production OAuth endpoints or enable CIMD. Preserve this as an explicit blocker rather than inferring that an OAuth client library, test server, fixture, or MCP resource-server helper is an authorization server.

Before resuming implementation, choose between:

1. A maintained OAuth core library plus a narrowly scoped MCP authorization profile, with a reviewed storage/identity/consent boundary and no hand-rolled general OAuth protocol; or
2. A separate/managed authorization server, which requires revisiting the single-binary/self-hosted product constraint; or
3. A different upstream solution that satisfies the current constraints.

Regardless of the choice, Puxiri's application layer remains authoritative for current user status, project membership and permission checks. OAuth scopes or token claims do not replace that check. Metadata never grants client trust, consent or project access.

## Consequences

- MCP remote acceptance and production OAuth remain blocked; do not claim the four clients are validated.
- Continue independent Phase 0 work without connecting real accounts, opening public login flows, or fetching CIMD documents.
- A future choice requires a new ADR amendment/decision and isolated security, migration, and client-compatibility evidence before product integration.
- This ADR does not approve the RustAuth spike for production and does not modify product dependencies, schema, endpoints or credentials.

## Evidence

- `docs/development-plan.md`, evidence 37–41, including the per-candidate source links and explicit limitations.
- Temporary RustAuth probes were outside the repository; no production database or credentials were used.

## Candidate follow-up: `oauth-as` 1.0.0

The 2026-10-06 review added `oauth-as` 1.0.0 as a plausible modular-core candidate; it does **not** change the deferred status or select a dependency. The crate was published on 2026-09-24, is MIT OR Apache-2.0, offers an optional Axum adapter, and defines an async host-implemented `Storage` contract rather than requiring a second database or fixed product schema. Its documented/server APIs cover authorization-code PKCE, RFC 8707 resource indicators, consent records/revocation, configurable client registration, metadata, and optional CIMD. A temporary compile-only probe outside the repository compiled `AuthorizationServer` + `MemoryStorage` + `ServiceBuilder` into the existing Axum 0.8.9 `Router` with pinned `oauth-as = 1.0.0`; no product dependency, schema, endpoint, or credential was changed.

This makes it a better *spike candidate* for the authorized single-binary direction, not a drop-in. No first-party SQLite storage adapter was established in this review; Puxiri would own a substantial persistent adapter and prove the crate's atomic take/compare-and-swap, cascade, revocation-barrier, and conformance contracts against its SQLite transactions. The HTTP adapter deliberately has no login/session system: its subject, approval, and CSRF hooks are synchronous closures over request headers, so fitting Puxiri's async, current-state session and identity checks requires a reviewed seam rather than assuming middleware/session integration. The crate's consent record is not Puxiri project membership or the product's project-specific consent policy. The Axum compile probe did not exercise a request, grant, PKCE exchange, storage implementation, metadata, or client.

Before any selection or product implementation, a bounded isolated spike must review the exact 1.0.0 source/release and dependency/MSRV/security posture; implement or prototype the SQLite `Storage` contract against a disposable DB and upstream conformance tests; demonstrate session/approval/CSRF and Puxiri consent integration without bypassing current membership checks; and verify configured issuer/resource/audience, revocation and MCP metadata behavior. CIMD remains disabled pending its separate SSRF/cache/trust review. Four-client acceptance and any live-provider smoke remain separate gates, with live smoke requiring explicit authorization and controlled accounts.

Sources: [crate/release metadata](https://crates.io/api/v1/crates/oauth-as/1.0.0), [API documentation](https://docs.rs/oauth-as/1.0.0/oauth_as/), [published source](https://docs.rs/crate/oauth-as/1.0.0/source/), and the reproducible compile-only details/limitations in development-plan evidence 59.
