export interface ApiErrorBody {
  code: string
  message: string
  request_id: string
  issues?: Array<{ path: string; code: string; message: string }>
}

export interface MeResponse {
  username: string
  session: { created_at: string; expires_at: string }
}

export interface InstallationIdentity {
  channel: 'stable' | 'main' | 'development' | 'unknown'
  version: string | null
  source_sha: string | null
  package_identity: string | null
}

export interface SettingsResponse {
  revision: string
  display_timezone: string
  supported_timezones: string[]
  allowed_bind_roots: string[]
  slug_max_length: number
  supported_mount_types: Array<'owned_volume' | 'external_volume' | 'bind'>
  configuration_limits: { health: HealthConfigurationLimits }
  idempotency_replayed?: boolean
}

export type CleanupProtectionReason = 'active' | 'pending' | 'current_draft' | 'recent_rollback' | 'deployment_recovery' | 'cleanup_in_progress'

export interface CleanupCandidate {
  artifact_kind: 'release' | 'config_revision' | 'temporary'
  artifact_id: string
  app_id?: string
  estimated_logical_bytes: number
  release_created_at: string | null
}

export interface CleanupPreview {
  candidates: CleanupCandidate[]
  protected: Array<{ reason: CleanupProtectionReason; count: number }>
  estimated_logical_bytes: number
  confirmation_token: string
  expires_at: string
}

export interface CleanupApplyResult {
  operation_id: string
  plan_hash: string
  status: 'completed' | 'completed_with_failures'
  items: Array<{ app_id?: string; artifact_kind: string; artifact_id: string; status: 'deleted' | 'retained'; error_code?: string }>
  idempotency_replayed: boolean
}

export interface NumericLimit { min: number; max: number; default: number }
export interface HealthConfigurationLimits {
  running_stable_window_seconds: NumericLimit
  http_interval_seconds: NumericLimit
  http_timeout_seconds: NumericLimit
  http_retries: NumericLimit
  http_start_period_seconds: NumericLimit
  stop_grace_period_seconds: NumericLimit
}

export type DockerStatus = 'starting' | 'ready' | 'unavailable' | 'permission_denied' | 'incompatible'

export interface DiskSnapshot {
  status: 'normal' | 'warning' | 'critical' | 'unknown'
  total_bytes: number | null
  available_bytes: number | null
  used_percent: number | null
}

export interface SystemHealth {
  status: 'ok' | 'degraded'
  docker: {
    status: DockerStatus
    error_code: string | null
    server_version: string | null
    api_version: string | null
    os: string | null
    architecture: string | null
    observed_at: string
  }
  recovery: { status: 'ok' | 'degraded'; issue_count: number; issues_by_code: Record<string, number> }
  memory: { total_bytes: number | null; available_bytes: number | null; used_percent: number | null }
  disk: { state: DiskSnapshot; docker: DiskSnapshot | null }
  streams: { active: number; limit: number }
  projection: { status: 'ok' | 'degraded' }
  storage_cleanup: { status: 'ok' | 'pending' | 'degraded' | 'unavailable'; pending_operations: number }
  deployments: { active: number; interrupted: number; needs_attention: number; limit: number }
  registry_credentials: { status: 'ok' | 'degraded' | 'unavailable'; count: number }
  polling: { coordinator: { status: 'running' | 'degraded' | 'stopped'; due: number; inflight: number }; store_status: 'ok' | 'degraded'; enabled: number; suppressed: number; app_errors: number }
  webhooks: { status: 'ok' | 'degraded' | 'disabled'; configured: number; replay_records: number }
}

export interface ActiveRelease { id: string; image_ref: string }

export interface ContainerProjection {
  id: string
  name: string
  status: string
  health: string
  exit_code: number | null
  restart_count: number | null
  started_at: string | null
  finished_at: string | null
  configured_image_ref: string | null
  image_id: string | null
  ports: Array<{ container_port: number; protocol: string; host_ip: string; host_port: number }>
  mounts: Array<{ kind: string; source: string | null; destination: string; read_only: boolean }>
  networks: Array<{ name: string; container_ip: string | null; aliases: string[] }>
}

