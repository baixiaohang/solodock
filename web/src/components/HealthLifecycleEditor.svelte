<script lang="ts">
  import { issueAt, issuesUnder, type FormIssue } from '../lib/formErrors'
  import type { DraftInput, HealthConfigurationLimits, HttpHealthcheck } from '../lib/types'

  let {
    health = $bindable<DraftInput['health']>({ policy: 'running', stable_window_seconds: 15 }),
    stopGrace = $bindable(10),
    limits,
    issues = [],
  }: { health: DraftInput['health']; stopGrace: number; limits: HealthConfigurationLimits | null; issues?: FormIssue[] } = $props()
  let mode = $derived(health.policy === 'healthy' && health.http ? 'healthy_http' : health.policy)
  const defaultHttp = (): HttpHealthcheck => ({
    client: 'curl', scheme: 'http', host: '127.0.0.1', port: 3000, path: '/healthz',
    interval_seconds: limits!.http_interval_seconds.default,
    timeout_seconds: limits!.http_timeout_seconds.default,
    retries: limits!.http_retries.default,
    start_period_seconds: limits!.http_start_period_seconds.default,
  })
  function change(value: string) {
    if (!limits) return
    if (value === 'running') health = { policy: 'running', stable_window_seconds: limits.running_stable_window_seconds.default }
    else if (value === 'healthy') health = { policy: 'healthy' }
    else if (value === 'healthy_http') health = { policy: 'healthy', http: defaultHttp() }
    else if (value === 'completed') health = { policy: 'completed' }
    else health = { policy: 'disabled', acknowledge_reduced_safety: true }
  }
</script>
<fieldset class="row-editor" disabled={!limits}><legend>健康与生命周期</legend>
  {#if !limits}<p class="form-error" role="alert">无法获取后端配置限制，已禁用健康与生命周期编辑；请刷新后重试。</p>{/if}
  <div class="editor-row"><label>健康策略<select data-issue-path="health" value={mode} onchange={(event) => change(event.currentTarget.value)}><option value="running">容器持续运行</option><option value="healthy">镜像 HEALTHCHECK</option><option value="healthy_http">HTTP readiness</option><option value="completed">运行完成即成功</option><option value="disabled">禁用（降低安全）</option></select></label>
  <label>停机宽限（秒）<input data-issue-path="stop_grace_period_seconds" type="number" min={limits?.stop_grace_period_seconds.min} max={limits?.stop_grace_period_seconds.max} bind:value={stopGrace} aria-invalid={issueAt(issues, 'stop_grace_period_seconds') ? 'true' : undefined} /></label></div>
  {#if issueAt(issues, 'stop_grace_period_seconds')}<p class="form-error" role="alert">{issueAt(issues, 'stop_grace_period_seconds')?.message}</p>{/if}
  {#if health.policy === 'running'}
    <label>稳定窗口（秒）<input data-issue-path="health.stable_window_seconds" type="number" min={limits?.running_stable_window_seconds.min} max={limits?.running_stable_window_seconds.max} bind:value={health.stable_window_seconds} aria-invalid={issueAt(issues, 'health.stable_window_seconds') ? 'true' : undefined} /></label>
  {:else if health.policy === 'healthy' && health.http}
    <div class="editor-row"><label>客户端<select data-issue-path="health.http.client" bind:value={health.http.client}><option value="curl">curl</option><option value="wget">wget</option></select></label><label>Host<select data-issue-path="health.http.host" bind:value={health.http.host}><option value="127.0.0.1">127.0.0.1</option><option value="localhost">localhost</option><option value="::1">::1</option></select></label><label>端口<input data-issue-path="health.http.port" type="number" min="1" max="65535" bind:value={health.http.port} aria-invalid={issueAt(issues, 'health.http.port') ? 'true' : undefined} /></label><label>路径<input data-issue-path="health.http.path" bind:value={health.http.path} placeholder="/readyz" aria-invalid={issueAt(issues, 'health.http.path') ? 'true' : undefined} required /></label></div>
    <div class="editor-row"><label>间隔（秒）<input data-issue-path="health.http.interval_seconds" type="number" min={limits?.http_interval_seconds.min} max={limits?.http_interval_seconds.max} bind:value={health.http.interval_seconds} aria-invalid={issueAt(issues, 'health.http.interval_seconds') ? 'true' : undefined} /></label><label>超时（秒）<input data-issue-path="health.http.timeout_seconds" type="number" min={limits?.http_timeout_seconds.min} max={limits?.http_timeout_seconds.max} bind:value={health.http.timeout_seconds} aria-invalid={issueAt(issues, 'health.http.timeout_seconds') ? 'true' : undefined} /></label><label>重试次数<input data-issue-path="health.http.retries" type="number" min={limits?.http_retries.min} max={limits?.http_retries.max} bind:value={health.http.retries} aria-invalid={issueAt(issues, 'health.http.retries') ? 'true' : undefined} /></label><label>启动宽限（秒）<input data-issue-path="health.http.start_period_seconds" type="number" min={limits?.http_start_period_seconds.min} max={limits?.http_start_period_seconds.max} bind:value={health.http.start_period_seconds} aria-invalid={issueAt(issues, 'health.http.start_period_seconds') ? 'true' : undefined} /></label></div>
  {/if}
  {#each issuesUnder(issues, 'health') as issue}<p class="form-error" role="alert">{issue.message}</p>{/each}
  <p class="muted">宽限是强制结束前的最大等待；服务提前退出会立即继续。</p>
</fieldset>
