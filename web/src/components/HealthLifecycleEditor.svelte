<script lang="ts">
  import type { DraftInput, HttpHealthcheck } from '../lib/types'
  let { health = $bindable<DraftInput['health']>({ policy: 'running', stable_window_seconds: 15 }), stopGrace = $bindable(10) }: { health: DraftInput['health']; stopGrace: number } = $props()
  let mode = $derived(health.policy === 'healthy' && health.http ? 'healthy_http' : health.policy)
  const defaultHttp = (): HttpHealthcheck => ({ client: 'curl', scheme: 'http', host: '127.0.0.1', port: 3000, path: '/healthz', interval_seconds: 10, timeout_seconds: 5, retries: 6, start_period_seconds: 30 })
  function change(value: string) {
    if (value === 'running') health = { policy: 'running', stable_window_seconds: 15 }
    else if (value === 'healthy') health = { policy: 'healthy' }
    else if (value === 'healthy_http') health = { policy: 'healthy', http: defaultHttp() }
    else if (value === 'completed') health = { policy: 'completed' }
    else health = { policy: 'disabled', acknowledge_reduced_safety: true }
  }
</script>
<fieldset class="row-editor"><legend>健康与生命周期</legend>
  <div class="editor-row"><label>健康策略<select value={mode} onchange={(event) => change(event.currentTarget.value)}><option value="running">容器持续运行</option><option value="healthy">镜像 HEALTHCHECK</option><option value="healthy_http">HTTP readiness</option><option value="completed">运行完成即成功</option><option value="disabled">禁用（降低安全）</option></select></label>
  <label>停机宽限（秒）<input type="number" min="1" max="600" bind:value={stopGrace} /></label></div>
  {#if health.policy === 'running'}
    <label>稳定窗口（秒）<input type="number" min="1" max="600" bind:value={health.stable_window_seconds} /></label>
  {:else if health.policy === 'healthy' && health.http}
    <div class="editor-row"><label>客户端<select bind:value={health.http.client}><option value="curl">curl</option><option value="wget">wget</option></select></label><label>Host<select bind:value={health.http.host}><option value="127.0.0.1">127.0.0.1</option><option value="localhost">localhost</option><option value="::1">::1</option></select></label><label>端口<input type="number" min="1" max="65535" bind:value={health.http.port} /></label><label>路径<input bind:value={health.http.path} placeholder="/readyz" required /></label></div>
    <div class="editor-row"><label>间隔（秒）<input type="number" min="1" max="600" bind:value={health.http.interval_seconds} /></label><label>超时（秒）<input type="number" min="1" max="600" bind:value={health.http.timeout_seconds} /></label><label>重试次数<input type="number" min="1" max="20" bind:value={health.http.retries} /></label><label>启动宽限（秒）<input type="number" min="0" max="3600" bind:value={health.http.start_period_seconds} /></label></div>
  {/if}
  <p class="muted">宽限是强制结束前的最大等待；服务提前退出会立即继续。</p>
</fieldset>
