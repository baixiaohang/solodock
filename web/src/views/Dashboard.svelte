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
  <div class="page-heading"><div><p class="eyebrow">OVERVIEW</p><h1>应用观察台</h1><p class="muted">文件系统期望状态与 Docker 实际状态的只读对照。</p></div><button class="ghost" onclick={() => void load()}>刷新</button></div>
  {#if error}<p class="notice danger">{error}</p>{/if}
  {#if health}<SystemBar {health} />{/if}
  {#if apps?.docker_status !== 'ready'}<p class="notice warning">Docker 当前不可用，仍可查看已恢复的应用目录；容器状态将在 daemon 恢复后自动更新。</p>{/if}
  <section class="app-grid" aria-label="应用列表">
    {#each apps?.apps ?? [] as app}
      <a class="app-card" href={`#/apps/${app.id}`}>
        <div class="card-top"><div><h2>{app.display_name}</h2><code>{app.slug}</code></div><span class:healthy={app.actual?.health === 'healthy'} class="state-pill">{app.actual?.status ?? 'unavailable'}</span></div>
        <dl class="metrics"><div><dt>CPU</dt><dd>{stats[app.id]?.cpu_percent?.toFixed(1) ?? '—'}%</dd></div><div><dt>内存</dt><dd>{formatBytes(stats[app.id]?.memory_usage_bytes ?? null)}</dd></div></dl>
        <div class="image-row"><span>活动</span><code>{shortRef(app.active_release?.image_ref)}</code><span>实际</span><code>{shortRef(app.actual?.image_ref)}</code></div>
        {#if app.drift_codes.length}<div class="drifts">{#each app.drift_codes as code}<span title={driftText(code)}>{driftText(code)}</span>{/each}</div>{:else}<p class="aligned">期望与实际一致</p>{/if}
      </a>
    {:else}
      <div class="empty"><h2>尚无应用</h2><p>M2 是只读观察层；应用注册将在后续里程碑提供。</p></div>
    {/each}
  </section>
</main>
