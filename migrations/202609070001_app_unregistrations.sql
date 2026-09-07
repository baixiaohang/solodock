-- Unlike idempotency responses, these lifecycle facts must survive ordinary GC.
CREATE TABLE app_unregistrations (
    app_id TEXT PRIMARY KEY NOT NULL,
    operation_id TEXT NOT NULL UNIQUE,
    source TEXT NOT NULL CHECK (source IN ('deletion', 'operator_repair')),
    completed_at TEXT NOT NULL,
    finalized INTEGER NOT NULL DEFAULT 0 CHECK (finalized IN (0,1))
);
