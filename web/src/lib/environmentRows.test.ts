import { describe, expect, it, vi } from 'vitest'
import { buildEnvironment, emptyEnvironmentRow, environmentRowsFromDraft } from './environmentRows'

vi.stubGlobal('crypto', { randomUUID: () => '00000000-0000-4000-8000-000000000001' })

describe('environment row projection', () => {
  it('keeps stored secrets without exposing a value and replaces public values directly', () => {
    const rows = environmentRowsFromDraft({
      public_environment: [{ key: 'LOG_LEVEL', value: 'info' }],
      secret_keys: ['TOKEN'],
    })
    rows[0].value = 'debug'
    expect(buildEnvironment(rows)).toEqual({
      public: [{ key: 'LOG_LEVEL', value: 'debug' }],
      secrets: [{ key: 'TOKEN', operation: 'keep' }],
    })
  })

  it('covers replace, delete, rename and public-secret conversion deterministically', () => {
    const rows = environmentRowsFromDraft({
      public_environment: [{ key: 'PUBLIC', value: 'visible' }],
      secret_keys: ['OLD', 'REMOVE'],
    })
    rows[0].sensitive = true
    rows[1].key = 'NEW'; rows[1].value = 'replacement'
    rows[2].removed = true
    expect(buildEnvironment(rows)).toEqual({
      public: [],
      secrets: [
        { key: 'OLD', operation: 'delete' },
        { key: 'REMOVE', operation: 'delete' },
        { key: 'PUBLIC', operation: 'replace', value: 'visible' },
        { key: 'NEW', operation: 'replace', value: 'replacement' },
      ],
    })
  })

  it('rejects duplicate keys and secret-to-public without a new value', () => {
    const duplicate = [emptyEnvironmentRow(), emptyEnvironmentRow()]
    duplicate[0].key = 'A'; duplicate[1].key = 'A'
    expect(() => buildEnvironment(duplicate)).toThrow('duplicate')
    const secret = environmentRowsFromDraft({ public_environment: [], secret_keys: ['TOKEN'] })
    secret[0].sensitive = false
    expect(() => buildEnvironment(secret)).toThrow('replacement')
  })

  it('lets a renamed secret take over a deleted secret key without duplicate operations', () => {
    const rows = environmentRowsFromDraft({ public_environment: [], secret_keys: ['A', 'B'] })
    rows[0].key = 'B'; rows[0].value = 'replacement-for-b'
    rows[1].removed = true
    expect(buildEnvironment(rows)).toEqual({
      public: [],
      secrets: [
        { key: 'A', operation: 'delete' },
        { key: 'B', operation: 'replace', value: 'replacement-for-b' },
      ],
    })
  })

  it('supports public-secret takeover, public takeover and secret rename cycles', () => {
    const takeover = environmentRowsFromDraft({
      public_environment: [{ key: 'PUBLIC', value: 'new-b' }],
      secret_keys: ['B'],
    })
    takeover[0].key = 'B'; takeover[0].sensitive = true
    takeover[1].removed = true
    expect(buildEnvironment(takeover)).toEqual({
      public: [],
      secrets: [{ key: 'B', operation: 'replace', value: 'new-b' }],
    })

    const publicTakeover = environmentRowsFromDraft({
      public_environment: [{ key: 'PUBLIC', value: 'visible-b' }],
      secret_keys: ['B'],
    })
    publicTakeover[0].key = 'B'
    publicTakeover[1].removed = true
    expect(buildEnvironment(publicTakeover)).toEqual({
      public: [{ key: 'B', value: 'visible-b' }],
      secrets: [{ key: 'B', operation: 'delete' }],
    })

    const cycle = environmentRowsFromDraft({ public_environment: [], secret_keys: ['A', 'B'] })
    cycle[0].key = 'B'; cycle[0].value = 'from-a'
    cycle[1].key = 'A'; cycle[1].value = 'from-b'
    expect(buildEnvironment(cycle)).toEqual({
      public: [],
      secrets: [
        { key: 'A', operation: 'replace', value: 'from-b' },
        { key: 'B', operation: 'replace', value: 'from-a' },
      ],
    })
  })
})
