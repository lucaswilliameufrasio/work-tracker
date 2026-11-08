# Durable job processing foundation

This design follows [ADR 0005](adr/0005-sqlite-job-recovery.md). The application SQLite database is the durable queue; `outbox_events` remains an immutable event journal and is not a lease table.

## Slice 251 — durable store

The internal `JobStore` persists versioned JSON-object payloads, a stable deduplication key, optional source outbox event, availability time, an explicit maximum attempt count, and state. Enqueue is atomic and idempotent for an exact repeated specification. Reusing a key for different work conflicts. The trusted producer supplies an explicit outstanding-work ceiling for its `job_kind`; all calls for one kind must use the same configured ceiling. The store checks that kind's pending plus running rows inside `BEGIN IMMEDIATE`, returning a capacity error before accepting new work. Exact replay remains successful even when the queue is full. This bounds each registered producer kind, not the sum of an unbounded set of future kinds; handler registration/configuration must remain code-owned and finite.

Claiming is a short `BEGIN IMMEDIATE` transaction. It selects only due `pending` work, increments attempt count and claim epoch, and writes a fresh opaque claim token plus lease expiry. A heartbeat and terminal transition require that exact token/epoch and an unexpired lease; heartbeat cannot revive an expired claim. An expired `running` row is deliberately not claimable. Permanent failure is terminal; transient failure is rescheduled only when the handler supplies a future retry time, and exhausting the job's declared attempt count becomes terminal. Error codes are bounded identifiers, not arbitrary error messages. Pending jobs can be canceled; canceling in-flight work belongs to the supervisor.

`Database::open` runs abandoned-claim recovery after acquiring the installation's exclusive directory lock and applying migrations. Recovery increments the epoch before requeueing, or marks the job failed when its attempt budget is exhausted. A stale claim therefore cannot complete after restart. At runtime, only the future supervisor may recover a timed-out task, and it must cancel and join that task before updating the row. The store has no public "steal expired lease" operation.

Slice 251 intentionally did not install a worker loop or register handlers: there was no real job consumer, and a no-op dispatcher would incorrectly mark outbox events handled. It exposed no externally callable job-submission route/service, did not publish events or call external services, and did not create an outbox-to-jobs handoff. Those remain subsequent slices with concrete handler contracts.

## Slice 252 — runtime task ownership primitive

`supervisor::SupervisedJobTask` binds a claimed job to one Tokio task. A caller may await natural completion or request local abort and then await the join. Only a joined cancellation/panic yields the opaque `StoppedClaim` accepted by `JobStore::recover_after_worker_join`; recovery rechecks the exact token/epoch, advances the epoch, clears the lease, and either requeues immediately as `worker_interrupted` or fails exhausted work. A panic is terminal as `worker_panicked`, rather than being silently retried as a transient handler error. This is the cancel→join→fence ordering established by the earlier bundle-job experiment and ADR0005. Dropping the task owner requests local abort but cannot join in `Drop`, so it intentionally yields no recovery proof; the row stays `running` until safe startup recovery after the installation lock is reacquired.

This is an ownership primitive, not the application worker loop: it does not claim jobs, renew leases, choose concurrency/shutdown budgets, or register handlers, and it is not started by `main`. Tokio abort confirms that the local future stopped being polled, not that an already-issued remote effect was canceled; external handlers still need idempotency/reconciliation. The issue outbox remains untouched. Until automation rules/actions and a real handler contract exist, no event is converted to a job and no dispatcher is activated.

## Data and privacy bounds

- Payload is a JSON object, maximum 64 KiB encoded, with a positive schema version. Payload schemas are owned by each future handler; producers should store only the minimum identifiers/configuration needed and must not put secrets or issue free text in the outbox.
- Job kinds, dedupe keys, source event UUIDs and error codes are validated before persistence. Database constraints independently enforce byte, enum, JSON, foreign-key, lease/state and attempt invariants.
- Job identity and payload are immutable after enqueue. State, availability, attempts, lease, safe error code and timestamps are mutable only through the typed store transitions.
- No terminal-job retention/purge endpoint exists yet. Do not treat current storage retention as a production policy.

## Required verification

Use migrated file-backed SQLite. Cover exact duplicate acceptance and payload conflict, outstanding-capacity concurrency, due-time ordering, concurrent single-winner claims, heartbeat/expiry/fencing, explicit transient retry and attempt exhaustion, cancellation, terminal transition rollback, startup recovery after closing/reopening the database, stale epoch rejection, and migration preservation of prior outbox/history/receipts. Full `mise exec -- make check` remains required. Tests are local and synthetic; they do not prove production capacity, external delivery, or power-loss durability.
