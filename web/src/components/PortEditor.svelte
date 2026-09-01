<script lang="ts">
  import type { DraftInput } from '../lib/types'
  let { ports = $bindable<DraftInput['ports']>([]) }: { ports: DraftInput['ports'] } = $props()
  function add() { ports = [...ports, { host_ip: '127.0.0.1', host_port: 3000, container_port: 3000, protocol: 'tcp' }] }
  function remove(index: number) { ports = ports.filter((_, current) => current !== index) }
</script>

<fieldset class="row-editor"><legend>端口</legend><p class="muted">只允许发布到 loopback；不填写时不会开放宿主端口。</p>
  {#each ports as port, index}<div class="editor-row port-row">
    <label>Host IP<select bind:value={port.host_ip}><option value="127.0.0.1">127.0.0.1</option><option value="::1">::1</option></select></label>
    <label>宿主端口<input type="number" min="1" max="65535" bind:value={port.host_port} required /></label>
    <label>容器端口<input type="number" min="1" max="65535" bind:value={port.container_port} required /></label>
    <label>协议<select bind:value={port.protocol}><option value="tcp">TCP</option><option value="udp">UDP</option></select></label>
    <button type="button" class="ghost" onclick={() => remove(index)}>删除</button>
  </div>{/each}
  <button type="button" class="ghost" onclick={add}>添加端口</button>
</fieldset>
