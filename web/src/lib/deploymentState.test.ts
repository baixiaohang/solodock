import { afterEach, describe, expect, it, vi } from 'vitest'
import { clearWriteOnlyCredential, isTerminalDeployment, writeOnlyRetryIdentity } from './deploymentState'
import { LocalMutationValidationError } from './mutationState'

afterEach(() => vi.restoreAllMocks())

describe('deployment state', () => {
  it('polls only queued and running deployments', () => {
    expect(isTerminalDeployment('queued')).toBe(false)
    expect(isTerminalDeployment('running')).toBe(false)
    for (const value of ['succeeded', 'no_op', 'failed', 'rolled_back', 'needs_attention', 'interrupted'] as const) {
      expect(isTerminalDeployment(value)).toBe(true)
    }
  })

  it('clears write-only credential input after use', () => {
    const form = { secret: 'secret-canary' }
    clearWriteOnlyCredential(form)
    expect(form.secret).toBe('')
    expect(JSON.stringify(form)).not.toContain('secret-canary')
  })

  it('retains only a digest when reusing a write-only request identity', async () => {
    const secret = 'retry-secret-canary'
    const first = await writeOnlyRetryIdentity(undefined, { registry: 'ghcr.io' }, secret)
    const replay = await writeOnlyRetryIdentity(first, { registry: 'ghcr.io' }, secret)
    expect(replay.key).toBe(first.key)
    expect(JSON.stringify(first)).not.toContain(secret)
  })

  it('never stores a webhook secret in its retry identity', async () => {
    const secret = 'webhook-retry-secret-canary'
    const identity = await writeOnlyRetryIdentity(
      undefined,
      { expected_metadata_revision: 'revision' },
      secret,
    )
    expect(JSON.stringify(identity)).not.toContain(secret)
    expect(identity.fingerprint).toContain('secretSha256')
  })

  it('classifies a local digest failure before any mutation as known not applied', async () => {
    vi.spyOn(crypto.subtle, 'digest').mockRejectedValueOnce(new Error('local crypto unavailable'))
    await expect(writeOnlyRetryIdentity(undefined, {}, crypto.randomUUID())).rejects.toBeInstanceOf(LocalMutationValidationError)
  })
})
