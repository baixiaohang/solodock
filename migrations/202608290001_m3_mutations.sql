CREATE TABLE idempotency_records (
    actor TEXT NOT NULL,
    route TEXT NOT NULL,
    key_hmac BLOB NOT NULL,
    request_hmac BLOB NOT NULL,
    operation_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'succeeded', 'interrupted', 'failed')),
    response_status INTEGER,
    response_body TEXT,
    error_code TEXT,
    effect_phase TEXT CHECK (effect_phase IS NULL OR effect_phase IN ('started', 'completed')),
    pre_container_id TEXT,
    pre_started_at TEXT,
    post_container_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (actor, route, key_hmac)
);

CREATE TABLE deletion_previews (
    token_hash BLOB PRIMARY KEY,
    session_id TEXT NOT NULL,
    app_id TEXT NOT NULL,
    slug TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    preview_hash BLOB NOT NULL,
    preview_json TEXT NOT NULL,
    remove_container INTEGER NOT NULL,
    container_ids_json TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX deletion_previews_app_id ON deletion_previews(app_id);
