# Protected setup bootstrap — feature design

**Status:** Incremental implementation; public setup flow is not implemented.
**Source of requirements:** `docs/development-plan.md`, sections 7.1 and 8.

## Actors and journey

- **Local operator:** starts the empty installation and obtains the bootstrap token from a local terminal. Local host access is the authority to initialize the first administrator.
- **Setup wizard:** a future same-origin browser flow that accepts the token and gathers organization, owner, recovery, canonical URL, email, login, signup, and backup settings.
- **Initial administrator:** becomes both organization owner and installation administrator; is not automatically added to future projects.

The operator-issued token must never be replaceable by the first public visitor. Setup completion must atomically consume the token, create the organization and initial administrator, and set `installation_state.setup_completed_at`. The setup route must be absent after completion. This transaction and route are not implemented by the current slice.

## Current token lifecycle

States are: incomplete/no token → incomplete/current token → complete/consumed token. The current implementation slice adds only the local token store and terminal-only `setup-token` command; later slices must provide atomic setup completion and token consumption.

The token is 256-bit random material. SQLite stores only its SHA-256 digest. Issuing through the local command rotates any prior unconsumed token. The command refuses to print the token when stdout is not an interactive terminal, avoiding accidental disclosure through service logs or pipes. It must run while the server is stopped because the data-directory lock is exclusive.

The plan specifies one-time use but no token TTL. Until product review establishes another policy, an issued token does not expire automatically; it is invalidated by local reissue or successful setup. This preserves restart recovery without silently inventing a duration. The command is local host authority, not an HTTP recovery endpoint.

## Security boundary and remaining work

- The local token command is not a setup wizard and does not expose a network route.
- Future browser routes must validate `Origin`, use a same-origin boundary, and avoid ambient-cookie-only authorization; CSRF, safe error responses, and request limits are mandatory before public exposure.
- Invalid/failed setup attempts must not consume the token; final completion and token consumption must share one SQLite write transaction.
- Email confirmation and primary-email selection remain separate: an unconfirmed address cannot become primary or be used for login. The wizard needs a safe local confirmation path when outbound email is not configured.
- Recovery-kit cryptography and backup format remain deferred by ADR 0002. Do not enable encrypted backups or claim recovery readiness before that decision and the required export gate.
- The server now loads or creates the master-key file at startup (default `<data-dir>/master.key`; optional `--master-key` / `PUXIRI_MASTER_KEY`) and keeps it in memory. No product value is encrypted yet, and authenticated encryption/AAD/key rotation remain unimplemented.

Revisit the no-TTL behavior if setup-token exposure or operational requirements demonstrate a need for expiration. If that happens, define expiry and local reissue behavior together so an unattended/restarted install cannot be stranded.
