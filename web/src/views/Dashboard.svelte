<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { openSse } from '../lib/sse'
  import { driftText, formatBytes, shortRef } from '../lib/presentation'
  import type { AppsResponse, StatsSample, SystemHealth } from '../lib/types'
  import SystemBar from '../components/SystemBar.svelte'

  let health: SystemHealth | null = null
  let apps: AppsResponse | null = null
  let error = ''
  let stats: Record<string, StatsSample> = {}
  let sources: EventSource[] = []

  onMount(() => {
    void load()
    return () => sources.forEach((source) => source.close())
  })

  async function load() {
    try {
      ;[health, apps] = await Promise.all([api<SystemHealth>('/api/v1/system/health'), api<AppsResponse>('/api/v1/apps')])
      sources.forEach((source) => source.close())
      sources = (apps?.apps ?? []).filter((app) => app.actual).slice(0, 8).map((app) => openSse(`/api/v1/apps/${app.id}/stats`, {
        stats: (event) => { stats = { ...stats, [app.id]: JSON.parse(event.data) as StatsSample } },
      }))
    } catch { error = '无法加载只读观察数据' }
  }
</script>

<main class="page-shell">
  <div class="page-heading"><div><p class="eyebrow">OVERVIEW</p><h1>应用控制台</h1><p class="muted">管理 digest-pinned release、部署健康门禁与安全生命周期操作。</p></div><div class="actions"><a class="button-link" href="#/apps/new">注册应用</a><button class="ghost" onclick={() => void load()}>刷新</button></div></div>
  {#if error}<p class="notice danger">{error}</p>{/if}
  {#if health}<SystemBar {health} />{/if}
  {#if apps && apps.docker_status !== 'ready'}<p class="notice warning">Docker 当前不可用，仍可查看已恢复的应用目录；容器状态将在 daemon 恢复后自动更新。</p>{/if}
  <section class="app-list-panel" aria-label="应用列表">
    {#if (apps?.apps.length ?? 0) > 0}
      <div class="table-scroll">
        <table class="app-table">
          <thead><tr><th scope="col">应用</th><th scope="col">状态</th><th scope="col">CPU</th><th scope="col">内存</th><th scope="col">活动镜像</th><th scope="col">Drift</th></tr></thead>
          <tbody>
            {#each apps?.apps ?? [] as app}
              <tr>
                <td data-label="应用"><a class="app-name" href={`#/apps/${app.id}`}>{app.display_name}<code>{app.slug}</code></a></td>
                <td data-label="状态"><span class:healthy={app.actual?.health === 'healthy'} class="state-pill">{app.actual?.status ?? 'unavailable'}</span></td>
                <td data-label="CPU" class="metric-value">{stats[app.id]?.cpu_percent?.toFixed(1) ?? '—'}%</td>
                <td data-label="内存" class="metric-value">{formatBytes(stats[app.id]?.memory_usage_bytes ?? null)}</td>
                <td data-label="活动镜像"><code class="truncate" title={app.active_release?.image_ref ?? ''}>{shortRef(app.active_release?.image_ref)}</code></td>
                <td data-label="Drift">{#if app.drift_codes.length}<div class="drifts">{#each app.drift_codes as code}<span title={driftText(code)}>{driftText(code)}</span>{/each}</div>{:else}<span class="aligned">期望与实际一致</span>{/if}</td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {:else}
      <div class="empty"><h2>尚无应用</h2><p>先注册应用配置，再从 draft tag 解析具体平台 digest 并部署。</p><a class="button-link" href="#/apps/new">注册第一个应用</a></div>
    {/if}
  </section>
</main>
