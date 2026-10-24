# ADR 0005: Durable SQLite jobs with supervised lease recovery

- **Status:** Accepted
- **Date:** 2026-10-24
- **Context:** `docs/development-plan.md`, sections 2, 4 and 11; the Phase 1 requirement for outbox/jobs/leases

## Context

Issue mutations already append immutable outbox envelopes in the same SQLite transaction as their state changes. The product has no durable job table or worker supervisor yet. The deployment is one application process per data directory, backed by local SQLite; introducing a separate broker would violate the product architecture. A lease timeout can identify a claim that needs attention, but it cannot prove that an asynchronous task or external request from the old claim has stopped.

## Decision

- Persist jobs in the application SQLite database. Keep the outbox append-only and distinct from mutable job execution state.
- Give each claim a monotonically increasing fencing epoch and a fresh opaque claim token. Only the current token/epoch, with an unexpired lease, may heartbeat or commit a result.
- Never make an expired running claim eligible merely because its deadline passed. During process execution, the supervisor must request cancellation, await the owned task's termination, and only then fence/requeue it. On process startup, recovery may run only after `Database::open` has acquired the exclusive data-directory lock, which establishes that the prior application process no longer owns the installation.
- Persist local effects and job completion in one SQLite transaction when a concrete handler is added. External effects remain at-least-once; handlers must use downstream idempotency/reconciliation and must not interpret timeout as proof that an effect did not happen.
- Require each trusted producer to supply a consistent outstanding-job admission limit for its code-owned job kind, a bounded payload, a stable deduplication key, and a maximum attempt count. Retry times are explicit producer/handler decisions; the store performs no hidden retry or delay. No externally callable job-submission route/service or worker handler is enabled by this foundational slice.

## Alternatives considered

- Reclaiming every expired lease directly in `claim_next` is simpler, but can overlap a still-running worker and duplicate external effects. Fencing protects database writes, not a request already in flight outside SQLite.
- Adding a broker or separate worker process would contradict the single-process/local-SQLite architecture and add operational dependencies without measured need.
- Treating outbox rows themselves as mutable queue state would destroy their append-only event history and couple event retention to execution attempts.

## Consequences and limits

- SQLite serializes claims and state transitions with short `BEGIN IMMEDIATE` transactions. Durable acceptance occurs only after enqueue commits.
- A process restart can recover abandoned claims safely under the installation lock. Runtime timeout recovery still requires an application polling/scheduling supervisor; the task ownership primitive does not claim jobs or renew leases, so an expired claim remains unavailable until startup recovery or explicit joined recovery.
- This decision does not select a generic retry/backoff curve, queue capacity default, retention/purge policy, handler payload schemas, concurrency target, or production SLO. Those must be set by concrete producers and measured operational requirements.
- Slice 251 provided persistence and lifecycle primitives only. It did not dispatch the outbox, start a background worker, publish events, or claim delivery guarantees.
- Runtime recovery code must carry proof that the task owning a claim was joined before it can fence/requeue that claim. A timeout or expired lease is not that proof. The currently implemented `SupervisedJobTask` is only this ownership primitive; application polling, heartbeat cadence, concurrency, shutdown budget, concrete handlers and the outbox handoff remain separate decisions/slices.

## Evidence

- The user selected supervised recovery plus fencing on 2026-10-24. The prior bundle-job experiment documented that a lease timeout alone does not prove worker death and required the supervisor to join/stop the owned worker before recovery; see `docs/development-plan.md`, slice 7 bundle-job evidence.
- The current architecture requires a single process per local data directory and forbids adding a queue service without measured need; see `docs/development-plan.md`, sections 2 and 2.1.
