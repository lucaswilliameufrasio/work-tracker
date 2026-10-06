# ADR 0001: Authorization server selection is deferred

- **Status:** Deferred — product/architecture decision required
- **Date:** 2026-10-06
- **Context:** `docs/development-plan.md`, sections 10.1–10.3 and Phase 0 evidence 37–41, 59, 62–65

## Context

Puxiri needs OAuth authorization for remote MCP clients while preserving a single Rust/Axum binary, the existing SQLite database and migrations, local Puxiri identity/session/consent, PKCE S256, current membership authorization, audience/resource binding, revocation, MCP protected-resource metadata and client-specific consent. CIMD is a separate trust-review and user-consent state and remains unavailable until its SSRF/cache/security boundary is implemented.

The candidate review and RustAuth 0.3.0 probes are recorded in the development plan. RustAuth compiles with Axum and passed a small, temporary shared-SQLite-file smoke, but its composed core/OAuth schema conflicts with Puxiri's `users`, `sessions` and OAuth domain and would require uncontrolled schema integration. `mcp-oauth`, `auth-framework`, Garrison and TurboMCP Auth do not provide the required combination of AS persistence, Puxiri identity/consent and MCP authorization profile. `oxide-auth` exposes useful OAuth 2.0 primitives and required PKCE, but its release/async integration and the additional MCP protocol boundary remain unvalidated.

## Decision

The authorization server must remain embeddable in the existing single Rust/Axum binary and use Puxiri's SQLite and identity/session/consent domain. Do not select or add an authorization-server dependency yet. Do not implement production OAuth endpoints or enable CIMD. Preserve this as an explicit blocker rather than inferring that an OAuth client library, test server, fixture, or MCP resource-server helper is an authorization server.

The user has ruled out a separate authorization-server service and explicitly authorized a bounded, isolated in-house prototype. This authorizes prototype work only, not a production OAuth server, endpoints, dependency/schema changes, external exposure, or client acceptance. The remaining acceptable route is:

1. A sufficiently maintained and reviewed OAuth core library plus a narrowly scoped MCP authorization profile, with a reviewed storage/identity/consent boundary and no hand-rolled general OAuth protocol; or
2. The now-authorized prototype of a narrowly scoped in-house implementation if no acceptable embeddable upstream core can be established; production adoption still requires a separate reviewed decision.

No current candidate meets the evidence bar. `oauth-as` remains unselected: its modular shape is promising, but the reviewed release is recent and effectively single-maintainer, and no independent security review or mature governance/release record was established. Download counts and successful compilation are not evidence of security. This is a trust/evidence gap, not a claim that the crate is vulnerable.

Regardless of the choice, Puxiri's application layer remains authoritative for current user status, project membership and permission checks. OAuth scopes or token claims do not replace that check. Metadata never grants client trust, consent or project access.

## Consequences

- MCP remote acceptance and production OAuth remain blocked; do not claim the four clients are validated.
- Continue independent Phase 0 work without connecting real accounts, opening public login flows, or fetching CIMD documents.
- A future choice requires a new ADR amendment/decision and isolated security, migration, and client-compatibility evidence before product integration.
- This ADR does not approve the RustAuth spike for production and does not modify product dependencies, schema, endpoints or credentials.
- A separate authorization-server service is not authorized; single-binary deployment remains a product requirement.
- The prototype authorization is restricted to isolated state-machine/storage evidence; no production routes or OAuth claims are implied.
- Independent protocol/security review is required before production exposure, in addition to full persistence, consent, identity, interoperability, and recovery gates.

## Evidence

- `docs/development-plan.md`, evidence 37–41, including the per-candidate source links and explicit limitations.
- Temporary RustAuth probes were outside the repository; no production database or credentials were used.

## Candidate review: `oauth-as` 1.0.0 — not selected

The 2026-10-06 review examined `oauth-as` 1.0.0 as a modular-core candidate; it does **not** change the deferred status or select a dependency. The crate was published on 2026-09-24, is MIT OR Apache-2.0, offers an optional Axum adapter, and defines an async host-implemented `Storage` contract rather than requiring a second database or fixed product schema. Its documented/server APIs cover authorization-code PKCE, RFC 8707 resource indicators, consent records/revocation, configurable client registration, metadata, and optional CIMD. A temporary compile-only probe outside the repository compiled `AuthorizationServer` + `MemoryStorage` + `ServiceBuilder` into the existing Axum 0.8.9 `Router` with pinned `oauth-as = 1.0.0`; no product dependency, schema, endpoint, or credential was changed. The user subsequently raised the crate's limited adoption and single-maintainer trust surface; this concern is accepted, and no independent audit/governance evidence was established to resolve it.

At the time of the initial review, this was considered a *spike candidate*, not a drop-in. Following the user's trust concern, it is not a current candidate for further integration absent materially stronger independent maintenance/security evidence or explicit acceptance. No first-party SQLite storage adapter was established; Puxiri would own a substantial persistent adapter and prove the crate's atomic take/compare-and-swap, cascade, revocation-barrier, and conformance contracts against its SQLite transactions. The HTTP adapter deliberately has no login/session system: its subject, approval, and CSRF hooks are synchronous closures over request headers, so fitting Puxiri's async, current-state session and identity checks requires a reviewed seam rather than assuming middleware/session integration. The crate's consent record is not Puxiri project membership or the product's project-specific consent policy. The Axum compile probe did not exercise a request, grant, PKCE exchange, storage implementation, metadata, or client.

Do not spend more integration effort on `oauth-as` unless independent maintenance/security evidence materially changes or the user explicitly accepts its trust model. Any future upstream candidate must first pass provenance, governance, maintenance, security-review, protocol-surface, storage-conformance, and identity/session/consent checks. CIMD remains disabled pending its separate SSRF/cache/trust review. Four-client acceptance and any live-provider smoke remain separate gates, with live smoke requiring explicit authorization and controlled accounts.

Sources: [crate/release metadata](https://crates.io/api/v1/crates/oauth-as/1.0.0), [API documentation](https://docs.rs/oauth-as/1.0.0/oauth_as/), [published source](https://docs.rs/crate/oauth-as/1.0.0/source/), and the reproducible compile-only details/limitations in development-plan evidence 59 and 62–65.
