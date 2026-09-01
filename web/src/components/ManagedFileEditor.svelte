<script lang="ts">
  import { emptyManagedFileRow, type ManagedFileRow } from '../lib/managedFileRows'
  let { rows = $bindable<ManagedFileRow[]>([]) }: { rows: ManagedFileRow[] } = $props()
  function add() { rows = [...rows, emptyManagedFileRow()] }
  function remove(index: number) { rows = rows.flatMap((row, current) => current !== index ? [row] : row.originalLogicalName !== null ? [{ ...row, removed: true }] : []) }
  function placeholder(row: ManagedFileRow): string {
    const canKeep = row.storedSecret && row.originalSensitive && row.sensitive && row.originalLogicalName === row.logicalName
    return canKeep ? '已保存（留空保持）' : row.originalSensitive ? '请输入新内容' : ''
  }
</script>
<fieldset class="row-editor"><legend>托管文件</legend>
  {#each rows as row, index}{#if !row.removed}<div class="editor-row file-row"><label>逻辑名称<input bind:value={row.logicalName} required /></label><label>容器路径<input bind:value={row.targetPath} required /></label><label class="checkbox"><input type="checkbox" bind:checked={row.sensitive} /> 敏感</label><label>内容<input type={row.sensitive ? 'password' : 'text'} bind:value={row.value} placeholder={placeholder(row)} /></label><button type="button" class="ghost" onclick={() => remove(index)}>删除</button></div>{/if}{/each}
  <button type="button" class="ghost" onclick={add}>添加文件</button><p class="muted">Secret 文件保持 write-only；已保存内容不会回显。</p>
</fieldset>
