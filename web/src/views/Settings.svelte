<script lang="ts">
  import { onMount } from 'svelte'
  import { mutation } from '../lib/api'
  import { retryIdentity, type RetryIdentity } from '../lib/mutationState'
  import { applyTimeSettings, loadTimeSettings, timeSettings } from '../lib/time'
  import type { SettingsResponse } from '../lib/types'

  let settings = $state<SettingsResponse | null>(null)
  let selected = $state('UTC')
  let bindRoots = $state<string[]>([])
  let busy = $state(false)
  let error = $state('')
  let saved = $state(false)
  let retry = $state<RetryIdentity | undefined>()

  onMount(() => { void load() })

  async function load() {
    error = ''
    try {
      settings = await loadTimeSettings()
      selected = settings.display_timezone
      bindRoots = [...(settings.allowed_bind_roots ?? [])]
      retry = undefined
    } catch {
      error = '无法加载全局设置。'
    }
  }

  async function save() {
    if (!settings || !settings.supported_timezones.includes(selected)) return
    busy = true; error = ''; saved = false
    const request = {
      expected_revision: settings.revision,
      display_timezone: selected,
      allowed_bind_roots: bindRoots.filter(Boolean),
    }
    retry = retryIdentity(retry, request)
    try {
      settings = await mutation<SettingsResponse>('/api/v1/settings', request, { method: 'PUT', idempotencyKey: retry.key })
      retry = undefined
      applyTimeSettings(settings)
      selected = settings.display_timezone
      bindRoots = [...(settings.allowed_bind_roots ?? [])]
      saved = true
    } catch {
      error = '保存失败；设置可能已被其他页面修改，请刷新后重试。'
    } finally {
      busy = false
    }
  }

  function region(timezone: string): string {
    return timezone === 'UTC' ? '常用' : timezone.split('/')[0] ?? '其他'
  }
</script>

<main class="page-shell narrow">
  <div class="page-heading"><div><p class="eyebrow">SYSTEM</p><h1>系统设置</h1><p class="muted">只改变 Web 显示；数据库、API、SSE 和下载日志继续使用 UTC。</p></div></div>
  {#if error}<p class="notice danger" role="alert">{error}</p>{/if}
  {#if $timeSettings.warning}<p class="notice warning" role="alert">{$timeSettings.warning}</p>{/if}
  {#if settings}
    <form class="panel settings-form" onsubmit={(event) => { event.preventDefault(); void save() }}>
      <label for="display-timezone">显示时区</label>
      <select id="display-timezone" bind:value={selected} disabled={busy}>
        {#each settings.supported_timezones as timezone}
          <option value={timezone}>{timezone === 'UTC' ? 'UTC（协调世界时）' : `${timezone} · ${region(timezone)}`}</option>
        {/each}
      </select>
      <p class="muted">选项来自后端内置的 IANA tzdb，不能提交任意字符串。保存后所有已打开页面立即按新时区重绘。</p>
      <fieldset class="row-editor"><legend>存储访问</legend><p class="muted">每行一个已存在的安全宿主目录。SoloDock 只把它作为 bind allowlist，不会浏览、创建、改权限或删除目录。</p>{#each bindRoots as root, index}<div class="editor-row"><label>允许根目录<input bind:value={bindRoots[index]} placeholder="/home/ubuntu" required /></label><button type="button" class="ghost" onclick={() => { bindRoots = bindRoots.filter((_, current) => current !== index) }}>删除</button></div>{/each}<button type="button" class="ghost" onclick={() => { bindRoots = [...bindRoots, ''] }}>添加根目录</button></fieldset>
      <div class="actions"><button disabled={busy}>{busy ? '保存中…' : '保存系统设置'}</button>{#if saved}<span class="success-text" role="status">已生效</span>{/if}</div>
    </form>
  {/if}
</main>
