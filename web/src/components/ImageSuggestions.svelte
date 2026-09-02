<script lang="ts">
  import { ApiError, mutation } from '../lib/api'
  import type { DraftInput, ImageConfigSuggestion } from '../lib/types'
  let { image, credentialRef, ports = $bindable<DraftInput['ports']>([]), volumes = $bindable<DraftInput['volumes']>([]), onStructureChange }: { image: string; credentialRef: string | null; ports: DraftInput['ports']; volumes: DraftInput['volumes']; onStructureChange?: (path: string) => void } = $props()
  let suggestion = $state<ImageConfigSuggestion | null>(null)
  let busy = $state(false)
  let error = $state('')
  async function inspect() {
    busy = true
    error = ''
    suggestion = null
    try {
      suggestion = await mutation('/api/v1/images/inspect-config', {
        discovery_image_ref: image,
        credential_ref: credentialRef,
      })
    } catch (cause) {
      error = cause instanceof ApiError
        ? `${cause.body.code}: ${cause.body.message}`
        : '无法连接 SoloDock 控制面，请稍后重试。'
    } finally {
      busy = false
    }
  }
  function applyPort(port: ImageConfigSuggestion['exposed_ports'][number]) { if (!ports.some((item) => item.container_port === port.container_port && item.protocol === port.protocol)) { ports = [...ports, { host_ip: '127.0.0.1', host_port: port.container_port, ...port }]; onStructureChange?.('ports') } }
  function applyVolume(target: string) { if (!volumes.some((item) => item.target_path === target)) { volumes = [...volumes, { kind: 'owned', logical_name: `data-${volumes.length + 1}`, target_path: target }]; onStructureChange?.('volumes') } }
</script>
<section class="image-suggestions"><button type="button" class="ghost" disabled={busy || !image} onclick={() => void inspect()}>{busy ? '读取中…' : '读取镜像配置建议'}</button>
{#if error}<p class="muted">{error}</p>{/if}
{#if suggestion}<div class="notice"><p><strong>Digest：</strong><code>{suggestion.resolved_digest}</code> · User <code>{suggestion.user ?? '未声明'}</code> · Stop signal <code>{suggestion.stop_signal ?? '默认'}</code></p>
{#each suggestion.exposed_ports as port}<button type="button" class="ghost" onclick={() => applyPort(port)}>采用端口 {port.container_port}/{port.protocol}</button>{/each}
{#each suggestion.volume_targets as target}<button type="button" class="ghost" onclick={() => applyVolume(target)}>采用持久目录 {target}</button>{/each}
<p class="muted">建议不会自动覆盖当前配置；只有点击采用并保存 draft 后才生效。</p></div>{/if}</section>
