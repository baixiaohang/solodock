const driftMessages: Record<string, string> = {
  DOCKER_UNAVAILABLE: 'Docker 暂时不可用',
  CONTAINER_MISSING: '容器缺失',
  CONTAINER_AMBIGUOUS: '发现多个候选容器',
  LABEL_INVALID: '容器归属标签无效',
  ACTIVE_RELEASE_MISSING: '缺少活动版本',
  RELEASE_ID_MISMATCH: '运行版本与活动版本不一致',
  IMAGE_REF_MISMATCH: '运行镜像与活动镜像不一致',
  NETWORK_ATTACHMENT_MISMATCH: '实际网络 attachment 与 immutable release 期望不一致',
  NETWORK_ALIAS_MISMATCH: '实际网络缺少期望 alias',
}

export function driftText(code: string): string {
  return driftMessages[code] ?? '检测到未知漂移'
}

export function shortRef(value: string | null | undefined): string {
  if (!value) return '—'
  const digest = value.split('@sha256:')[1]
  return digest ? `sha256:${digest.slice(0, 12)}` : value.slice(0, 20)
}

export function formatBytes(value: number | null): string {
  if (value === null) return '—'
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
  let amount = value
  let index = 0
  while (amount >= 1024 && index < units.length - 1) { amount /= 1024; index += 1 }
  return `${amount.toFixed(index === 0 ? 0 : 1)} ${units[index]}`
}
