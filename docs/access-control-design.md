# Authorization policy matrix — feature design

**Status:** The deny-by-default Rust capability policy, fresh SQLite loading of current organization/project access context, transactional project creation, and internal project-membership add/change/remove/self-leave commands are implemented. Session-to-principal wiring, HTTP/MCP/job/download/SSE enforcement, audit browsing, and route integration are not implemented.

**Requirements:** `docs/development-plan.md`, sections 5, 6, and 13.

## Actor context and policy

Every decision uses the account state, installation-admin flag, current organization role, current project role, project archived state, and whether a recent privileged-action proof (password reauthentication or MFA step-up) has been verified. Inactive accounts are denied before role checks. Invalid membership combinations fail during context construction: project membership requires organization membership; a guest cannot be project manager or installation admin; installation admins must be organization members.

`AccessStore::load_organization_context` performs one fresh SQLite query joining account state, organization membership, and the separate installation-admin table. It returns `None` for an unknown account, strictly parses persisted state/role values, and fails closed for malformed values or broken admin-membership invariants. It deliberately supplies no project role; the separate project-context loader joins explicit `projects`/`project_members` state. `ProjectStore::create` and `ProjectMembershipStore` use transaction-scoped context loaders while holding SQLite's writer lock. Callers must pass a user ID from a session they have already validated; stores neither authenticate arbitrary IDs nor cache the result. The recent-proof boolean on access loaders remains a trusted caller assertion, not proof validation.

`AccessStore::load_project_context` joins the requested project with the authenticated user's current account state, organization role, installation-admin membership, and explicit project role, plus the project's archived state. It must not grant content access from owner/admin or installation-admin status; an owner/admin context without project membership exists only to authorize the separately defined membership/archive operations. A caller without project membership or org-admin authority receives `None`, indistinguishable from an unknown project at the transport boundary. Every lookup is fresh; protected mutations must use `load_project_context_in_transaction` so role/archive changes cannot race authorization. Callers still decide a specific capability with `authorize` before touching content.

The `AccessCapability` matrix expresses the policy table without treating roles as content access by themselves:

- Installation-admin capabilities are operational health, installation settings (auth/email/CAPTCHA/backup), and user ban. Full-backup download requires recent password reauthentication or MFA step-up. Installation-admin status alone does not grant project access.
- Organization owners/admins can manage organization membership, create projects, manage project membership, and archive/unarchive projects; only the owner can transfer ownership. Those org roles do not grant project content access.
- Project managers can read/write/manage schema and membership, export, and archive/unarchive only their project. Members can read and mutate issues; viewers can read only. Guests can be members or viewers but cannot become managers. `ProjectMembershipStore` now restricts manager assignment to org owner/admin and enforces the guest cap.
- Archived projects permit reads and manager exports; organization owners/admins and project managers can archive/unarchive without gaining content access. Ordinary mutations and schema/membership administration are denied until reopened.

## Boundary and remaining work

### Project membership mutations

`ProjectMembershipStore` changes membership only for users already in the organization; invitations and organization membership creation remain separate. Each command opens `BEGIN IMMEDIATE`, reloads the actor's current account/org/project state, checks `ManageProjectMembership`, and revalidates the target's account and organization membership before writing. The caller must supply the actor ID from a validated session; these services do not authenticate arbitrary IDs.

`ProjectMembershipStore::leave` is separate from administrator removal: any active user with explicit membership can remove only their own membership, preserving their organization account (including a guest leaving their last project). It uses the `LeaveProject` capability, which is denied for archived projects; the last manager must appoint a replacement first. The self-leave audit records the same user as actor and subject, and audit failure rolls back the exit.

Organization owners/admins may assign any project role and are the only actors who may add, demote, or remove a project manager. A user acting only with project-manager authority may add, change, or remove member/viewer roles but cannot appoint or alter manager membership; an owner/admin who is also a project manager retains the stronger organization authority. Organization guests may hold only member/viewer project roles. New membership and role escalation require an active account; a banned/disabled member may still be demoted, while removal remains available to authorized membership managers. An unchanged role is a no-op with no audit event.

Voluntary demotion/removal cannot leave a project without a manager; promote a replacement first. SQLite triggers enforce this invariant for direct role updates/deletes as well as service calls. Ban does not delete membership. The plan requires a project with no active manager to become read-only; ban workflow and that project-wide read-only enforcement remain unimplemented.

Every committed add/change/remove is audited atomically with the project as target, the affected user as `subject_user_id`, and typed before/after project roles. Self-grants are therefore identifiable; audit rows do not contain arbitrary payloads. Audit failure rolls back the membership change. These commands provide neither membership listing nor public routes/UI.

The policy and stores are not yet a general authorization boundary: project creation and project-membership mutations recheck current context transactionally, and a project-context loader exists, but no endpoint, job, or project-content use case is wired to it. Organization membership/role changes, ownership transfer, invitation/exit/ban flows, last-manager-ban read-only behavior, and remaining protected operations still need implementation. Protected operations must use transaction-scoped loading/fencing and explicit capability checks. Do not cache long-lived claims or use frontend checks as authorization.
