export interface ApiErrorBody {
  code: string
  message: string
  request_id: string
}

export interface MeResponse {
  username: string
  session: { created_at: string; expires_at: string }
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
  disk: { state: DiskSnapshot; docker: DiskSnapshot | null }
  streams: { active: number; limit: number }
  projection: { status: 'ok' | 'degraded' }
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
  networks: Array<{ name: string; container_ip: string | null }>
}

export interface AppObservation {
  id: string
  slug: string
  display_name: string
  active_release: ActiveRelease | null
  actual: ContainerProjection | null
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
  slug: string
  display_name: string
  discovery_image_ref: string
  credential_ref: null
  auto_deploy_enabled: false
  poll_interval_seconds: number
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
  networks: Array<{ kind: 'owned_default' } | { kind: 'external'; name: string }>
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
  remove_container: boolean
  container_ids: string[]
  managed_files: Array<{ logical_name: string; configured_in: 'active' | 'draft' | 'active_and_draft' }>
  retained: {
    containers: string[]
    owned_volumes: Array<{ name: string; configured_in: 'active' | 'draft' | 'active_and_draft'; exists: boolean }>
    external_volumes: Array<{ name: string; configured_in: 'active' | 'draft' | 'active_and_draft'; exists: boolean }>
    binds: Array<{ source: string; readonly: boolean; configured_in: 'active' | 'draft' | 'active_and_draft'; exists: boolean }>
    networks: Array<{ name: string; configured_in: 'active' | 'draft' | 'active_and_draft'; exists: boolean }>
  }
  orphan_warning: boolean
  confirmation_token: string
  expires_at: string
}

export interface DraftResponse {
  discovery_image_ref: string
  poll_interval_seconds: number
  public_environment: Array<{ key: string; value: string }>
  secret_keys: string[]
  files: Array<{ logical_name: string; target_path: string; sensitive: boolean; readonly: boolean; content?: string }>
  ports: DraftInput['ports']
  volumes: DraftInput['volumes']
  binds: DraftInput['binds']
  networks: DraftInput['networks']
  health: DraftInput['health']
}

export interface AppDetailResponse extends AppObservation {
  draft: DraftResponse | null
  draft_revision: string | null
  draft_config_sha256: string | null
  active_config_revision: string | null
  desired_state: 'running' | 'stopped'
  deployment_status: 'ACTIVE' | 'DEPLOY_REQUIRED'
  available_actions: Array<'start' | 'stop' | 'restart' | 'deletion_preview'>
  compose_available: boolean
}

export interface AppMutationResponse {
  app: { id: string; slug: string; display_name: string; config_revision: string; deployment_status: string; warnings: string[] }
  idempotency_replayed: boolean
  projection_warning?: string
}

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
