<script lang="ts">
  import { networkEditorError } from '../lib/networks'
  import { issuesUnder, type FormIssue } from '../lib/formErrors'
  import type { ExternalNetworkAttachment } from '../lib/types'
  import { messageText, t } from '../lib/i18n'

  let {
    ownedDefaultNetwork = $bindable(true),
    serviceDiscoveryEnabled = $bindable(true),
    externalNetworks = $bindable<ExternalNetworkAttachment[]>([]),
    issues = [],
    onStructureChange,
  }: {
    ownedDefaultNetwork: boolean
    serviceDiscoveryEnabled?: boolean
    externalNetworks: ExternalNetworkAttachment[]
    issues?: FormIssue[]
    onStructureChange?: (path: string) => void
  } = $props()

  let error = $derived(networkEditorError({ ownedDefaultNetwork, serviceDiscoveryEnabled, externalNetworks }))

  function addNetwork() {
    externalNetworks = [...externalNetworks, { name: '', aliases: [] }]
    onStructureChange?.('networks')
  }

  function removeNetwork(index: number) {
    externalNetworks = externalNetworks.filter((_, current) => current !== index)
    onStructureChange?.('networks')
  }

  function updateNetworkName(networkIndex: number, name: string) {
    externalNetworks = externalNetworks.map((network, index) => index === networkIndex
      ? { ...network, name }
      : network)
  }

  function addAlias(networkIndex: number) {
    externalNetworks = externalNetworks.map((network, index) => index === networkIndex
      ? { ...network, aliases: [...network.aliases, ''] }
      : network)
    onStructureChange?.('networks')
  }

  function removeAlias(networkIndex: number, aliasIndex: number) {
    externalNetworks = externalNetworks.map((network, index) => index === networkIndex
      ? { ...network, aliases: network.aliases.filter((_, current) => current !== aliasIndex) }
      : network)
    onStructureChange?.('networks')
  }

  function updateAlias(networkIndex: number, aliasIndex: number, alias: string) {
    externalNetworks = externalNetworks.map((network, index) => index === networkIndex
      ? { ...network, aliases: network.aliases.map((value, current) => current === aliasIndex ? alias : value) }
      : network)
  }
</script>

<fieldset class="network-editor wide">
  <legend>{$t('Networks')}</legend>
  <label class="checkbox">
    <input data-issue-path="owned_default_network" aria-label={$t('Create an application-specific default network')} type="checkbox" bind:checked={ownedDefaultNetwork} />
    {$t('Create an application-specific default network')}
  </label>
  <label class="checkbox"><input data-issue-path="service_discovery_enabled" aria-label={$t('Enable platform service discovery')} type="checkbox" bind:checked={serviceDiscoveryEnabled} /> {$t('Enable platform service discovery')}</label>
  <p class="muted">{$t('Enabled services can communicate through service slugs and container ports. All enabled services share one internal trust domain; service-level network ACLs are not available.')}</p>
  <p class="muted">{$t('External networks must already exist. SoloDock never creates, changes, or deletes them.')}</p>
  {#each externalNetworks as network, networkIndex}
    {@const networkIssues = issuesUnder(issues, `networks[${networkIndex}]`)}
    <section class="network-attachment">
      <div class="network-row">
        <label>{$t('External network name')}<input data-issue-path={`networks[${networkIndex}].name`} aria-label={$t('External network {number}', { number: networkIndex + 1 })} value={network.name} oninput={(event) => updateNetworkName(networkIndex, event.currentTarget.value)} aria-invalid={networkIssues.some((issue) => issue.path.endsWith('.name')) ? 'true' : undefined} required /></label>
        <button type="button" class="ghost" onclick={() => removeNetwork(networkIndex)}>{$t('Delete network')}</button>
      </div>
      <div class="alias-list">
        {#each network.aliases as alias, aliasIndex}
          <div class="alias-row">
            <label>{$t('Alias')}<input data-issue-path={`networks[${networkIndex}].aliases[${aliasIndex}]`} aria-label={$t('Network {network} alias {alias}', { network: networkIndex + 1, alias: aliasIndex + 1 })} value={alias} oninput={(event) => updateAlias(networkIndex, aliasIndex, event.currentTarget.value)} aria-invalid={networkIssues.some((issue) => issue.path.includes(`aliases[${aliasIndex}]`)) ? 'true' : undefined} required pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" maxlength="63" /></label>
            <button type="button" class="ghost" onclick={() => removeAlias(networkIndex, aliasIndex)}>{$t('Delete alias')}</button>
          </div>
        {/each}
        <button type="button" class="ghost" onclick={() => addAlias(networkIndex)}>{$t('Add alias')}</button>
      </div>
      {#each networkIssues as issue}<p class="form-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
    </section>
  {/each}
  <button type="button" class="ghost" onclick={addNetwork}>{$t('Add external network')}</button>
  {#if error}<p class="form-error" role="alert">{messageText(error, $t)}</p>{/if}
  {#each issues.filter((issue) => !issue.path.includes('[')) as issue}<p class="form-error" role="alert">{messageText(issue.message, $t)}</p>{/each}
</fieldset>
