CREATE TABLE app_retention (
    app_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
    keep_versions INTEGER NOT NULL CHECK(keep_versions BETWEEN 1 AND 100),
    revision TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_checked_at TEXT,
    last_status TEXT,
    last_error_code TEXT
);
CREATE TABLE automatic_cleanup_authorizations (
    operation_id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    policy_json TEXT NOT NULL,
    cleanup_kind TEXT NOT NULL CHECK(cleanup_kind IN ('artifacts','images')),
    plan_hash BLOB NOT NULL,
    result_json TEXT,
    created_at TEXT NOT NULL
);
-- Automatic image operations have policy authorization instead of a preview.
-- Rebuild both tables together to preserve all existing foreign keys and ledgers.
CREATE TABLE image_cleanup_operations_new (
    operation_id TEXT PRIMARY KEY,
    token_hmac BLOB REFERENCES image_cleanup_previews(token_hmac),
    plan_json TEXT NOT NULL,
    plan_hash BLOB NOT NULL,
    created_at TEXT NOT NULL
);
INSERT INTO image_cleanup_operations_new SELECT * FROM image_cleanup_operations;
CREATE TABLE image_cleanup_items_new (
    operation_id TEXT NOT NULL REFERENCES image_cleanup_operations_new(operation_id),
    ordinal INTEGER NOT NULL,
    image_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('planned','started','removed','retained')),
    PRIMARY KEY(operation_id,ordinal),
    UNIQUE(operation_id,image_id)
);
INSERT INTO image_cleanup_items_new SELECT * FROM image_cleanup_items;
DROP TABLE image_cleanup_items;
DROP TABLE image_cleanup_operations;
ALTER TABLE image_cleanup_operations_new RENAME TO image_cleanup_operations;
ALTER TABLE image_cleanup_items_new RENAME TO image_cleanup_items;
