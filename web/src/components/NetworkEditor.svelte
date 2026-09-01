<script lang="ts">
  import { networkEditorError } from '../lib/networks'
  import type { ExternalNetworkAttachment } from '../lib/types'

  let {
    ownedDefaultNetwork = $bindable(true),
    serviceDiscoveryEnabled = $bindable(true),
    externalNetworks = $bindable<ExternalNetworkAttachment[]>([]),
  }: {
    ownedDefaultNetwork: boolean
    serviceDiscoveryEnabled?: boolean
    externalNetworks: ExternalNetworkAttachment[]
  } = $props()

  let error = $derived(networkEditorError({ ownedDefaultNetwork, serviceDiscoveryEnabled, externalNetworks }))

  function addNetwork() {
    externalNetworks = [...externalNetworks, { name: '', aliases: [] }]
  }

  function removeNetwork(index: number) {
    externalNetworks = externalNetworks.filter((_, current) => current !== index)
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
  }

  function removeAlias(networkIndex: number, aliasIndex: number) {
    externalNetworks = externalNetworks.map((network, index) => index === networkIndex
      ? { ...network, aliases: network.aliases.filter((_, current) => current !== aliasIndex) }
      : network)
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
    <input aria-label="创建应用专属默认网络" type="checkbox" bind:checked={ownedDefaultNetwork} />
    创建应用专属默认网络
  </label>
  <label class="checkbox"><input aria-label="启用平台内部服务发现" type="checkbox" bind:checked={serviceDiscoveryEnabled} /> 启用平台内部服务发现</label>
  <p class="muted">启用后可通过服务 slug 和容器端口互通；所有启用服务属于同一内部信任域，当前没有服务级网络 ACL。</p>
  <p class="muted">External network 必须预先存在；SoloDock 不会创建、修改或删除它。</p>
  {#each externalNetworks as network, networkIndex}
    <section class="network-attachment">
      <div class="network-row">
        <label>External network 名称<input aria-label={`External network ${networkIndex + 1}`} value={network.name} oninput={(event) => updateNetworkName(networkIndex, event.currentTarget.value)} required /></label>
        <button type="button" class="ghost" onclick={() => removeNetwork(networkIndex)}>删除网络</button>
      </div>
      <div class="alias-list">
        {#each network.aliases as alias, aliasIndex}
          <div class="alias-row">
            <label>Alias<input aria-label={`Network ${networkIndex + 1} alias ${aliasIndex + 1}`} value={alias} oninput={(event) => updateAlias(networkIndex, aliasIndex, event.currentTarget.value)} required pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" maxlength="63" /></label>
            <button type="button" class="ghost" onclick={() => removeAlias(networkIndex, aliasIndex)}>删除 alias</button>
          </div>
        {/each}
        <button type="button" class="ghost" onclick={() => addAlias(networkIndex)}>添加 alias</button>
      </div>
    </section>
  {/each}
  <button type="button" class="ghost" onclick={addNetwork}>添加 external network</button>
  {#if error}<p class="form-error" role="alert">{error}</p>{/if}
</fieldset>
