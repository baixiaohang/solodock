import type { DraftInput, ExternalNetworkAttachment } from './types'

export interface NetworkEditorState {
  ownedDefaultNetwork: boolean
  externalNetworks: ExternalNetworkAttachment[]
}

export function networkEditorState(
  ownedDefaultNetwork: boolean,
  networks: DraftInput['networks'],
): NetworkEditorState {
  return {
    ownedDefaultNetwork,
    externalNetworks: networks
      .filter((network): network is Extract<DraftInput['networks'][number], { kind: 'external' }> => network.kind === 'external')
      .map((network) => ({ name: network.name, aliases: [...(network.aliases ?? [])] })),
  }
}

export function networkEditorError(state: NetworkEditorState): string {
  if (!state.ownedDefaultNetwork && state.externalNetworks.length === 0) {
    return '关闭应用专属默认网络时，至少需要一个 external network。'
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
