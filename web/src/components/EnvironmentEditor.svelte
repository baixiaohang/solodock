<script lang="ts">
  import { emptyEnvironmentRow, type EnvironmentRow } from '../lib/environmentRows'

  let { rows = $bindable() }: { rows: EnvironmentRow[] } = $props()

  function remove(row: EnvironmentRow) {
    if (row.originalKey !== null) row.removed = true
    else rows = rows.filter((candidate) => candidate.id !== row.id)
  }

  function update(row: EnvironmentRow, field: 'key' | 'value', value: string) {
    row[field] = value
    rows = [...rows]
  }

  function updateSensitive(row: EnvironmentRow, sensitive: boolean) {
    row.sensitive = sensitive
    rows = [...rows]
  }

  function valuePlaceholder(row: EnvironmentRow): string {
    const canKeep = row.storedSecret
      && row.originalSensitive
      && row.sensitive
      && row.originalKey === row.key
    return canKeep ? '已保存（留空保持）' : row.originalSensitive ? '请输入新值' : 'VALUE'
  }
</script>

<fieldset class="environment-editor">
  <legend>环境变量</legend>
  <p class="muted">每行一条配置。普通值可直接编辑；已保存的 Secret 保持不可见，留空表示 keep，输入新值表示 replace。</p>
  <div class="environment-header" aria-hidden="true"><span>名称</span><span>值</span><span>敏感</span><span>操作</span></div>
  {#each rows.filter((row) => !row.removed) as row (row.id)}
    <div class="environment-row">
      <label><span class="sr-only">变量名</span><input value={row.key} oninput={(event) => update(row, 'key', event.currentTarget.value)} required placeholder="KEY" autocomplete="off" /></label>
      <label><span class="sr-only">变量值</span><input type={row.sensitive ? 'password' : 'text'} value={row.value} oninput={(event) => update(row, 'value', event.currentTarget.value)} placeholder={valuePlaceholder(row)} autocomplete={row.sensitive ? 'new-password' : 'off'} /></label>
      <label class="sensitive-toggle"><input type="checkbox" checked={row.sensitive} onchange={(event) => updateSensitive(row, event.currentTarget.checked)} /> <span>敏感</span></label>
      <button type="button" class="ghost compact" onclick={() => remove(row)}>删除</button>
    </div>
  {:else}
    <p class="muted">尚无环境变量。</p>
  {/each}
  <button type="button" class="ghost" onclick={() => { rows = [...rows, emptyEnvironmentRow()] }}>＋ 添加一行</button>
</fieldset>
