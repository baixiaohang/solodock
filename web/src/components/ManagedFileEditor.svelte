<script lang="ts">
  import { issuesUnder, type FormIssue } from '../lib/formErrors'
  import { emptyManagedFileRow, type ManagedFileRow } from '../lib/managedFileRows'
  import { messageText, t } from '../lib/i18n'
  let { rows = $bindable<ManagedFileRow[]>([]), issues = [], onStructureChange }: { rows: ManagedFileRow[]; issues?: FormIssue[]; onStructureChange?: (path: string) => void } = $props()
  function add() { rows = [...rows, emptyManagedFileRow()]; onStructureChange?.('files') }
  function remove(index: number) { rows = rows.flatMap((row, current) => current !== index ? [row] : row.originalLogicalName !== null ? [{ ...row, removed: true }] : []); onStructureChange?.('files') }
  function setSensitive(row: ManagedFileRow, sensitive: boolean) { row.sensitive = sensitive; rows = [...rows]; onStructureChange?.('files') }
  function placeholder(row: ManagedFileRow): string {
    const canKeep = row.storedSecret && row.originalSensitive && row.sensitive && row.originalLogicalName === row.logicalName
    return canKeep ? $t('Saved (leave empty to keep)') : row.originalSensitive ? $t('Enter new content') : ''
  }
  let activeRows = $derived(rows.filter((row) => !row.removed))
</script>
<fieldset class="row-editor"><legend>{$t('Managed files')}</legend>
  {#each activeRows as row}{@const index = rows.indexOf(row)}{@const activeIndex = activeRows.indexOf(row)}{@const rowIssues = issuesUnder(issues, `files[${activeIndex}]`)}<div class="editor-row file-row"><label>{$t('Logical name')}<input data-issue-path={`files[${activeIndex}].logical_name`} bind:value={row.logicalName} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.logical_name')) ? 'true' : undefined} required /></label><label>{$t('Container path')}<input data-issue-path={`files[${activeIndex}].target_path`} bind:value={row.targetPath} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.target_path')) ? 'true' : undefined} required /></label><label class="checkbox"><input data-issue-path={`files[${activeIndex}].sensitive`} type="checkbox" checked={row.sensitive} onchange={(event) => setSensitive(row, event.currentTarget.checked)} /> {$t('Sensitive')}</label><label>{$t('Content')}<input data-issue-path={`files[${activeIndex}].content`} type={row.sensitive ? 'password' : 'text'} bind:value={row.value} aria-invalid={rowIssues.some((issue) => issue.path.endsWith('.content')) ? 'true' : undefined} placeholder={placeholder(row)} /></label><button type="button" class="ghost" onclick={() => remove(index)}>{$t('Delete')}</button>{#each rowIssues as issue}<p class="form-error editor-inline-error" role="alert">{messageText(issue.message, $t)}</p>{/each}</div>{/each}
  <button type="button" class="ghost" onclick={add}>{$t('Add file')}</button><p class="muted">{$t('Secret files remain write-only; saved content is never returned.')}</p>
  {#each issues.filter((issue) => !issue.path.includes('[')) as issue}<p class="form-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
</fieldset>
