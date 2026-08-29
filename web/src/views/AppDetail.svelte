<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { openSse } from '../lib/sse'
  import { driftText, formatBytes, shortRef } from '../lib/presentation'
  import type { AppObservation, StatsSample } from '../lib/types'
  import LogsPane from '../components/LogsPane.svelte'
  let { appId }: { appId: string } = $props()
  let app = $state<AppObservation | null>(null)
  let stats = $state<StatsSample | null>(null)
  let tab = $state<'overview' | 'logs'>('overview')
  let error = $state('')

  onMount(() => {
    void api<AppObservation>(`/api/v1/apps/${appId}`).then((value) => { app = value }).catch(() => { error = '无法加载应用详情' })
    const source = openSse(`/api/v1/apps/${appId}/stats`, { stats: (event) => { stats = JSON.parse(event.data) as StatsSample } })
    return () => source.close()
  })
</script>

<main class="page-shell">
  <a class="back" href="#/">← 返回观察台</a>
  {#if error}<p class="notice danger">{error}</p>{/if}
  {#if app}
    <div class="detail-heading"><div><p class="eyebrow">APPLICATION</p><h1>{app.display_name}</h1><code>{app.id}</code></div><span class:healthy={app.actual?.health === 'healthy'} class="state-pill large">{app.actual?.status ?? 'unavailable'} · {app.actual?.health ?? 'unknown'}</span></div>
    <div class="tabs"><button class:active={tab === 'overview'} onclick={() => { tab = 'overview' }}>概览</button><button class:active={tab === 'logs'} onclick={() => { tab = 'logs' }}>实时日志</button></div>
    {#if tab === 'logs'}
      <LogsPane {appId} />
    {:else}
      {#if app.drift_codes.length}<div class="notice warning">{#each app.drift_codes as code}<span>{driftText(code)}</span>{/each}</div>{/if}
      <section class="detail-grid">
        <article class="panel"><h2>版本对照</h2><dl class="fact-list"><div><dt>活动镜像</dt><dd><code>{shortRef(app.active_release?.image_ref)}</code></dd></div><div><dt>实际镜像</dt><dd><code>{shortRef(app.actual?.configured_image_ref)}</code></dd></div><div><dt>容器 ID</dt><dd><code>{app.actual?.id.slice(0, 12) ?? '—'}</code></dd></div><div><dt>重启次数</dt><dd>{app.actual?.restart_count ?? '—'}</dd></div><div><dt>退出码</dt><dd>{app.actual?.exit_code ?? '—'}</dd></div></dl></article>
        <article class="panel"><h2>实时资源</h2><dl class="fact-list"><div><dt>CPU</dt><dd>{stats?.cpu_percent?.toFixed(2) ?? '—'}%</dd></div><div><dt>内存</dt><dd>{formatBytes(stats?.memory_usage_bytes ?? null)} / {formatBytes(stats?.memory_limit_bytes ?? null)}</dd></div><div><dt>接收</dt><dd>{formatBytes(stats?.network_rx_bytes ?? null)}</dd></div><div><dt>发送</dt><dd>{formatBytes(stats?.network_tx_bytes ?? null)}</dd></div></dl></article>
        <article class="panel wide"><h2>端口</h2>{#each app.actual?.ports ?? [] as port}<p><code>{port.host_ip}:{port.host_port}</code> → {port.container_port}/{port.protocol}</p>{:else}<p class="muted">无 loopback 端口映射</p>{/each}</article>
        <article class="panel"><h2>挂载</h2>{#each app.actual?.mounts ?? [] as mount}<p><span class="tag">{mount.kind}</span> {mount.destination} · {mount.read_only ? '只读' : '读写'}</p>{:else}<p class="muted">无挂载</p>{/each}</article>
        <article class="panel"><h2>网络</h2>{#each app.actual?.networks ?? [] as network}<p>{network.name} · <code>{network.container_ip ?? '—'}</code></p>{:else}<p class="muted">无网络信息</p>{/each}</article>
      </section>
    {/if}
  {/if}
</main>
