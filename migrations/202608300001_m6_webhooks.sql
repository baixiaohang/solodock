CREATE TABLE webhook_replays (
    app_id TEXT NOT NULL,
    secret_revision TEXT NOT NULL,
    nonce_sha256 BLOB NOT NULL,
    received_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    request_id TEXT NOT NULL,
    PRIMARY KEY (app_id, secret_revision, nonce_sha256)
);

CREATE INDEX webhook_replays_expiry ON webhook_replays(expires_at);

ALTER TABLE poll_states ADD COLUMN webhook_sequence INTEGER NOT NULL DEFAULT 0;
ALTER TABLE poll_states ADD COLUMN webhook_processed_sequence INTEGER NOT NULL DEFAULT 0;
ALTER TABLE poll_states ADD COLUMN last_webhook_received_at TEXT;
ALTER TABLE poll_states ADD COLUMN last_wake_source TEXT;
