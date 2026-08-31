<script lang="ts">
  import type { SystemHealth } from '../lib/types'
  import { formatBytes } from '../lib/presentation'
  let { health }: { health: SystemHealth } = $props()
</script>

<section class:degraded={health.status === 'degraded'} class="system-bar" aria-label="系统健康">
  <div class="system-fact"><span class="label">Docker</span><strong>{health.docker.status}</strong><small>{health.docker.server_version ?? health.docker.error_code ?? '正在探测'}</small></div>
  <div class="system-fact"><span class="label">恢复扫描</span><strong>{health.recovery.status}</strong><small>{health.recovery.issue_count} 个问题</small></div>
  <div class="system-fact"><span class="label">状态盘可用</span><strong>{formatBytes(health.disk.state.available_bytes)}</strong><small>{health.disk.state.used_percent?.toFixed(1) ?? '—'}% 已用</small></div>
  <div class="system-fact"><span class="label">实时连接</span><strong>{health.streams.active} / {health.streams.limit}</strong><small>有界 SSE</small></div>
</section>
