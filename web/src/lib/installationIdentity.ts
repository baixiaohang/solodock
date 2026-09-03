import type { Translate } from './i18n'
import type { InstallationIdentity } from './types'

export const unknownInstallationIdentity: InstallationIdentity = {
  channel: 'unknown',
  version: null,
  source_sha: null,
  package_identity: null,
}

export function installationSummary(identity: InstallationIdentity, translate: Translate): string {
  const source = identity.source_sha?.slice(0, 7)
  if (identity.channel === 'stable' && identity.version && source) {
    return translate('SoloDock v{version} · stable · {sha}', { version: identity.version, sha: source })
  }
  if (identity.channel === 'main' && source) {
    return translate('SoloDock main · {sha}', { sha: source })
  }
  if (identity.channel === 'development' && identity.version) {
    return translate('SoloDock {version} · development', { version: identity.version })
  }
  return translate('SoloDock · unknown')
}

export function copyableInstallationIdentity(identity: InstallationIdentity): string {
  return [
    `channel=${identity.channel}`,
    `version=${identity.version ?? 'unknown'}`,
    `source_sha=${identity.source_sha ?? 'unknown'}`,
    `package_identity=${identity.package_identity ?? 'unknown'}`,
  ].join('\n')
}
