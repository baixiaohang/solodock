<script lang="ts">
  import { onMount } from 'svelte'
  import { appendDeduplicatedLog, openSse } from '../lib/sse'
  import type { LogEvent } from '../lib/types'
  import { formatTimestamp, timeSettings } from '../lib/time'
  import { locale, t } from '../lib/i18n'
  let { appId }: { appId: string } = $props()
  let lines = $state<Array<LogEvent & { id: string }>>([])
  let status = $state('connecting')
  let autoScroll = $state(true)
  let viewport = $state<HTMLDivElement>()

  onMount(() => {
    const source = openSse(`/api/v1/apps/${appId}/logs?tail=200`, {
      log: (event) => {
        lines = appendDeduplicatedLog(lines, { ...(JSON.parse(event.data) as LogEvent), id: event.lastEventId })
        if (autoScroll) requestAnimationFrame(() => viewport?.scrollTo({ top: viewport.scrollHeight }))
        status = 'live'
      },
      stream_error: () => { status = 'reconnecting' },
    })
    source.onopen = () => { status = 'live' }
    source.onerror = () => { status = 'reconnecting' }
    return () => source.close()
  })

  function download() {
    const blob = new Blob([lines.map((line) => `${line.timestamp} ${line.stream} ${line.message}`).join('\n')], { type: 'text/plain' })
    const link = document.createElement('a')
    link.href = URL.createObjectURL(blob)
    link.download = `solodock-${appId}-logs.txt`
    link.click()
    URL.revokeObjectURL(link.href)
  }
</script>

<section class="logs-panel">
  <header class="logs-toolbar"><span class={`connection ${status}`}>{$t(status === 'live' ? 'Live' : status === 'reconnecting' ? 'Reconnecting' : 'Connecting')}</span><label><input type="checkbox" bind:checked={autoScroll} /> {$t('Auto-scroll')}</label><button class="ghost" onclick={download}>{$t('Download current window')}</button></header>
  <div class="log-viewport" role="log" aria-label={$t('Live application logs')} bind:this={viewport} onscroll={(event) => { const target = event.currentTarget; if (target.scrollTop + target.clientHeight < target.scrollHeight - 30) autoScroll = false }}>
    {#each lines as line (line.id)}
      <div class:stderr={line.stream === 'stderr'} class="log-line"><time datetime={line.timestamp}>{formatTimestamp(line.timestamp, $timeSettings.timezone, $locale)}</time><span class="stream">{line.stream}</span><span>{line.message}</span></div>
    {:else}<p class="log-empty">{$t('Waiting for logs…')}</p>{/each}
  </div>
</section>
