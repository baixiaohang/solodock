<script lang="ts">
  import { issueAt, type FormIssue } from '../lib/formErrors'
  import type { DraftInput } from '../lib/types'
  let { ports = $bindable<DraftInput['ports']>([]), issues = [], onStructureChange }: { ports: DraftInput['ports']; issues?: FormIssue[]; onStructureChange?: (path: string) => void } = $props()
  function add() { ports = [...ports, { host_ip: '127.0.0.1', host_port: 3000, container_port: 3000, protocol: 'tcp' }]; onStructureChange?.('ports') }
  function remove(index: number) { ports = ports.filter((_, current) => current !== index); onStructureChange?.('ports') }
</script>

<fieldset class="row-editor"><legend>端口</legend><p class="muted">只允许发布到 loopback；不填写时不会开放宿主端口。</p>
  {#each ports as port, index}{@const portIssue = issueAt(issues, `ports[${index}].host_ip`) ?? issueAt(issues, `ports[${index}].host_port`) ?? issueAt(issues, `ports[${index}].container_port`)}<div class="editor-row port-row">
    <label>Host IP<select data-issue-path={`ports[${index}].host_ip`} bind:value={port.host_ip} aria-invalid={issueAt(issues, `ports[${index}].host_ip`) ? 'true' : undefined}><option value="127.0.0.1">127.0.0.1</option><option value="::1">::1</option></select></label>
    <label>宿主端口<input data-issue-path={`ports[${index}].host_port`} type="number" min="1" max="65535" bind:value={port.host_port} aria-invalid={issueAt(issues, `ports[${index}].host_port`) ? 'true' : undefined} required /></label>
    <label>容器端口<input data-issue-path={`ports[${index}].container_port`} type="number" min="1" max="65535" bind:value={port.container_port} aria-invalid={issueAt(issues, `ports[${index}].container_port`) ? 'true' : undefined} required /></label>
    <label>协议<select data-issue-path={`ports[${index}].protocol`} bind:value={port.protocol}><option value="tcp">TCP</option><option value="udp">UDP</option></select></label>
    <button type="button" class="ghost" onclick={() => remove(index)}>删除</button>
    {#if portIssue}<p class="form-error storage-inline-error" role="alert">{portIssue.message}</p>{/if}
  </div>{/each}
  <button type="button" class="ghost" onclick={add}>添加端口</button>
</fieldset>