export type NetworkMode = 'owned_only' | 'owned_and_external' | 'external_only' | 'owned_and_platform' | 'owned_platform_and_external' | 'platform_and_external' | 'platform_only'
export interface ExternalNetworkAttachment { name: string; aliases: string[] }
export interface NetworkPlan {
  owned_default_network: boolean
  service_discovery_enabled: boolean
  mode: NetworkMode
  external: ExternalNetworkAttachment[]
}
export interface ComposePlan {
  service: 'app'
  image_ref: string
  runnable: boolean
  stop_grace_period_seconds: number
  ports: number
  mounts: number
  networks: number
  network_mode: NetworkMode
  owned_default_network: { docker_name: string; bridge_name: string } | null
  external_networks: ExternalNetworkAttachment[]
  warnings: string[]
}

export interface AppObservation {
  id: string
  slug: string
  display_name: string
  active_release: ActiveRelease | null
  actual_release_id: string | null
  actual: ContainerProjection | null
  expected_network_plan: NetworkPlan | null
  expected_owned_default_network: { docker_name: string; bridge_name: string } | null
  actual_owned_default_network: { docker_name: string; driver: string | null; bridge_name: string | null } | null
  drift_codes: string[]
}

export type SecretOperation =
  | { operation: 'keep' }
  | { operation: 'replace'; value: string }
  | { operation: 'delete' }

export interface HttpHealthcheck {
  client: 'curl' | 'wget'
  scheme: 'http'
  host: '127.0.0.1' | 'localhost' | '::1'
  port: number
  path: string
  interval_seconds: number
  timeout_seconds: number
  retries: number
  start_period_seconds: number
}

export interface DraftInput {
  security_profile?: string | null
  display_name: string
  discovery_image_ref: string
  credential_ref: string | null
  auto_deploy_enabled: boolean
  auto_deploy_acknowledged: boolean
  poll_interval_seconds: number
  stop_grace_period_seconds: number
  environment: {
    public: Array<{ key: string; value: string }>
    secrets: Array<{ key: string } & SecretOperation>
  }
  files: Array<({
    logical_name: string
    target_path: string
    sensitive: false
    readonly: true
    content: string
  } | ({
    logical_name: string
    target_path: string
    sensitive: true
    readonly: true
  } & SecretOperation))>
  ports: Array<{ host_ip: '127.0.0.1' | '::1'; host_port: number; container_port: number; protocol: 'tcp' | 'udp' }>
  volumes: Array<
    | { kind: 'owned'; logical_name: string; target_path: string }
    | { kind: 'external'; name: string; target_path: string }
  >
  binds: Array<{ source: string; target_path: string; readonly: boolean; acknowledge_non_rollbackable: boolean }>
  owned_default_network: boolean
  service_discovery_enabled: boolean
  networks: Array<{ kind: 'owned_default' } | { kind: 'external'; name: string; aliases?: string[] }>
  health:
    | { policy: 'running'; stable_window_seconds: number }
    | { policy: 'completed' }
    | { policy: 'disabled'; acknowledge_reduced_safety: true }
    | { policy: 'healthy'; http?: HttpHealthcheck }
}

export interface DeletionPreviewResponse {
  app_id: string
  slug: string
  expected_revision: string
  project_name: string
  active_release_id: string | null
  active_config_revision: string | null
  pending_release_id: string | null
  pending_config_revision: string | null
  remove_container: boolean
  container_ids: string[]
  managed_files: Array<{ logical_name: string; configured_in: ConfiguredScope }>
  retained: {
    containers: string[]
    owned_volumes: Array<{ name: string; configured_in: ConfiguredScope; exists: boolean }>
    external_volumes: Array<{ name: string; configured_in: ConfiguredScope; exists: boolean }>
    binds: Array<{ source: string; readonly: boolean; configured_in: ConfiguredScope; exists: boolean }>
    networks: Array<{ name: string; bridge_name: string | null; kind: 'owned_default' | 'external'; aliases: string[]; configured_in: ConfiguredScope; exists: boolean }>
  }
  orphan_warning: boolean
  webhook_configured: boolean
  confirmation_token: string
  expires_at: string
}

export interface WebhookStatus {
  configured: boolean
  degraded: boolean
  metadata_revision: string | null
  secret_revision: string | null
  algorithm: 'hmac-sha256-v1'
  public_origin: string
  public_path: string
  created_at: string | null
  rotated_at: string | null
}

export type ConfiguredScope = 'active' | 'pending' | 'draft' | 'active_and_pending' | 'active_and_draft' | 'pending_and_draft' | 'active_pending_and_draft'

