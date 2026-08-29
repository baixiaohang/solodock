CREATE TABLE deployments (
    id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL,
    trigger TEXT NOT NULL CHECK (trigger IN ('manual', 'rollback')),
    requested_revision TEXT NOT NULL,
    from_release_id TEXT,
    expected_pending_release_id TEXT,
    expected_actual_release_id TEXT,
    expected_actual_container_id TEXT,
    predecessor_runtime_release_id TEXT,
    candidate_release_id TEXT,
    rollback_target_release_id TEXT,
    rollback_of_deployment_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('queued','running','succeeded','no_op','failed','rolled_back','needs_attention','interrupted')),
    phase TEXT NOT NULL CHECK (phase IN ('queued','resolving','preparing','pulling','applying','verifying','committing','rolling_back','verifying_rollback','terminal')),
    source_image_ref TEXT,
    source_descriptor_digest TEXT,
    manifest_digest TEXT,
    platform TEXT,
    error_class TEXT,
    error_code TEXT,
    health_policy TEXT,
    health_result TEXT,
    effect_phase TEXT,
    pre_container_id TEXT,
    pre_started_at TEXT,
    post_container_id TEXT,
    rollback_effect_phase TEXT,
    rollback_pre_container_id TEXT,
    rollback_post_container_id TEXT,
    request_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT,
    updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX deployments_one_nonterminal_per_app
ON deployments(app_id)
WHERE status IN ('queued', 'running');

CREATE INDEX deployments_app_history
ON deployments(app_id, created_at DESC, id DESC);

CREATE TABLE deployment_transitions (
    deployment_id TEXT NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    phase TEXT NOT NULL,
    result TEXT NOT NULL,
    code TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (deployment_id, seq)
);
