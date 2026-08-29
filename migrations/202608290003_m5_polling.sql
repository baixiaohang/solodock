CREATE TABLE deployment_transitions_m5 AS
SELECT deployment_id, seq, phase, result, code, created_at
FROM deployment_transitions;

CREATE TABLE deployments_m5 (
    id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL,
    trigger TEXT NOT NULL CHECK (trigger IN ('manual', 'rollback', 'poll')),
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
    scheduled_source_image_ref TEXT,
    scheduled_source_descriptor_digest TEXT,
    scheduled_manifest_digest TEXT,
    scheduled_index_digest TEXT,
    scheduled_platform_os TEXT,
    scheduled_platform_architecture TEXT,
    scheduled_platform_variant TEXT,
    scheduled_local_image_id TEXT,
    scheduled_repository TEXT,
    scheduled_target_key TEXT,
    poll_generation TEXT,
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

INSERT INTO deployments_m5 (
    id,app_id,trigger,requested_revision,from_release_id,
    expected_pending_release_id,expected_actual_release_id,expected_actual_container_id,
    predecessor_runtime_release_id,candidate_release_id,rollback_target_release_id,
    rollback_of_deployment_id,status,phase,source_image_ref,source_descriptor_digest,
    manifest_digest,platform,error_class,error_code,health_policy,health_result,
    effect_phase,pre_container_id,pre_started_at,post_container_id,
    rollback_effect_phase,rollback_pre_container_id,rollback_post_container_id,
    request_id,created_at,started_at,completed_at,updated_at
)
SELECT
    id,app_id,trigger,requested_revision,from_release_id,
    expected_pending_release_id,expected_actual_release_id,expected_actual_container_id,
    predecessor_runtime_release_id,candidate_release_id,rollback_target_release_id,
    rollback_of_deployment_id,status,phase,source_image_ref,source_descriptor_digest,
    manifest_digest,platform,error_class,error_code,health_policy,health_result,
    effect_phase,pre_container_id,pre_started_at,post_container_id,
    rollback_effect_phase,rollback_pre_container_id,rollback_post_container_id,
    request_id,created_at,started_at,completed_at,updated_at
FROM deployments;

DROP TABLE deployment_transitions;
DROP TABLE deployments;
ALTER TABLE deployments_m5 RENAME TO deployments;

CREATE UNIQUE INDEX deployments_one_nonterminal_per_app
ON deployments(app_id)
WHERE status IN ('queued', 'running');

CREATE INDEX deployments_app_history
ON deployments(app_id, created_at DESC, id DESC);

CREATE UNIQUE INDEX deployments_poll_target
ON deployments(app_id, scheduled_target_key)
WHERE trigger = 'poll' AND scheduled_target_key IS NOT NULL AND status IN ('queued','running');

CREATE TABLE deployment_transitions (
    deployment_id TEXT NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    phase TEXT NOT NULL,
    result TEXT NOT NULL,
    code TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (deployment_id, seq)
);

INSERT INTO deployment_transitions
SELECT deployment_id, seq, phase, result, code, created_at
FROM deployment_transitions_m5;
DROP TABLE deployment_transitions_m5;

CREATE TABLE poll_states (
    app_id TEXT PRIMARY KEY NOT NULL,
    generation TEXT NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0,1)),
    consecutive_transient_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_transient_failures BETWEEN 0 AND 5),
    next_check_not_before TEXT,
    last_checked_at TEXT,
    last_success_at TEXT,
    last_source_descriptor_digest TEXT,
    last_etag TEXT,
    last_manifest_digest TEXT,
    last_platform TEXT,
    last_outcome TEXT NOT NULL CHECK (last_outcome IN (
        'disabled','scheduled','unchanged','config_pending_manual','busy_skipped',
        'blocked_drift','blocked_attention','suppressed_failed_target',
        'registry_error','credential_error','invalid_source','cancelled'
    )),
    last_error_class TEXT,
    last_error_code TEXT,
    suppressed_target_key TEXT,
    suppressed_deployment_id TEXT,
    updated_at TEXT NOT NULL
);

CREATE INDEX poll_states_due
ON poll_states(enabled, next_check_not_before);