export interface DraftResponse {
  security_profile?: string | null
  discovery_image_ref: string
  credential_ref: string | null
  auto_deploy_enabled: boolean
  poll_interval_seconds: number
  stop_grace_period_seconds: number
  public_environment: Array<{ key: string; value: string }>
  secret_keys: string[]
  files: Array<{ logical_name: string; target_path: string; sensitive: boolean; readonly: boolean; content?: string }>
  ports: DraftInput['ports']
  volumes: DraftInput['volumes']
  binds: DraftInput['binds']
  owned_default_network: boolean
  service_discovery_enabled: boolean
  networks: DraftInput['networks']
  health: DraftInput['health']
}

export interface AppDetailResponse extends AppObservation {
  resource_names: { project_name: string; owned_default_network_name: string; bridge_name: string }
  draft: DraftResponse | null
  draft_revision: string | null
  draft_config_sha256: string | null
  active_config_revision: string | null
  pending_release_id: string | null
  pending_image_ref: string | null
  desired_state: 'running' | 'stopped'
  deployment_status: 'ACTIVE' | 'PENDING' | 'RUNNING' | 'DEPLOY_REQUIRED' | 'UNCONFIGURED'
  available_actions: Array<'start' | 'stop' | 'restart' | 'deploy' | 'deletion_preview'>
  compose_available: boolean
  polling: PollState | null
}

export interface PollState {
  app_id: string
  generation: string
  enabled: boolean
  consecutive_transient_failures: number
  next_check_not_before: string | null
  last_checked_at: string | null
  last_success_at: string | null
  last_source_descriptor_digest: string | null
  last_manifest_digest: string | null
  last_platform: string | null
  last_outcome: 'disabled' | 'scheduled' | 'unchanged' | 'config_pending_manual' | 'busy_skipped' | 'blocked_drift' | 'blocked_attention' | 'suppressed_failed_target' | 'registry_error' | 'credential_error' | 'invalid_source' | 'cancelled'
  last_error_class: string | null
  last_error_code: string | null
  suppressed_target_key: string | null
  suppressed_deployment_id: string | null
  updated_at: string
}

export interface AppMutationResponse {
  app: { id: string; slug: string; display_name: string; config_revision: string | null; stop_grace_period_seconds: number | null; deployment_status: string; warnings: string[] }
  idempotency_replayed: boolean
  projection_warning?: string
}

export interface ImageConfigSuggestion { resolved_digest: string; exposed_ports: Array<{ container_port: number; protocol: 'tcp' | 'udp' }>; volume_targets: string[]; has_healthcheck: boolean; user: string | null; stop_signal: string | null; warnings: string[] }
export interface AppPresetDescriptor { id: string; schema_version: number; display_name: string; description: string; default_major: string; supported_majors: string[]; default_username: string; default_database: string; password_generated_by_client: boolean }

export interface AppListItem {
  id: string
  slug: string
  display_name: string
  active_release: ActiveRelease | null
  actual: { status: string; health: string; container_id: string; image_ref: string | null } | null
  drift_codes: string[]
}

export interface AppsResponse { docker_status: DockerStatus; observed_at: string; apps: AppListItem[] }

export interface StatsSample {
  observed_at: string
  cpu_percent: number | null
  memory_usage_bytes: number | null
  memory_limit_bytes: number | null
  memory_percent: number | null
  network_rx_bytes: number | null
  network_tx_bytes: number | null
}

export interface LogEvent { timestamp: string; stream: 'stdout' | 'stderr'; message: string; truncated: boolean }

export interface RegistryCredential {
  id: string
  registry: string
  username: string
  revision: string
  created_at: string
  rotated_at: string
  referenced_by_apps: number
}

export type DeploymentStatus = 'queued' | 'running' | 'succeeded' | 'no_op' | 'failed' | 'rolled_back' | 'needs_attention' | 'interrupted'
export interface Deployment {
  id: string
  app_id: string
  trigger: 'manual' | 'rollback' | 'poll'
  requested_revision: string
  from_release_id: string | null
  candidate_release_id: string | null
  rollback_target_release_id: string | null
  status: DeploymentStatus
  phase: string
  source_image_ref: string | null
  manifest_digest: string | null
  platform: string | null
  error_code: string | null
  health_result: string | null
  created_at: string
  started_at: string | null
  completed_at: string | null
  transitions?: Array<{ seq: number; phase: string; result: string; code: string | null; created_at: string }>
  available_actions: Array<'rollback'>
  safe_release_id?: string | null
  candidate_stop_grace_period_seconds?: number | null
  current_active_release_id?: string | null
  current_pending_release_id?: string | null
  current_actual_release_id?: string | null
  warnings?: string[]
}
export interface DeploymentPage { items: Deployment[]; next_cursor: string | null }
