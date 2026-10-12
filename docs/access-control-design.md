# Authorization policy matrix — feature design

**Status:** The deny-by-default Rust capability policy and SQLite loading of the current account/organization/installation-admin context are implemented. Project membership loading (the schema does not exist yet), session-to-principal wiring, HTTP/MCP/job/download/SSE enforcement, audit browsing, and route integration are not implemented.

**Requirements:** `docs/development-plan.md`, sections 5, 6, and 13.

## Actor context and policy

Every decision uses the account state, installation-admin flag, current organization role, current project role, project archived state, and whether a recent privileged-action proof (password reauthentication or MFA step-up) has been verified. Inactive accounts are denied before role checks. Invalid membership combinations fail during context construction: project membership requires organization membership; a guest cannot be project manager or installation admin; installation admins must be organization members.

`AccessStore::load_organization_context` performs one fresh SQLite query joining account state, organization membership, and the separate installation-admin table. It returns `None` for an unknown account, strictly parses persisted state/role values, and fails closed for malformed values or broken admin-membership invariants. It deliberately supplies no project role because `projects`/`project_members` have not been migrated. The caller must pass a user ID from a session it has already validated; the store neither authenticates sessions nor caches the result. Its recent-proof boolean is also a trusted caller assertion, not proof validation.

The `AccessCapability` matrix expresses the policy table without treating roles as content access by themselves:

- Installation-admin capabilities are operational health, installation settings (auth/email/CAPTCHA/backup), and user ban. Full-backup download requires recent password reauthentication or MFA step-up. Installation-admin status alone does not grant project access.
- Organization owners/admins can manage organization membership, create projects, manage project membership, and archive/unarchive projects; only the owner can transfer ownership. Those org roles do not grant project content access.
- Project managers can read/write/manage schema and membership, export, and archive/unarchive only their project. Members can read and mutate issues; viewers can read only. Guests can be members or viewers but cannot become managers. Callers assigning project roles must independently prevent role elevation and must not grant a guest manager.
- Archived projects permit reads and manager exports; organization owners/admins and project managers can archive/unarchive without gaining content access. Ordinary mutations and schema/membership administration are denied until reopened.

## Boundary and remaining work

The policy and store are not yet an authorization boundary: no endpoint, job, or use case loads this context and calls `authorize` before data access/mutation. The module does not prove that a project/user ID belongs to the selected installation, implement last-manager invariants, or provide transaction fencing against concurrent role changes. Those belong in typed application use cases and repository-backed integration tests. Do not cache long-lived claims or use frontend checks as authorization.
