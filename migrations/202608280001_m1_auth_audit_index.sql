CREATE TABLE admin_credentials (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    username TEXT NOT NULL CHECK (username = 'admin'),
    password_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    token_hash BLOB NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    idle_expires_at TEXT NOT NULL,
    absolute_expires_at TEXT NOT NULL
);

CREATE TABLE auth_throttle (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    window_started_at TEXT,
    failure_count INTEGER NOT NULL DEFAULT 0,
    blocked_until TEXT
);

INSERT INTO auth_throttle (singleton_id, failure_count) VALUES (1, 0);

CREATE TABLE audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor TEXT NOT NULL,
    request_id TEXT NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT,
    target_id TEXT,
    result TEXT NOT NULL,
    redacted_metadata TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE app_index (
    app_id TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    project_name TEXT NOT NULL UNIQUE,
    active_release_id TEXT,
    active_image_ref TEXT,
    source_updated_at TEXT NOT NULL,
    indexed_at TEXT NOT NULL
);
