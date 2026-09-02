<script lang="ts">
  import { issueAt, type FormIssue } from '../lib/formErrors'
  import type { DraftInput } from '../lib/types'

  let {
    volumes = $bindable<DraftInput['volumes']>([]),
    binds = $bindable<DraftInput['binds']>([]),
    allowedBindRoots = [],
    issues = [],
    onStructureChange,
  }: { volumes: DraftInput['volumes']; binds: DraftInput['binds']; allowedBindRoots: string[]; issues?: FormIssue[]; onStructureChange?: (path: string) => void } = $props()

  function addOwned() { volumes = [...volumes, { kind: 'owned', logical_name: `data-${volumes.length + 1}`, target_path: '/data' }]; onStructureChange?.('volumes') }
  function addExternal() { volumes = [...volumes, { kind: 'external', name: '', target_path: '/data' }]; onStructureChange?.('volumes') }
  function addBind() { binds = [...binds, { source: allowedBindRoots[0] ? `${allowedBindRoots[0]}/` : '', target_path: '/data', readonly: false, acknowledge_non_rollbackable: false }]; onStructureChange?.('binds') }
  function removeVolume(index: number) { volumes = volumes.filter((_, current) => current !== index); onStructureChange?.('volumes') }
  function removeBind(index: number) { binds = binds.filter((_, current) => current !== index); onStructureChange?.('binds') }
  function setReadonly(index: number, readonly: boolean) {
    binds[index] = { ...binds[index], readonly, acknowledge_non_rollbackable: false }
    binds = [...binds]
  }
  function setAcknowledged(index: number, acknowledged: boolean) {
    binds[index] = { ...binds[index], acknowledge_non_rollbackable: acknowledged }
    binds = [...binds]
  }
</script>

<fieldset class="row-editor storage-editor"><legend>持久存储</legend>
  {#each volumes as volume, index}{@const volumeNamePath = volume.kind === 'owned' ? `volumes[${index}].logical_name` : `volumes[${index}].name`}{@const volumeIssue = issueAt(issues, volumeNamePath) ?? issueAt(issues, `volumes[${index}].target_path`)}<div class="editor-row storage-volume-row">
    <label>类型<input value={volume.kind === 'owned' ? 'Managed' : 'External'} readonly /></label>
    {#if volume.kind === 'owned'}<label>逻辑名称<input data-issue-path={volumeNamePath} bind:value={volume.logical_name} aria-invalid={issueAt(issues, volumeNamePath) ? 'true' : undefined} required /></label>{:else}<label>Volume 名称<input data-issue-path={volumeNamePath} bind:value={volume.name} aria-invalid={issueAt(issues, volumeNamePath) ? 'true' : undefined} required /></label>{/if}
    <label>容器路径<input data-issue-path={`volumes[${index}].target_path`} class="path-input" bind:value={volume.target_path} aria-invalid={issueAt(issues, `volumes[${index}].target_path`) ? 'true' : undefined} required /></label>
    <button type="button" class="ghost compact" onclick={() => removeVolume(index)}>删除</button>
    {#if volumeIssue}<p class="form-error storage-inline-error" role="alert">{volumeIssue.message}</p>{/if}
  </div>{/each}
  {#each binds as bind, index}{@const bindIssue = issueAt(issues, `binds[${index}].source`) ?? issueAt(issues, `binds[${index}].target_path`) ?? issueAt(issues, `binds[${index}].acknowledge_non_rollbackable`)}<div class="editor-row storage-bind-row">
    <label>类型<input value="宿主目录" readonly /></label>
    <label>宿主路径<input data-issue-path={`binds[${index}].source`} class="path-input" bind:value={bind.source} list="allowed-bind-roots" aria-invalid={issueAt(issues, `binds[${index}].source`) ? 'true' : undefined} required /></label>
    <label>容器路径<input data-issue-path={`binds[${index}].target_path`} class="path-input" bind:value={bind.target_path} aria-invalid={issueAt(issues, `binds[${index}].target_path`) ? 'true' : undefined} required /></label>
    <label class="checkbox compact-check"><input data-issue-path={`binds[${index}]`} type="checkbox" checked={bind.readonly} onchange={(event) => setReadonly(index, event.currentTarget.checked)} /> 只读</label>
    {#if !bind.readonly}<label class="checkbox bind-ack"><input data-issue-path={`binds[${index}].acknowledge_non_rollbackable`} aria-label={`确认读写 bind ${index + 1} 不随 release 回滚`} type="checkbox" checked={bind.acknowledge_non_rollbackable} aria-invalid={issueAt(issues, `binds[${index}].acknowledge_non_rollbackable`) ? 'true' : undefined} onchange={(event) => setAcknowledged(index, event.currentTarget.checked)} /> 我了解数据不随 release 回滚</label>{:else}<span></span>{/if}
    <button type="button" class="ghost compact" onclick={() => removeBind(index)}>删除</button>
    {#if bindIssue}<p class="form-error storage-inline-error" role="alert">{bindIssue.message}</p>{/if}
  </div>{/each}
  <datalist id="allowed-bind-roots">{#each allowedBindRoots as root}<option value={`${root}/`}>{root}</option>{/each}</datalist>
  <div class="actions"><button type="button" class="ghost" onclick={addOwned}>添加 managed volume</button><button type="button" class="ghost" onclick={addExternal}>添加 external volume</button><button type="button" class="ghost" disabled={!allowedBindRoots.length} onclick={addBind}>添加宿主目录</button></div>
  {#if !allowedBindRoots.length}<p class="muted">系统设置尚未允许任何宿主根目录，因此 bind mount 已禁用。</p>{:else}<p class="muted">允许范围：{allowedBindRoots.join('、')}。SoloDock 不会创建、改权限或删除宿主目录；请自行确认镜像用户有权限。</p>{/if}
</fieldset>
