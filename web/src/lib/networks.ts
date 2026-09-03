import type { DraftInput, ExternalNetworkAttachment } from './types'
import { localized, type LocalizedMessage } from './i18n'

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

export function networkEditorError(state: NetworkEditorState): LocalizedMessage | null {
  if (!state.ownedDefaultNetwork && !state.serviceDiscoveryEnabled && state.externalNetworks.length === 0) {
    return localized('At least an application network, platform service discovery, or one external network is required.')
  }
  return null
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
