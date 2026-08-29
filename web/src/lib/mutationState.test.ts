import { describe, expect, it } from 'vitest'
import { initialDeletionChoice, retryIdentity } from './mutationState'

describe('mutation retry state', () => {
  it('retains a key only for the byte-equivalent action body', () => {
    const first = retryIdentity(undefined, { slug: 'one' }, () => 'key-one')
    expect(retryIdentity(first, { slug: 'one' }, () => 'unused')).toBe(first)
    expect(retryIdentity(first, { slug: 'two' }, () => 'key-two')).toEqual({
      fingerprint: '{"slug":"two"}',
      key: 'key-two',
    })
  })

  it('defaults deletion to data-preserving unregister', () => {
    expect(initialDeletionChoice()).toEqual({ removeContainer: false, previewLocked: false })
  })
})
