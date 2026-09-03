<script lang="ts">
  import {
    emptyEnvironmentRow,
    emptySecretEnvironmentRow,
    publicEnvironmentEntries,
    replacePublicEnvironmentRows,
    type EnvironmentRow,
  } from '../lib/environmentRows'
  import { EnvironmentTextError, parseEnvironmentText, serializeEnvironmentText } from '../lib/environmentText'
  import { issuesUnder, type FormIssue } from '../lib/formErrors'
  import { localized, messageText, t } from '../lib/i18n'

  let {
    rows = $bindable(),
    issues = [],
    clientIssue = $bindable<FormIssue | null>(null),
    onStructureChange,
  }: { rows: EnvironmentRow[]; issues?: FormIssue[]; clientIssue?: FormIssue | null; onStructureChange?: (path: string) => void } = $props()
  let mode = $state<'rows' | 'text'>('rows')
  let batchText = $state('')

  function remove(row: EnvironmentRow) {
    if (row.originalKey !== null) row.removed = true
    else rows = rows.filter((candidate) => candidate.id !== row.id)
    rows = [...rows]
    onStructureChange?.('environment')
  }

  function update(row: EnvironmentRow, field: 'key' | 'value', value: string) {
    row[field] = value
    clientIssue = null
    rows = [...rows]
  }

  function updateSensitive(row: EnvironmentRow, sensitive: boolean) {
    row.sensitive = sensitive
    clientIssue = null
    rows = [...rows]
    onStructureChange?.('environment')
  }

  function valuePlaceholder(row: EnvironmentRow): string {
    const canKeep = row.storedSecret
      && row.originalSensitive
      && row.sensitive
      && row.originalKey === row.key
    return canKeep ? $t('Saved (leave empty to keep)') : row.originalSensitive ? $t('Enter a new value') : 'VALUE'
  }

  function openTextMode() {
    if (mode === 'text') return
    batchText = serializeEnvironmentText(publicEnvironmentEntries(rows))
    clientIssue = null
    mode = 'text'
  }

  function updateBatch(value: string) {
    batchText = value
    const secretKeys = new Set(rows.filter((row) => !row.removed && row.sensitive).map((row) => row.key.trim()).filter(Boolean))
    try {
      rows = replacePublicEnvironmentRows(rows, parseEnvironmentText(value, secretKeys))
      clientIssue = null
      onStructureChange?.('environment')
    } catch (cause) {
      clientIssue = cause instanceof EnvironmentTextError
        ? cause.issue
        : { path: 'environment.public', code: 'ENV_TEXT_INVALID', message: localized('Invalid bulk environment-variable format') }
    }
  }

  function addPublic() {
    rows = [...rows, emptyEnvironmentRow()]
    onStructureChange?.('environment')
  }

  function addSecret() {
    rows = [...rows, emptySecretEnvironmentRow()]
    onStructureChange?.('environment')
  }

  function openRowsMode() {
    if (mode === 'rows') return
    updateBatch(batchText)
    if (!clientIssue) mode = 'rows'
  }

  function pathForRow(row: EnvironmentRow): string {
    const sameClass = activeRows.filter((candidate) => candidate.sensitive === row.sensitive)
    const index = sameClass.findIndex((candidate) => candidate.id === row.id)
    return row.sensitive ? `environment.secrets[${index}]` : `environment.public[${index}]`
  }

  function issuesForRow(row: EnvironmentRow): FormIssue[] {
    return issuesUnder(issues, pathForRow(row))
  }

  let activeRows = $derived(rows.filter((row) => !row.removed))
  let secretRows = $derived(activeRows.filter((row) => row.sensitive))
  let publicIssues = $derived(issuesUnder(issues, 'environment.public'))
  let sectionIssues = $derived(issues.filter((issue) => !issue.path.includes('[')))
</script>

