<script lang="ts">
  import { issueAt, issuesUnder, type FormIssue } from '../lib/formErrors'
  import { messageText, t } from '../lib/i18n'
  import type { DraftInput, HealthConfigurationLimits, HttpHealthcheck } from '../lib/types'

  let { health = $bindable<DraftInput['health']>({ policy: 'running', stable_window_seconds: 15 }), stopGrace = $bindable(10), limits, issues = [] }: { health: DraftInput['health']; stopGrace: number; limits: HealthConfigurationLimits | null; issues?: FormIssue[] } = $props()
  let mode = $derived(health.policy === 'healthy' && health.http ? 'healthy_http' : health.policy)
  const defaultHttp = (): HttpHealthcheck => ({ client: 'curl', scheme: 'http', host: '127.0.0.1', port: 3000, path: '/healthz', interval_seconds: limits!.http_interval_seconds.default, timeout_seconds: limits!.http_timeout_seconds.default, retries: limits!.http_retries.default, start_period_seconds: limits!.http_start_period_seconds.default })
  function change(value: string) {
    if (!limits) return
    if (value === 'running') health = { policy: 'running', stable_window_seconds: limits.running_stable_window_seconds.default }
    else if (value === 'healthy') health = { policy: 'healthy' }
    else if (value === 'healthy_http') health = { policy: 'healthy', http: defaultHttp() }
    else if (value === 'completed') health = { policy: 'completed' }
    else health = { policy: 'disabled', acknowledge_reduced_safety: true }
  }
</script>
<fieldset class="row-editor" disabled={!limits}><legend>{$t('Health and lifecycle')}</legend>
  {#if !limits}<p class="form-error" role="alert">{$t('Could not load backend configuration limits. Health and lifecycle editing is disabled; refresh and try again.')}</p>{/if}
  <div class="editor-row"><label>{$t('Health policy')}<select data-issue-path="health" value={mode} onchange={(event) => change(event.currentTarget.value)}><option value="running">{$t('Container remains running')}</option><option value="healthy">{$t('Image HEALTHCHECK')}</option><option value="healthy_http">{$t('HTTP readiness')}</option><option value="completed">{$t('Complete successfully when the process exits')}</option><option value="disabled">{$t('Disabled (reduced safety)')}</option></select></label>
  <label>{$t('Stop grace period (seconds)')}<input data-issue-path="stop_grace_period_seconds" type="number" min={limits?.stop_grace_period_seconds.min} max={limits?.stop_grace_period_seconds.max} bind:value={stopGrace} aria-invalid={issueAt(issues, 'stop_grace_period_seconds') ? 'true' : undefined} /></label></div>
  {#if issueAt(issues, 'stop_grace_period_seconds')}<p class="form-error" role="alert">{messageText(issueAt(issues, 'stop_grace_period_seconds')!.message, $t)}</p>{/if}
  {#if health.policy === 'running'}
    <label>{$t('Stable window (seconds)')}<input data-issue-path="health.stable_window_seconds" type="number" min={limits?.running_stable_window_seconds.min} max={limits?.running_stable_window_seconds.max} bind:value={health.stable_window_seconds} aria-invalid={issueAt(issues, 'health.stable_window_seconds') ? 'true' : undefined} /></label>
  {:else if health.policy === 'healthy' && health.http}
    <div class="editor-row"><label>{$t('Client')}<select data-issue-path="health.http.client" bind:value={health.http.client}><option value="curl">curl</option><option value="wget">wget</option></select></label><label>{$t('Host')}<select data-issue-path="health.http.host" bind:value={health.http.host}><option value="127.0.0.1">127.0.0.1</option><option value="localhost">localhost</option><option value="::1">::1</option></select></label><label>{$t('Ports')}<input data-issue-path="health.http.port" type="number" min="1" max="65535" bind:value={health.http.port} aria-invalid={issueAt(issues, 'health.http.port') ? 'true' : undefined} /></label><label>{$t('Path')}<input data-issue-path="health.http.path" bind:value={health.http.path} placeholder="/readyz" aria-invalid={issueAt(issues, 'health.http.path') ? 'true' : undefined} required /></label></div>
    <div class="editor-row"><label>{$t('Interval (seconds)')}<input data-issue-path="health.http.interval_seconds" type="number" min={limits?.http_interval_seconds.min} max={limits?.http_interval_seconds.max} bind:value={health.http.interval_seconds} aria-invalid={issueAt(issues, 'health.http.interval_seconds') ? 'true' : undefined} /></label><label>{$t('Timeout (seconds)')}<input data-issue-path="health.http.timeout_seconds" type="number" min={limits?.http_timeout_seconds.min} max={limits?.http_timeout_seconds.max} bind:value={health.http.timeout_seconds} aria-invalid={issueAt(issues, 'health.http.timeout_seconds') ? 'true' : undefined} /></label><label>{$t('Retries')}<input data-issue-path="health.http.retries" type="number" min={limits?.http_retries.min} max={limits?.http_retries.max} bind:value={health.http.retries} aria-invalid={issueAt(issues, 'health.http.retries') ? 'true' : undefined} /></label><label>{$t('Start period (seconds)')}<input data-issue-path="health.http.start_period_seconds" type="number" min={limits?.http_start_period_seconds.min} max={limits?.http_start_period_seconds.max} bind:value={health.http.start_period_seconds} aria-invalid={issueAt(issues, 'health.http.start_period_seconds') ? 'true' : undefined} /></label></div>
  {/if}
  {#each issuesUnder(issues, 'health') as issue}<p class="form-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
  <p class="muted">{$t('The grace period is the maximum wait before a forced stop; SoloDock proceeds immediately when the service exits earlier.')}</p>
</fieldset>
