<script lang="ts">
  import { onMount } from 'svelte'
  import { api } from '../lib/api'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  import { openSse } from '../lib/sse'
  import { driftText, formatBytes, shortRef, stateText } from '../lib/presentation'
  import type { AppsResponse, StatsSample, SystemHealth } from '../lib/types'
  import SystemBar from '../components/SystemBar.svelte'

  let health: SystemHealth | null = null
  let apps: AppsResponse | null = null
  let error: UserMessage | null = null
  let stats: Record<string, StatsSample> = {}
  let sources: EventSource[] = []
  let controller: AbortController | undefined
  let generation = 0
  let disposed = false

  onMount(() => {
    disposed = false
    void load()
    return () => {
      disposed = true
      generation += 1
      controller?.abort()
      controller = undefined
      closeSources(sources)
      sources = []
    }
  })

  function closeSources(current: EventSource[]) {
    current.forEach((source) => source.close())
  }

  function isCurrent(loadGeneration: number): boolean {
    return !disposed && generation === loadGeneration
  }

  async function load() {
    if (disposed) return
    const loadGeneration = ++generation
    controller?.abort()
    const loadController = new AbortController()
    controller = loadController
    try {
      const [loadedHealth, loadedApps] = await Promise.all([
        api<SystemHealth>('/api/v1/system/health', { signal: loadController.signal }),
        api<AppsResponse>('/api/v1/apps', { signal: loadController.signal }),
      ])
      if (!isCurrent(loadGeneration)) return

      closeSources(sources)
      sources = []
      stats = {}
      const nextSources: EventSource[] = []
      try {
        for (const app of loadedApps.apps.filter((candidate) => candidate.actual).slice(0, 8)) {
          const source = openSse(`/api/v1/apps/${app.id}/stats`, {
            stats: (event) => {
              if (isCurrent(loadGeneration)) {
                stats = { ...stats, [app.id]: JSON.parse(event.data) as StatsSample }
              }
            },
          })
          nextSources.push(source)
        }
      } catch (cause) {
        closeSources(nextSources)
        throw cause
      }
      if (!isCurrent(loadGeneration)) {
        closeSources(nextSources)
        return
      }
      health = loadedHealth
      apps = loadedApps
      sources = nextSources
      error = null
    } catch {
      if (isCurrent(loadGeneration)) error = localized('Could not load read-only observation data')
    } finally {
      if (isCurrent(loadGeneration) && controller === loadController) controller = undefined
    }
  }
</script>

<main class="page-shell">
  <div class="page-heading"><div><p class="eyebrow">{$t('OVERVIEW')}</p><h1>{$t('Application console')}</h1><p class="muted">{$t('Manage digest-pinned releases, deployment health gates, and safe lifecycle operations.')}</p></div><div class="actions"><a class="button-link" href="#/apps/new">{$t('Register application')}</a><button class="ghost" onclick={() => void load()}>{$t('Refresh')}</button></div></div>
  {#if error}<p class="notice danger">{messageText(error, $t)}</p>{/if}
  {#if health}<SystemBar {health} />{/if}
  {#if apps && apps.docker_status !== 'ready'}<p class="notice warning">{$t('Docker is unavailable. The recovered application catalog remains visible, and container state will update when the daemon recovers.')}</p>{/if}
  <section class="app-list-panel" aria-label={$t('Application list')}>
    {#if (apps?.apps.length ?? 0) > 0}
      <div class="table-scroll"><table class="app-table">
        <thead><tr><th scope="col">{$t('Application')}</th><th scope="col">{$t('Status')}</th><th scope="col">{$t('CPU')}</th><th scope="col">{$t('Memory')}</th><th scope="col">{$t('Active image')}</th><th scope="col">{$t('Drift')}</th></tr></thead>
        <tbody>{#each apps?.apps ?? [] as app}<tr>
          <td data-label={$t('Application')}><a class="app-name" href={`#/apps/${app.id}`}>{app.display_name}<code>{app.slug}</code></a></td>
          <td data-label={$t('Status')}><span class:healthy={app.actual?.health === 'healthy'} class="state-pill">{stateText(app.actual?.status, $t)}</span></td>
          <td data-label={$t('CPU')} class="metric-value">{stats[app.id]?.cpu_percent?.toFixed(1) ?? '—'}%</td>
          <td data-label={$t('Memory')} class="metric-value">{formatBytes(stats[app.id]?.memory_usage_bytes ?? null)}</td>
          <td data-label={$t('Active image')}><code class="truncate" title={app.active_release?.image_ref ?? ''}>{shortRef(app.active_release?.image_ref)}</code></td>
          <td data-label={$t('Drift')}>{#if app.drift_codes.length}<div class="drifts">{#each app.drift_codes as code}<span title={driftText(code, $t)}>{driftText(code, $t)}</span>{/each}</div>{:else}<span class="aligned">{$t('Expected and actual state match')}</span>{/if}</td>
        </tr>{/each}</tbody>
      </table></div>
    {:else}
      <div class="empty"><h2>{$t('No applications')}</h2><p>{$t('Register an application configuration, then resolve the draft tag to a platform-specific digest and deploy it.')}</p><a class="button-link" href="#/apps/new">{$t('Register the first application')}</a></div>
    {/if}
  </section>
</main>
