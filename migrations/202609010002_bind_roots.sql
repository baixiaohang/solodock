ALTER TABLE global_settings ADD COLUMN allowed_bind_roots_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE global_settings ADD COLUMN bind_roots_initialized INTEGER NOT NULL DEFAULT 0 CHECK (bind_roots_initialized IN (0, 1));
