import { describe, expect, it } from 'vitest'
import { ApiError } from './api'
import { initialDeletionChoice, LocalMutationValidationError, mutationFailure, retryIdentity } from './mutationState'

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

  it('retains retry state only when the mutation outcome is unknown', () => {
    const retry = { fingerprint: '{}', key: 'key-one' }
    const body = { code: 'TEST', message: 'safe', request_id: '' }

    expect(mutationFailure(retry, new TypeError('network failed'))).toEqual({
      outcome: 'outcome_unknown', retry,
    })
    expect(mutationFailure(retry, new DOMException('aborted', 'AbortError'))).toEqual({
      outcome: 'outcome_unknown', retry,
    })
    expect(mutationFailure(retry, new ApiError(502, body, 'outcome_unknown'))).toEqual({
      outcome: 'outcome_unknown', retry,
    })
    expect(mutationFailure(retry, new ApiError(422, body, 'known_not_applied'))).toEqual({
      outcome: 'known_not_applied', retry: undefined,
    })
    expect(mutationFailure(retry, new LocalMutationValidationError('invalid locally'))).toEqual({
      outcome: 'known_not_applied', retry: undefined,
    })
  })

  it('applies the same outcome rule to manually managed string keys', () => {
    const body = { code: 'TEST', message: 'safe', request_id: '' }
    expect(mutationFailure('manual-key', new ApiError(500, body, 'outcome_unknown')).retry).toBe('manual-key')
    expect(mutationFailure('manual-key', new ApiError(409, body, 'known_not_applied')).retry).toBeUndefined()
  })
})
