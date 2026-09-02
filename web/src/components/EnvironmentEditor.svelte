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
    return canKeep ? '已保存（留空保持）' : row.originalSensitive ? '请输入新值' : 'VALUE'
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
        : { path: 'environment.public', code: 'ENV_TEXT_INVALID', message: '批量环境变量格式无效' }
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
  <legend>环境变量</legend>
  <div class="mode-switch" aria-label="普通环境变量编辑模式">
    <button type="button" class:active={mode === 'rows'} onclick={openRowsMode}>逐行编辑</button>
    <button type="button" class:active={mode === 'text'} onclick={openTextMode}>批量文本</button>
  </div>
  {#if mode === 'rows'}
    <p class="muted">每行一条配置。普通值可直接编辑；已保存的 Secret 保持不可见，留空表示 keep，输入新值表示 replace。</p>
    <div class="environment-header" aria-hidden="true"><span>名称</span><span>值</span><span>敏感</span><span>操作</span></div>
    {#each activeRows as row (row.id)}
      {@const rowIssues = issuesForRow(row)}
      {@const rowPath = pathForRow(row)}
      <div class="environment-row" class:has-error={rowIssues.length > 0}>
        <label><span class="sr-only">变量名</span><input data-issue-path={`${rowPath}.key`} value={row.key} oninput={(event) => update(row, 'key', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.key')) ? 'true' : undefined} required placeholder="KEY" autocomplete="off" /></label>
        <label><span class="sr-only">变量值</span><input data-issue-path={`${rowPath}.value`} type={row.sensitive ? 'password' : 'text'} value={row.value} oninput={(event) => update(row, 'value', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => !issue.path.endsWith('.key')) ? 'true' : undefined} placeholder={valuePlaceholder(row)} autocomplete={row.sensitive ? 'new-password' : 'off'} /></label>
        <label class="sensitive-toggle"><input data-issue-path={rowPath} type="checkbox" checked={row.sensitive} onchange={(event) => updateSensitive(row, event.currentTarget.checked)} /> <span>敏感</span></label>
        <button data-issue-path={row.sensitive ? 'environment.secrets' : 'environment.public'} type="button" class="ghost compact" onclick={() => remove(row)}>删除</button>
        {#each rowIssues as issue}<p class="form-error editor-inline-error" role="alert">{issue.message}</p>{/each}
      </div>
    {:else}
      <p class="muted">尚无环境变量。</p>
    {/each}
    <button type="button" class="ghost" onclick={addPublic}>＋ 添加一行</button>
  {:else}
    <label>普通环境变量（一行一个 KEY=VALUE）
      <textarea data-issue-path="environment.public" aria-label="批量普通环境变量" rows="10" value={batchText} oninput={(event) => updateBatch(event.currentTarget.value)} aria-invalid={clientIssue || publicIssues.length ? 'true' : undefined} spellcheck="false" autocomplete="off"></textarea>
    </label>
    <p class="muted">按第一个 <code>=</code> 分隔；value 可为空或继续包含 <code>=</code>。不解析引号、注释、变量替换或 <code>export</code>。</p>
    {#if clientIssue}<p class="form-error" role="alert">{clientIssue.message}</p>{/if}
    {#each publicIssues as issue}<p class="form-error" role="alert">{issue.message}</p>{/each}
    <section class="secret-environment">
      <h3>Secret（write-only）</h3>
      <p class="muted">Secret 不进入批量文本；已保存的值不会回显，保存成功后输入会立即清空。</p>
      {#each secretRows as row (row.id)}
        {@const rowIssues = issuesForRow(row)}
        {@const rowPath = pathForRow(row)}
        <div class="environment-row secret-row">
          <label>变量名<input data-issue-path={`${rowPath}.key`} value={row.key} oninput={(event) => update(row, 'key', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.key')) ? 'true' : undefined} required placeholder="KEY" autocomplete="off" /></label>
          <label>Secret 值<input data-issue-path={`${rowPath}.value`} type="password" value={row.value} oninput={(event) => update(row, 'value', event.currentTarget.value)} aria-invalid={rowIssues.some((issue) => !issue.path.endsWith('.key')) ? 'true' : undefined} placeholder={valuePlaceholder(row)} autocomplete="new-password" /></label>
          <span></span>
          <button data-issue-path="environment.secrets" type="button" class="ghost compact" onclick={() => remove(row)}>删除</button>
          {#each rowIssues as issue}<p class="form-error editor-inline-error" role="alert">{issue.message}</p>{/each}
        </div>
      {:else}<p class="muted">尚无 Secret。</p>{/each}
      <button type="button" class="ghost" onclick={addSecret}>＋ 添加 Secret</button>
    </section>
  {/if}
  {#each sectionIssues as issue}<p class="form-error" role="alert">{issue.message}</p>{/each}
</fieldset>
