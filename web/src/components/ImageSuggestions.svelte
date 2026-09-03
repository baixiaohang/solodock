<script lang="ts">
  import { ApiError, mutation } from '../lib/api'
  import type { DraftInput, ImageConfigSuggestion } from '../lib/types'
  import { localized, messageText, t, type UserMessage } from '../lib/i18n'
  let { image, credentialRef, ports = $bindable<DraftInput['ports']>([]), volumes = $bindable<DraftInput['volumes']>([]), onStructureChange }: { image: string; credentialRef: string | null; ports: DraftInput['ports']; volumes: DraftInput['volumes']; onStructureChange?: (path: string) => void } = $props()
  let suggestion = $state<ImageConfigSuggestion | null>(null)
  let busy = $state(false)
  let error = $state<UserMessage | null>(null)
  async function inspect() {
    busy = true
    error = null
    suggestion = null
    try {
      suggestion = await mutation('/api/v1/images/inspect-config', {
        discovery_image_ref: image,
        credential_ref: credentialRef,
      })
    } catch (cause) {
      error = cause instanceof ApiError
        ? `${cause.body.code}: ${cause.body.message}`
        : localized('Could not connect to the SoloDock control plane; try again later.')
    } finally {
      busy = false
    }
  }
  function applyPort(port: ImageConfigSuggestion['exposed_ports'][number]) { if (!ports.some((item) => item.container_port === port.container_port && item.protocol === port.protocol)) { ports = [...ports, { host_ip: '127.0.0.1', host_port: port.container_port, ...port }]; onStructureChange?.('ports') } }
  function applyVolume(target: string) { if (!volumes.some((item) => item.target_path === target)) { volumes = [...volumes, { kind: 'owned', logical_name: `data-${volumes.length + 1}`, target_path: target }]; onStructureChange?.('volumes') } }
</script>
<section class="image-suggestions"><button type="button" class="ghost" disabled={busy || !image} onclick={() => void inspect()}>{busy ? $t('Reading…') : $t('Read image configuration suggestions')}</button>
{#if error}<p class="muted">{messageText(error, $t)}</p>{/if}
{#if suggestion}<div class="notice"><p><strong>{$t('Digest')}:</strong> <code>{suggestion.resolved_digest}</code> · {$t('User')} <code>{suggestion.user ?? $t('Not declared')}</code> · {$t('Stop signal')} <code>{suggestion.stop_signal ?? $t('Default')}</code></p>
{#each suggestion.exposed_ports as port}<button type="button" class="ghost" onclick={() => applyPort(port)}>{$t('Use port {port}/{protocol}', { port: port.container_port, protocol: port.protocol })}</button>{/each}
{#each suggestion.volume_targets as target}<button type="button" class="ghost" onclick={() => applyVolume(target)}>{$t('Use persistent path {path}', { path: target })}</button>{/each}
<p class="muted">{$t('Suggestions never overwrite the current configuration automatically. They take effect only after you select and save them to the draft.')}</p></div>{/if}</section>
