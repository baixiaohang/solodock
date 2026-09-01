<script lang="ts">
  import type { DraftInput } from '../lib/types'
  let { volumes = $bindable<DraftInput['volumes']>([]), binds = $bindable<DraftInput['binds']>([]), allowedBindRoots = [] }: { volumes: DraftInput['volumes']; binds: DraftInput['binds']; allowedBindRoots: string[] } = $props()
  function addOwned() { volumes = [...volumes, { kind: 'owned', logical_name: `data-${volumes.length + 1}`, target_path: '/data' }] }
  function addExternal() { volumes = [...volumes, { kind: 'external', name: '', target_path: '/data' }] }
  function addBind() { binds = [...binds, { source: allowedBindRoots[0] ? `${allowedBindRoots[0]}/` : '', target_path: '/data', readonly: false, acknowledge_non_rollbackable: true }] }
</script>

<fieldset class="row-editor"><legend>持久存储</legend>
  {#each volumes as volume, index}<div class="editor-row storage-row">
    <label>类型<input value={volume.kind === 'owned' ? 'SoloDock managed volume' : 'External volume'} readonly /></label>
    {#if volume.kind === 'owned'}<label>逻辑名称<input bind:value={volume.logical_name} required /></label>{:else}<label>Volume 名称<input bind:value={volume.name} required /></label>{/if}
    <label>容器路径<input bind:value={volume.target_path} required /></label>
    <button type="button" class="ghost" onclick={() => { volumes = volumes.filter((_, current) => current !== index) }}>删除</button>
  </div>{/each}
  {#each binds as bind, index}<div class="editor-row storage-row">
    <label>类型<input value="现有宿主目录" readonly /></label><label>宿主路径<input bind:value={bind.source} list="allowed-bind-roots" required /></label><label>容器路径<input bind:value={bind.target_path} required /></label>
    <label class="checkbox"><input type="checkbox" bind:checked={bind.readonly} /> 只读</label>
    <button type="button" class="ghost" onclick={() => { binds = binds.filter((_, current) => current !== index) }}>删除</button>
  </div>{/each}
  <datalist id="allowed-bind-roots">{#each allowedBindRoots as root}<option value={`${root}/`}>{root}</option>{/each}</datalist>
  <div class="actions"><button type="button" class="ghost" onclick={addOwned}>添加 managed volume</button><button type="button" class="ghost" onclick={addExternal}>添加 external volume</button><button type="button" class="ghost" disabled={!allowedBindRoots.length} onclick={addBind}>添加宿主目录</button></div>
  {#if !allowedBindRoots.length}<p class="muted">系统设置尚未允许任何宿主根目录，因此 bind mount 已禁用。</p>{:else}<p class="muted">允许范围：{allowedBindRoots.join('、')}。SoloDock 不会创建、改权限或删除宿主目录；请自行确认镜像用户有权限。</p>{/if}
</fieldset>
