CREATE TABLE global_settings (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    revision TEXT NOT NULL,
    display_timezone TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

INSERT INTO global_settings (singleton_id, revision, display_timezone, updated_at)
VALUES (1, '00000000-0000-0000-0000-000000000001', 'UTC', '1970-01-01T00:00:00Z');
