import type { DeploymentStatus } from './types'
import { retryIdentity, type RetryIdentity } from './mutationState'

export function isTerminalDeployment(status: DeploymentStatus): boolean {
  return !['queued', 'running'].includes(status)
}

export function clearWriteOnlyCredential(value: { secret: string }): void {
  value.secret = ''
}

export async function writeOnlyRetryIdentity(
  previous: RetryIdentity | undefined,
  publicBody: unknown,
  secret: string,
): Promise<RetryIdentity> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(secret))
  const secretSha256 = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('')
  return retryIdentity(previous, { publicBody, secretSha256 })
}
