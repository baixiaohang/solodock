<script lang="ts">
  import { issuesUnder, type FormIssue } from '../lib/formErrors'
  import { emptyManagedFileRow, type ManagedFileRow } from '../lib/managedFileRows'
  let { rows = $bindable<ManagedFileRow[]>([]), issues = [], onStructureChange }: { rows: ManagedFileRow[]; issues?: FormIssue[]; onStructureChange?: (path: string) => void } = $props()
  function add() { rows = [...rows, emptyManagedFileRow()]; onStructureChange?.('files') }
  function remove(index: number) { rows = rows.flatMap((row, current) => current !== index ? [row] : row.originalLogicalName !== null ? [{ ...row, removed: true }] : []); onStructureChange?.('files') }
  function setSensitive(row: ManagedFileRow, sensitive: boolean) { row.sensitive = sensitive; rows = [...rows]; onStructureChange?.('files') }
  function placeholder(row: ManagedFileRow): string {
    const canKeep = row.storedSecret && row.originalSensitive && row.sensitive && row.originalLogicalName === row.logicalName
    return canKeep ? '已保存（留空保持）' : row.originalSensitive ? '请输入新内容' : ''
  }
  let activeRows = $derived(rows.filter((row) => !row.removed))
</script>
<fieldset class="row-editor"><legend>托管文件</legend>
  {#each activeRows as row}{@const index = rows.indexOf(row)}{@const activeIndex = activeRows.indexOf(row)}{@const rowIssues = issuesUnder(issues, `files[${activeIndex}]`)}<div class="editor-row file-row"><label>逻辑名称<input data-issue-path={`files[${activeIndex}].logical_name`} bind:value={row.logicalName} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.logical_name')) ? 'true' : undefined} required /></label><label>容器路径<input data-issue-path={`files[${activeIndex}].target_path`} bind:value={row.targetPath} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.target_path')) ? 'true' : undefined} required /></label><label class="checkbox"><input data-issue-path={`files[${activeIndex}].sensitive`} type="checkbox" checked={row.sensitive} onchange={(event) => setSensitive(row, event.currentTarget.checked)} /> 敏感</label><label>内容<input data-issue-path={`files[${activeIndex}].content`} type={row.sensitive ? 'password' : 'text'} bind:value={row.value} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.content')) ? 'true' : undefined} placeholder={placeholder(row)} /></label><button type="button" class="ghost" onclick={() => remove(index)}>删除</button>{#each rowIssues as issue}<p class="form-error editor-inline-error" role="alert">{issue.message}</p>{/each}</div>{/each}
  <button type="button" class="ghost" onclick={add}>添加文件</button><p class="muted">Secret 文件保持 write-only；已保存内容不会回显。</p>
  {#each issues.filter((issue) => !issue.path.includes('[')) as issue}<p class="form-error" role="alert">{issue.message}</p>{/each}
</fieldset>
