# My sessions — bounded self-service journey

**Slice223:** Authenticated people inspect and terminate their own sessions at `/settings/sessions`. Existing API ownership/CSRF rules remain authoritative; the screen does not enable setup/login or grant administrative access. The home link enters a separate account-security task, not an all-in-one dashboard. Full-page links provide predictable browser navigation without adding a router dependency.

## Journey and state ownership

1. Load one candidate page (20 records), mark the current session, show UTC creation/activity/expiry and a short opaque reference. Do not invent devices, IP addresses or location. Empty pages with continuation expose an explicit next-page button. Replace pages instead of accumulating history; offer return to the first page.
2. Selecting one session or “end all” opens a dedicated inline confirmation view with a clear target and current/all-session warning. Cancel returns to the same page and restores focus to the triggering action. Confirmation is not a modal; no fake modal semantics/focus trap is needed.
3. Submission disables further actions by transitioning to a single `submitting` state. The existing client fetches fresh CSRF and sends one DELETE. Current/global success clears private list state and shows signed-out completion. Other-session success reloads the first page and announces completion.
4. Read401 clears private state and explains that access is no longer active; read404 explains the API is not enabled. Other read failures offer an explicit reload. Mutation401 becomes signed-out; target404 rereads safely. Other mutation failures discard the list and show an **unconfirmed outcome**, with a read-only “consult sessions” action, never automatic resubmission. A later read failure after a confirmed mutation must not turn that completed mutation into an unconfirmed one.

`session-screen.ts` owns the discriminated-union workflow and lifetime cancellation; the Svelte component owns rendering/focus only. No independent confirmation/loading/error booleans or bearer/storage state. Unmount aborts requests and suppresses late state changes. The server revalidates identity; stale client `is_current` is a display hint, not authority.

## Visual direction and accessibility

Preserve the existing Inter typography and forest palette: background `#f7f8f5`, surface `#ffffff`, text `#17231c`, muted `#506357`, primary `#245c3a`; use danger `#a52b32` with white only for confirmed termination. Dark mode inherits existing tokens and uses `#ffb4b7` danger on `#3b1017`. Left-aligned heading, restrained divided list, dates grouped per session, primary reading actions separated from global termination. On narrow screens actions stack below content; no decorative cards, new fonts, location icons, animation or marketing badges.

The initial design-system search returned irrelevant sales/finance guidance; it was not adopted. A narrower UX search verified explicit destructive confirmation and success feedback. Keep visible keyboard focus, 44px controls, status announcements, meaningful headings and return-focus behavior. Review desktop/mobile dark views and horizontal overflow. The design is intentionally a security list, not a generic sales dashboard.

## Acceptance evidence

Pure workflow tests cover transitions/races/cancellation/uncertain outcomes. Browser tests must also exercise real Rust router/SQLite with temporary synthetic sessions, not replace the owned API with page-route mocks. A local HTTPS fixture may use ephemeral test-only certificates/proxy and an isolated browser context to verify Secure/HttpOnly/SameSite cookie transport and actual Origin/CSRF, confirmations and audit read-back. This remains a controlled local fixture: no login/provider/public deployment, real account data, cross-browser/proxy certification or completed release gate is implied.

The first real Chromium fault test found native transport retransmission of a bodyless DELETE when the connection closes before response headers. “No automatic retry” describes application code, not a guarantee of one wire transmission by browsers/proxies. Session-bound transactions must remain replay-safe; global/current replay returns401 after revocation, selected replay404, with one committed audit per change. Cover both pre-header loss (native retransmission may occur) and an incomplete post-header body (UI must show unconfirmed outcome); do not retry the command in response to either. The test proxy fault behavior is not a production endpoint or changed API success contract.

Remaining browser-hardening acceptance includes history/BFCache/private-page freshness, cross-tab identity/revocation revalidation, additional browser engines, cross-site SameSite navigation/subresource cases, and production certificate/proxy behavior. The controlled fixture does not close those gates or the full login/onboarding/release journey. No forensic memory/cache-erasure guarantee is made.
