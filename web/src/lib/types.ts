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
