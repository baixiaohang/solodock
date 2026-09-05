CREATE TABLE image_cleanup_previews (
    token_hmac BLOB PRIMARY KEY,
    session_id TEXT NOT NULL,
    plan_json TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT
);
CREATE TABLE image_cleanup_operations (
    operation_id TEXT PRIMARY KEY,
    token_hmac BLOB NOT NULL REFERENCES image_cleanup_previews(token_hmac),
    plan_json TEXT NOT NULL,
    plan_hash BLOB NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE image_cleanup_items (
    operation_id TEXT NOT NULL REFERENCES image_cleanup_operations(operation_id),
    ordinal INTEGER NOT NULL,
    image_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('planned','started','removed','retained')),
    PRIMARY KEY (operation_id, ordinal),
    UNIQUE(operation_id, image_id)
);
