# Authorization policy matrix — feature design

**Status:** The deny-by-default Rust capability policy, fresh SQLite loading of current organization/project access context, and transactional project creation with explicit creator-manager membership are implemented. Session-to-principal wiring, HTTP/MCP/job/download/SSE enforcement, audit browsing, and route integration are not implemented.

**Requirements:** `docs/development-plan.md`, sections 5, 6, and 13.

## Actor context and policy

Every decision uses the account state, installation-admin flag, current organization role, current project role, project archived state, and whether a recent privileged-action proof (password reauthentication or MFA step-up) has been verified. Inactive accounts are denied before role checks. Invalid membership combinations fail during context construction: project membership requires organization membership; a guest cannot be project manager or installation admin; installation admins must be organization members.

`AccessStore::load_organization_context` performs one fresh SQLite query joining account state, organization membership, and the separate installation-admin table. It returns `None` for an unknown account, strictly parses persisted state/role values, and fails closed for malformed values or broken admin-membership invariants. It deliberately supplies no project role; although `projects`/`project_members` are now persisted, project-scoped lookup/enforcement is not implemented. `ProjectStore::create` uses the transaction-scoped loader while holding SQLite's writer lock. The caller must pass a user ID from a session it has already validated; the store neither authenticates sessions nor caches the result. Its recent-proof boolean is also a trusted caller assertion, not proof validation.

`AccessStore::load_project_context` joins the requested project with the authenticated user's current account state, organization role, installation-admin membership, and explicit project role, plus the project's archived state. It must not grant content access from owner/admin or installation-admin status; an owner/admin context without project membership exists only to authorize the separately defined membership/archive operations. A caller without project membership or org-admin authority receives `None`, indistinguishable from an unknown project at the transport boundary. Every lookup is fresh; protected mutations must use `load_project_context_in_transaction` so role/archive changes cannot race authorization. Callers still decide a specific capability with `authorize` before touching content.

The `AccessCapability` matrix expresses the policy table without treating roles as content access by themselves:

- Installation-admin capabilities are operational health, installation settings (auth/email/CAPTCHA/backup), and user ban. Full-backup download requires recent password reauthentication or MFA step-up. Installation-admin status alone does not grant project access.
- Organization owners/admins can manage organization membership, create projects, manage project membership, and archive/unarchive projects; only the owner can transfer ownership. Those org roles do not grant project content access.
- Project managers can read/write/manage schema and membership, export, and archive/unarchive only their project. Members can read and mutate issues; viewers can read only. Guests can be members or viewers but cannot become managers. Callers assigning project roles must independently prevent role elevation and must not grant a guest manager.
- Archived projects permit reads and manager exports; organization owners/admins and project managers can archive/unarchive without gaining content access. Ordinary mutations and schema/membership administration are denied until reopened.

## Boundary and remaining work

The policy and store are not yet a general authorization boundary: project creation rechecks the current organization principal and calls `authorize`, and a project context loader now exists, but no endpoint, job, or project-content use case is wired to it. Last-manager invariants and other role assignment/removal rules remain unimplemented. Protected operations still need transaction-scoped loading/fencing and explicit capability checks. Do not cache long-lived claims or use frontend checks as authorization.
