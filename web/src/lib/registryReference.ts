import type { RegistryCredential } from './types'

export function logicalRegistry(reference: string): string | null {
  const value = reference.trim()
  if (!value || value.includes('://') || value.includes('@')) return null
  const parts = value.split('/')
  if (parts.length === 1) return 'docker.io'
  const first = parts[0]?.toLowerCase()
  if (!first) return null
  if (first.includes('.') || first.includes(':') || first === 'localhost') {
    return ['index.docker.io', 'registry-1.docker.io'].includes(first) ? 'docker.io' : first
  }
  return 'docker.io'
}

export function credentialsForReference(
  credentials: RegistryCredential[],
  reference: string,
): RegistryCredential[] {
  const registry = logicalRegistry(reference)
  return registry ? credentials.filter((credential) => credential.registry === registry) : []
}
