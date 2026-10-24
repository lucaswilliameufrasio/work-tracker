# Durable job processing foundation

This design follows [ADR 0005](adr/0005-sqlite-job-recovery.md). The application SQLite database is the durable queue; `outbox_events` remains an immutable event journal and is not a lease table.

## Current slice

The internal `JobStore` persists versioned JSON-object payloads, a stable deduplication key, optional source outbox event, availability time, an explicit maximum attempt count, and state. Enqueue is atomic and idempotent for an exact repeated specification. Reusing a key for different work conflicts. The trusted producer supplies an explicit outstanding-work ceiling for its `job_kind`; all calls for one kind must use the same configured ceiling. The store checks that kind's pending plus running rows inside `BEGIN IMMEDIATE`, returning a capacity error before accepting new work. Exact replay remains successful even when the queue is full. This bounds each registered producer kind, not the sum of an unbounded set of future kinds; handler registration/configuration must remain code-owned and finite.

Claiming is a short `BEGIN IMMEDIATE` transaction. It selects only due `pending` work, increments attempt count and claim epoch, and writes a fresh opaque claim token plus lease expiry. A heartbeat and terminal transition require that exact token/epoch and an unexpired lease; heartbeat cannot revive an expired claim. An expired `running` row is deliberately not claimable. Permanent failure is terminal; transient failure is rescheduled only when the handler supplies a future retry time, and exhausting the job's declared attempt count becomes terminal. Error codes are bounded identifiers, not arbitrary error messages. Pending jobs can be canceled; canceling in-flight work belongs to the supervisor.

`Database::open` runs abandoned-claim recovery after acquiring the installation's exclusive directory lock and applying migrations. Recovery increments the epoch before requeueing, or marks the job failed when its attempt budget is exhausted. A stale claim therefore cannot complete after restart. At runtime, only the future supervisor may recover a timed-out task, and it must cancel and join that task before updating the row. The store has no public "steal expired lease" operation.

This slice intentionally does not install a worker loop or register handlers: there is no real job consumer yet, and a no-op dispatcher would incorrectly mark outbox events handled. It exposes no externally callable job-submission route/service, does not publish events or call external services, and does not create an outbox-to-jobs handoff. Those are subsequent slices with concrete handler contracts.

## Data and privacy bounds

- Payload is a JSON object, maximum 64 KiB encoded, with a positive schema version. Payload schemas are owned by each future handler; producers should store only the minimum identifiers/configuration needed and must not put secrets or issue free text in the outbox.
- Job kinds, dedupe keys, source event UUIDs and error codes are validated before persistence. Database constraints independently enforce byte, enum, JSON, foreign-key, lease/state and attempt invariants.
- Job identity and payload are immutable after enqueue. State, availability, attempts, lease, safe error code and timestamps are mutable only through the typed store transitions.
- No terminal-job retention/purge endpoint exists yet. Do not treat current storage retention as a production policy.

## Required verification

Use migrated file-backed SQLite. Cover exact duplicate acceptance and payload conflict, outstanding-capacity concurrency, due-time ordering, concurrent single-winner claims, heartbeat/expiry/fencing, explicit transient retry and attempt exhaustion, cancellation, terminal transition rollback, startup recovery after closing/reopening the database, stale epoch rejection, and migration preservation of prior outbox/history/receipts. Full `mise exec -- make check` remains required. Tests are local and synthetic; they do not prove production capacity, external delivery, or power-loss durability.
