<script lang="ts">
  import { networkEditorError } from '../lib/networks'
  import { issuesUnder, type FormIssue } from '../lib/formErrors'
  import type { ExternalNetworkAttachment } from '../lib/types'

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
  <legend>网络</legend>
  <label class="checkbox">
    <input data-issue-path="owned_default_network" aria-label="创建应用专属默认网络" type="checkbox" bind:checked={ownedDefaultNetwork} />
    创建应用专属默认网络
  </label>
  <label class="checkbox"><input data-issue-path="service_discovery_enabled" aria-label="启用平台内部服务发现" type="checkbox" bind:checked={serviceDiscoveryEnabled} /> 启用平台内部服务发现</label>
  <p class="muted">启用后可通过服务 slug 和容器端口互通；所有启用服务属于同一内部信任域，当前没有服务级网络 ACL。</p>
  <p class="muted">External network 必须预先存在；SoloDock 不会创建、修改或删除它。</p>
  {#each externalNetworks as network, networkIndex}
    {@const networkIssues = issuesUnder(issues, `networks[${networkIndex}]`)}
    <section class="network-attachment">
      <div class="network-row">
        <label>External network 名称<input data-issue-path={`networks[${networkIndex}].name`} aria-label={`External network ${networkIndex + 1}`} value={network.name} oninput={(event) => updateNetworkName(networkIndex, event.currentTarget.value)} aria-invalid={networkIssues.some((issue) => issue.path.endsWith('.name')) ? 'true' : undefined} required /></label>
        <button type="button" class="ghost" onclick={() => removeNetwork(networkIndex)}>删除网络</button>
      </div>
      <div class="alias-list">
        {#each network.aliases as alias, aliasIndex}
          <div class="alias-row">
            <label>Alias<input data-issue-path={`networks[${networkIndex}].aliases[${aliasIndex}]`} aria-label={`Network ${networkIndex + 1} alias ${aliasIndex + 1}`} value={alias} oninput={(event) => updateAlias(networkIndex, aliasIndex, event.currentTarget.value)} aria-invalid={networkIssues.some((issue) => issue.path.includes(`aliases[${aliasIndex}]`)) ? 'true' : undefined} required pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" maxlength="63" /></label>
            <button type="button" class="ghost" onclick={() => removeAlias(networkIndex, aliasIndex)}>删除 alias</button>
          </div>
        {/each}
        <button type="button" class="ghost" onclick={() => addAlias(networkIndex)}>添加 alias</button>
      </div>
      {#each networkIssues as issue}<p class="form-error" role="alert">{issue.message}</p>{/each}
    </section>
  {/each}
  <button type="button" class="ghost" onclick={addNetwork}>添加 external network</button>
  {#if error}<p class="form-error" role="alert">{error}</p>{/if}
  {#each issues.filter((issue) => !issue.path.includes('[')) as issue}<p class="form-error" role="alert">{issue.message}</p>{/each}
</fieldset>
