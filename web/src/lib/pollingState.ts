import type { PollState } from './types'

const labels: Record<PollState['last_outcome'], string> = {
  disabled: '已禁用',
  scheduled: '已调度新 digest',
  unchanged: 'digest 未变化',
  config_pending_manual: '仅配置变化，等待手动部署',
  busy_skipped: '应用忙，本轮已合并',
  blocked_drift: '运行态漂移，已阻止',
  blocked_attention: '需要管理员处理',
  suppressed_failed_target: '失败 digest 已抑制',
  registry_error: 'Registry 检查失败',
  credential_error: 'Registry credential 失败',
  invalid_source: '镜像引用无效',
  cancelled: '检查已取消',
}

export function pollOutcomeText(state: PollState | null): string {
  return state ? labels[state.last_outcome] : '尚未检查'
}

export function pollNeedsAttention(state: PollState | null): boolean {
  return state !== null && [
    'blocked_drift',
    'blocked_attention',
    'suppressed_failed_target',
    'registry_error',
    'credential_error',
    'invalid_source',
  ].includes(state.last_outcome)
}
