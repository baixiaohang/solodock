import type { DraftInput, ExternalNetworkAttachment } from './types'

export interface NetworkEditorState {
  ownedDefaultNetwork: boolean
  serviceDiscoveryEnabled: boolean
  externalNetworks: ExternalNetworkAttachment[]
}

export function networkEditorState(
  ownedDefaultNetwork: boolean,
  serviceDiscoveryEnabled: boolean,
  networks: DraftInput['networks'],
): NetworkEditorState {
  return {
    ownedDefaultNetwork,
    serviceDiscoveryEnabled,
    externalNetworks: networks
      .filter((network): network is Extract<DraftInput['networks'][number], { kind: 'external' }> => network.kind === 'external')
      .map((network) => ({ name: network.name, aliases: [...(network.aliases ?? [])] })),
  }
}

export function networkEditorError(state: NetworkEditorState): string {
  if (!state.ownedDefaultNetwork && !state.serviceDiscoveryEnabled && state.externalNetworks.length === 0) {
    return '至少需要应用专属网络、平台内部服务发现或一个 external network。'
  }
  return ''
}

export function networkDraft(state: NetworkEditorState): Pick<DraftInput, 'owned_default_network' | 'networks'> {
  return {
    owned_default_network: state.ownedDefaultNetwork,
    networks: state.externalNetworks.map((network) => ({
      kind: 'external' as const,
      name: network.name,
      aliases: [...network.aliases],
    })),
  }
}
