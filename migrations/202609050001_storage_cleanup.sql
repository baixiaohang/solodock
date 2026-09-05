CREATE TABLE storage_cleanup_previews (
    token_hmac BLOB PRIMARY KEY,
    session_id TEXT NOT NULL,
    cleanup_kind TEXT NOT NULL CHECK (cleanup_kind IN ('artifacts')),
    facts_hash BLOB NOT NULL,
    preview_json TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE storage_cleanup_operations (
    operation_id TEXT PRIMARY KEY,
    cleanup_kind TEXT NOT NULL CHECK (cleanup_kind IN ('artifacts')),
    plan_hash BLOB NOT NULL,
    plan_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('planned','running','completed_with_failures','completed')),
    created_at TEXT NOT NULL,
    completed_at TEXT,
    retirement_pending INTEGER NOT NULL DEFAULT 0 CHECK (retirement_pending IN (0,1))
);

CREATE TABLE storage_cleanup_items (
    operation_id TEXT NOT NULL REFERENCES storage_cleanup_operations(operation_id),
    ordinal INTEGER NOT NULL,
    app_id TEXT,
    artifact_kind TEXT NOT NULL CHECK (artifact_kind IN ('release','config_revision','temporary')),
    artifact_id TEXT NOT NULL,
    config_revision_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('planned','detached','failed')),
    error_code TEXT,
    PRIMARY KEY (operation_id, ordinal)
);

CREATE TABLE cleaned_releases (
    app_id TEXT NOT NULL,
    release_id TEXT NOT NULL,
    cleanup_operation_id TEXT NOT NULL REFERENCES storage_cleanup_operations(operation_id),
    removed_at TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    local_image_id TEXT NOT NULL,
    platform_os TEXT NOT NULL,
    platform_architecture TEXT NOT NULL,
    platform_variant TEXT,
    PRIMARY KEY (app_id, release_id)
);

CREATE INDEX storage_cleanup_items_app
ON storage_cleanup_items(app_id, artifact_kind, artifact_id);
