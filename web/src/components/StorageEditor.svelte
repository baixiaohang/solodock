<script lang="ts">
  import { issueAt, type FormIssue } from '../lib/formErrors'
  import { messageText, t } from '../lib/i18n'
  import type { DraftInput } from '../lib/types'

  let { volumes = $bindable<DraftInput['volumes']>([]), binds = $bindable<DraftInput['binds']>([]), allowedBindRoots = [], issues = [], onStructureChange }: { volumes: DraftInput['volumes']; binds: DraftInput['binds']; allowedBindRoots: string[]; issues?: FormIssue[]; onStructureChange?: (path: string) => void } = $props()
  function addOwned() { volumes = [...volumes, { kind: 'owned', logical_name: `data-${volumes.length + 1}`, target_path: '/data' }]; onStructureChange?.('volumes') }
  function addExternal() { volumes = [...volumes, { kind: 'external', name: '', target_path: '/data' }]; onStructureChange?.('volumes') }
  function addBind() { binds = [...binds, { source: allowedBindRoots[0] ? `${allowedBindRoots[0]}/` : '', target_path: '/data', readonly: false, acknowledge_non_rollbackable: false }]; onStructureChange?.('binds') }
  function removeVolume(index: number) { volumes = volumes.filter((_, current) => current !== index); onStructureChange?.('volumes') }
  function removeBind(index: number) { binds = binds.filter((_, current) => current !== index); onStructureChange?.('binds') }
  function setReadonly(index: number, readonly: boolean) { binds[index] = { ...binds[index], readonly, acknowledge_non_rollbackable: false }; binds = [...binds] }
  function setAcknowledged(index: number, acknowledged: boolean) { binds[index] = { ...binds[index], acknowledge_non_rollbackable: acknowledged }; binds = [...binds] }
</script>

<fieldset class="row-editor storage-editor"><legend>{$t('Persistent storage')}</legend>
  {#each volumes as volume, index}{@const volumeNamePath = volume.kind === 'owned' ? `volumes[${index}].logical_name` : `volumes[${index}].name`}{@const volumeIssue = issueAt(issues, volumeNamePath) ?? issueAt(issues, `volumes[${index}].target_path`)}<div class="editor-row storage-volume-row">
    <label>{$t('Type')}<input value={volume.kind === 'owned' ? $t('Managed') : $t('External')} readonly /></label>
    {#if volume.kind === 'owned'}<label>{$t('Logical name')}<input data-issue-path={volumeNamePath} bind:value={volume.logical_name} aria-invalid={issueAt(issues, volumeNamePath) ? 'true' : undefined} required /></label>{:else}<label>{$t('Volume name')}<input data-issue-path={volumeNamePath} bind:value={volume.name} aria-invalid={issueAt(issues, volumeNamePath) ? 'true' : undefined} required /></label>{/if}
    <label>{$t('Container path')}<input data-issue-path={`volumes[${index}].target_path`} class="path-input" bind:value={volume.target_path} aria-invalid={issueAt(issues, `volumes[${index}].target_path`) ? 'true' : undefined} required /></label>
    <button type="button" class="ghost compact" onclick={() => removeVolume(index)}>{$t('Delete')}</button>
    {#if volumeIssue}<p class="form-error storage-inline-error" role="alert">{messageText(volumeIssue.message, $t)}</p>{/if}
  </div>{/each}
  {#each binds as bind, index}{@const bindIssue = issueAt(issues, `binds[${index}].source`) ?? issueAt(issues, `binds[${index}].target_path`) ?? issueAt(issues, `binds[${index}].acknowledge_non_rollbackable`)}<div class="editor-row storage-bind-row">
    <label>{$t('Type')}<input value={$t('Host directory')} readonly /></label>
    <label>{$t('Host path')}<input data-issue-path={`binds[${index}].source`} class="path-input" bind:value={bind.source} list="allowed-bind-roots" aria-invalid={issueAt(issues, `binds[${index}].source`) ? 'true' : undefined} required /></label>
    <label>{$t('Container path')}<input data-issue-path={`binds[${index}].target_path`} class="path-input" bind:value={bind.target_path} aria-invalid={issueAt(issues, `binds[${index}].target_path`) ? 'true' : undefined} required /></label>
    <label class="checkbox compact-check"><input data-issue-path={`binds[${index}]`} type="checkbox" checked={bind.readonly} onchange={(event) => setReadonly(index, event.currentTarget.checked)} /> {$t('Read-only')}</label>
    {#if !bind.readonly}<label class="checkbox bind-ack"><input data-issue-path={`binds[${index}].acknowledge_non_rollbackable`} aria-label={$t('Confirm that read-write bind {number} does not roll back with a release', { number: index + 1 })} type="checkbox" checked={bind.acknowledge_non_rollbackable} aria-invalid={issueAt(issues, `binds[${index}].acknowledge_non_rollbackable`) ? 'true' : undefined} onchange={(event) => setAcknowledged(index, event.currentTarget.checked)} /> {$t('I understand that data does not roll back with a release')}</label>{:else}<span></span>{/if}
    <button type="button" class="ghost compact" onclick={() => removeBind(index)}>{$t('Delete')}</button>
    {#if bindIssue}<p class="form-error storage-inline-error" role="alert">{messageText(bindIssue.message, $t)}</p>{/if}
  </div>{/each}
  <datalist id="allowed-bind-roots">{#each allowedBindRoots as root}<option value={`${root}/`}>{root}</option>{/each}</datalist>
  <div class="actions"><button type="button" class="ghost" onclick={addOwned}>{$t('Add managed volume')}</button><button type="button" class="ghost" onclick={addExternal}>{$t('Add external volume')}</button><button type="button" class="ghost" disabled={!allowedBindRoots.length} onclick={addBind}>{$t('Add host directory')}</button></div>
  {#if !allowedBindRoots.length}<p class="muted">{$t('Bind mounts are disabled because no host root is allowed in system settings.')}</p>{:else}<p class="muted">{$t('Allowed roots: {roots}. SoloDock never creates, changes permissions on, or deletes host directories. Ensure the image user has access.', { roots: allowedBindRoots.join(', ') })}</p>{/if}
</fieldset>