<fieldset class="environment-editor">
  <legend>{$t('Environment variables')}</legend>
  <div class="mode-switch" aria-label={$t('Public environment variable editing mode')}>
    <button type="button" class:active={mode === 'rows'} onclick={openRowsMode}>{$t('Edit rows')}</button>
    <button type="button" class:active={mode === 'text'} onclick={openTextMode}>{$t('Bulk text')}</button>
  </div>
  {#if mode === 'rows'}
    <p class="muted">{$t('Configure one variable per row. Public values can be edited directly. Stored secrets remain hidden; empty means keep and a new value means replace.')}</p>
    <div class="environment-header" aria-hidden="true"><span>{$t('Name')}</span><span>{$t('Value')}</span><span>{$t('Sensitive')}</span><span>{$t('Actions')}</span></div>
    {#each activeRows as row (row.id)}
      {@const rowIssues = issuesForRow(row)}
      {@const rowPath = pathForRow(row)}
      <div class="environment-row" class:has-error={rowIssues.length > 0}>
        <label><span class="sr-only">{$t('Variable name')}</span><input data-issue-path={`${rowPath}.key`} value={row.key} oninput={(event) => update(row, 'key', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.key')) ? 'true' : undefined} required placeholder="KEY" autocomplete="off" /></label>
        <label><span class="sr-only">{$t('Variable value')}</span><input data-issue-path={`${rowPath}.value`} type={row.sensitive ? 'password' : 'text'} value={row.value} oninput={(event) => update(row, 'value', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => !issue.path.endsWith('.key')) ? 'true' : undefined} placeholder={valuePlaceholder(row)} autocomplete={row.sensitive ? 'new-password' : 'off'} /></label>
        <label class="sensitive-toggle"><input data-issue-path={rowPath} type="checkbox" checked={row.sensitive} onchange={(event) => updateSensitive(row, event.currentTarget.checked)} /> <span>{$t('Sensitive')}</span></label>
        <button data-issue-path={row.sensitive ? 'environment.secrets' : 'environment.public'} type="button" class="ghost compact" onclick={() => remove(row)}>{$t('Delete')}</button>
        {#each rowIssues as issue}<p class="form-error editor-inline-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
      </div>
    {:else}
      <p class="muted">{$t('No environment variables.')}</p>
    {/each}
    <button type="button" class="ghost" onclick={addPublic}>＋ {$t('Add row')}</button>
  {:else}
    <label>{$t('Public environment variables (one KEY=VALUE per line)')}
      <textarea data-issue-path="environment.public" aria-label={$t('Bulk public environment variables')} rows="10" value={batchText} oninput={(event) => updateBatch(event.currentTarget.value)} aria-invalid={clientIssue || publicIssues.length ? 'true' : undefined} spellcheck="false" autocomplete="off"></textarea>
    </label>
    <p class="muted">{$t('Split at the first =. Values may be empty or contain additional = characters. Quotes, comments, variable expansion, and export are not parsed.')}</p>
    {#if clientIssue}<p class="form-error" role="alert">{messageText(clientIssue.message, $t)}</p>{/if}
    {#each publicIssues as issue}<p class="form-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
    <section class="secret-environment">
      <h3>{$t('Secret (write-only)')}</h3>
      <p class="muted">{$t('Secrets are excluded from bulk text. Saved values are never returned, and inputs are cleared immediately after a successful save.')}</p>
      {#each secretRows as row (row.id)}
        {@const rowIssues = issuesForRow(row)}
        {@const rowPath = pathForRow(row)}
        <div class="environment-row secret-row">
          <label>{$t('Variable name')}<input data-issue-path={`${rowPath}.key`} value={row.key} oninput={(event) => update(row, 'key', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.key')) ? 'true' : undefined} required placeholder="KEY" autocomplete="off" /></label>
          <label>{$t('Secret value')}<input data-issue-path={`${rowPath}.value`} type="password" value={row.value} oninput={(event) => update(row, 'value', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => !issue.path.endsWith('.key')) ? 'true' : undefined} placeholder={valuePlaceholder(row)} autocomplete="new-password" /></label>
          <span></span>
          <button data-issue-path="environment.secrets" type="button" class="ghost compact" onclick={() => remove(row)}>{$t('Delete')}</button>
          {#each rowIssues as issue}<p class="form-error editor-inline-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
        </div>
      {:else}<p class="muted">{$t('No secrets.')}</p>{/each}
      <button type="button" class="ghost" onclick={addSecret}>＋ {$t('Add secret')}</button>
    </section>
  {/if}
  {#each sectionIssues as issue}<p class="form-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
</fieldset>
