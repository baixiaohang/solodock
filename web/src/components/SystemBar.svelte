<script lang="ts">
  import type { SystemHealth } from '../lib/types'
  import { formatBytes, stateText } from '../lib/presentation'
  import { t } from '../lib/i18n'
  let { health }: { health: SystemHealth } = $props()
</script>

<section class:degraded={health.status === 'degraded'} class="system-bar" aria-label={$t('System health')}>
  <div class="system-fact"><span class="label">Docker</span><strong>{stateText(health.docker.status, $t)}</strong><small>{health.docker.server_version ?? health.docker.error_code ?? $t('Detecting')}</small></div>
  <div class="system-fact"><span class="label">{$t('Recovery scan')}</span><strong>{stateText(health.recovery.status, $t)}</strong><small>{$t('{count} issues', { count: health.recovery.issue_count })}</small></div>
  <div class="system-fact"><span class="label">{$t('Storage cleanup')}</span><strong>{stateText(health.storage_cleanup.status, $t)}</strong><small>{$t('{count} pending operations', { count: health.storage_cleanup.pending_operations })}</small></div>
  <div class="system-fact"><span class="label">{$t('Host memory available')}</span><strong>{formatBytes(health.memory.available_bytes)}</strong><small>{$t('{percent}% used', { percent: health.memory.used_percent?.toFixed(1) ?? '—' })}</small></div>
  <div class="system-fact"><span class="label">{$t('State disk available')}</span><strong>{formatBytes(health.disk.state.available_bytes)}</strong><small>{$t('{percent}% used', { percent: health.disk.state.used_percent?.toFixed(1) ?? '—' })}</small></div>
  <div class="system-fact"><span class="label">{$t('Live connections')}</span><strong>{health.streams.active} / {health.streams.limit}</strong><small>{$t('Bounded SSE')}</small></div>
</section>
