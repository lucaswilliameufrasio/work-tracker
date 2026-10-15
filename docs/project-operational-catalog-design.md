# Operational project catalog — feature design

**Status:** Internal slice 212 implemented; no transport/UI.
**Source:** `docs/development-plan.md` §6 separates sidebar access from owner/admin operational oversight.

Only an active organization owner/admin may list the operational catalog, without joining projects or granting content access. Installation-admin alone, project manager, member, viewer, guest and inactive accounts are denied. Each request reloads actor authority and reads the catalog in one SQLite read transaction.

`ProjectStore::list_operational_catalog` returns ID, stable key, name, optional archive timestamp, membership count and issue count. Membership count includes all retained project memberships regardless of account state. Issue count includes all retained issues, including trash, completed and canceled work: it is a storage/oversight total, not a board count. It returns no description, issue IDs/titles/text, email, tokens or automation payloads. It does not append audit for reads or change membership.

It reuses `ProjectListOptions`: keyset pagination by immutable project key ascending, 1–100 items/default50, one-row lookahead, archived projects excluded by default and explicitly included via `include_archived`. Unknown/unauthorized actors are unavailable; invalid bounds/cursor and malformed stored archive timestamps fail explicitly. Counts are read in the same authorization snapshot, with no multi-request frozen snapshot guarantee.

The accessible navigation listing remains separate and continues requiring explicit membership. Seeing a project in this operational catalog never authorizes reading any of its issues. Tests cover that boundary, counts and their current state, filters/pagination, role revocation and input/storage errors. This is not an admin screen, audit browser or HTTP boundary.
